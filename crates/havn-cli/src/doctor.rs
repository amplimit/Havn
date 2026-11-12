//! `havn doctor` — diagnostic checklist.
//!
//! Goal: tell the operator within 30 seconds *what's wrong* without
//! forcing them to grep logs or read the spec. Each check produces one
//! line: `[ok|warn|fail] <area> — <one-liner>`.
//!
//! Failure modes covered (the ones I personally hit while building havn):
//! - `HAVN_AGE_KEY` env var unset but the credentials table is non-empty
//!   → gateway refuses to boot.
//! - `runtime_binary` / `init_binary` paths in config point at stale
//!   targets (built debug-only, then `cargo clean`).
//! - cgroup v2 not mounted, or the agent cgroup parent `havn` cannot be
//!   created / written
//!   → the gateway refuses to start with isolation.
//! - `iptables` / `ip` / `nsenter` missing → agents start without
//!   outbound network.
//! - Stale PID file → `havn gateway start --detach` refuses.
//! - Database schema out of sync (migration didn't run).
//! - Gateway is up but no listen socket (rare, but happens when the
//!   OS denies bind).

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

use crate::api::Gateway;
use crate::paths::{self, CliConfig};

#[derive(Debug, Clone, Copy)]
enum Verdict {
    Ok,
    Warn,
    Fail,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "[ok]  ",
            Self::Warn => "[warn]",
            Self::Fail => "[fail]",
        }
    }
}

fn line(v: Verdict, area: &str, msg: &str) {
    println!("{} {area:<14} {msg}", v.tag());
}

pub async fn run() -> anyhow::Result<()> {
    let cfg = paths::load_config().unwrap_or_default();
    let mut fails = 0usize;
    let mut warns = 0usize;

    macro_rules! mark {
        ($v:expr) => {
            match $v {
                Verdict::Fail => fails += 1,
                Verdict::Warn => warns += 1,
                Verdict::Ok => {}
            }
        };
    }

    println!("havn doctor");
    println!();

    let v = check_config_present();
    mark!(v);
    let v = check_data_dir(&cfg);
    mark!(v);
    let v = check_age_key();
    mark!(v);
    let v = check_binaries();
    mark!(v);
    let v = check_landlock();
    mark!(v);
    let v = check_cgroup_v2();
    mark!(v);
    let v = check_cgroup_parent();
    mark!(v);
    let v = check_network_tools();
    mark!(v);
    let v = check_db(&cfg).await;
    mark!(v);
    let v = check_gateway_reachable(&cfg).await;
    mark!(v);

    println!();
    if fails == 0 && warns == 0 {
        println!("all good ✓");
    } else {
        println!("summary: {fails} fail, {warns} warn");
    }

    if fails > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn check_config_present() -> Verdict {
    match paths::config_path() {
        Some(p) => {
            line(Verdict::Ok, "config", &format!("found at {}", p.display()));
            Verdict::Ok
        }
        None => {
            line(
                Verdict::Warn,
                "config",
                "no config.toml found — running on compiled defaults. Run `havn setup`.",
            );
            Verdict::Warn
        }
    }
}

fn check_data_dir(cfg: &CliConfig) -> Verdict {
    let d = paths::data_dir(cfg);
    if !d.exists() {
        line(
            Verdict::Fail,
            "data dir",
            &format!("{} does not exist — gateway can't start", d.display()),
        );
        return Verdict::Fail;
    }
    // Probe writability with a dotfile.
    let probe = d.join(".havn_doctor_probe");
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            line(
                Verdict::Ok,
                "data dir",
                &format!("writable at {}", d.display()),
            );
            Verdict::Ok
        }
        Err(e) => {
            line(
                Verdict::Fail,
                "data dir",
                &format!("{} not writable: {e}", d.display()),
            );
            Verdict::Fail
        }
    }
}

fn check_age_key() -> Verdict {
    match std::env::var("HAVN_AGE_KEY") {
        Ok(s) if !s.is_empty() => {
            line(Verdict::Ok, "HAVN_AGE_KEY", "set");
            Verdict::Ok
        }
        Ok(_) => {
            line(Verdict::Warn, "HAVN_AGE_KEY", "set but empty");
            Verdict::Warn
        }
        Err(_) => {
            line(
                Verdict::Warn,
                "HAVN_AGE_KEY",
                "not set in this shell — gateway env may still have it (check via /proc on a running gateway). Required when credentials exist.",
            );
            Verdict::Warn
        }
    }
}

fn check_binaries() -> Verdict {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            line(
                Verdict::Fail,
                "binaries",
                &format!("can't locate self: {e}"),
            );
            return Verdict::Fail;
        }
    };
    let dir = exe.parent().unwrap_or(Path::new("."));
    let candidates = [
        ("havn-gateway", dir.join("havn-gateway")),
        ("havn-runtime", dir.join("havn-runtime")),
        ("havn-init", dir.join("havn-init")),
    ];
    let mut worst = Verdict::Ok;
    for (name, p) in candidates {
        if p.exists() {
            line(
                Verdict::Ok,
                "binaries",
                &format!("{name} → {}", p.display()),
            );
        } else {
            line(
                Verdict::Fail,
                "binaries",
                &format!(
                    "{name} missing at {} (rebuild via `cargo build --release`)",
                    p.display()
                ),
            );
            worst = Verdict::Fail;
        }
    }
    worst
}

fn check_landlock() -> Verdict {
    if !cfg!(target_os = "linux") {
        return Verdict::Ok;
    }
    let allowed = std::env::var("HAVN_ALLOW_NO_LANDLOCK").as_deref() == Ok("1");
    match std::fs::read_to_string("/sys/kernel/security/lsm") {
        Ok(lsms) if lsms.split(',').any(|m| m.trim() == "landlock") => {
            line(Verdict::Ok, "landlock", "enabled");
            Verdict::Ok
        }
        Ok(_) if allowed => {
            line(
                Verdict::Warn,
                "landlock",
                "not enabled; HAVN_ALLOW_NO_LANDLOCK=1, so agents run without filesystem \
                 restriction",
            );
            Verdict::Warn
        }
        Ok(_) => {
            line(
                Verdict::Warn,
                "landlock",
                "not enabled in this kernel (needs 5.13+ with landlock in the lsm= list); \
                 the gateway refuses to start unless it runs with HAVN_ALLOW_NO_LANDLOCK=1",
            );
            Verdict::Warn
        }
        Err(e) => {
            line(
                Verdict::Warn,
                "landlock",
                &format!("could not read /sys/kernel/security/lsm: {e}"),
            );
            Verdict::Warn
        }
    }
}

fn check_cgroup_v2() -> Verdict {
    if !cfg!(target_os = "linux") {
        line(
            Verdict::Warn,
            "cgroup v2",
            "not Linux — the gateway refuses to start unless HAVN_SPAWNER=subprocess (no isolation)",
        );
        return Verdict::Warn;
    }
    if Path::new(CGROUP_ROOT).join("cgroup.controllers").exists() {
        line(Verdict::Ok, "cgroup v2", "mounted at /sys/fs/cgroup");
        Verdict::Ok
    } else {
        line(
            Verdict::Fail,
            "cgroup v2",
            "/sys/fs/cgroup/cgroup.controllers missing — boot with `systemd.unified_cgroup_hierarchy=1` or use a host with cgroup v2",
        );
        Verdict::Fail
    }
}

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

const REQUIRED_CONTROLLERS: &[&str] = &["cpu", "memory", "pids"];

/// What the gateway's cgroup setup needs, gathered without creating the
/// agent cgroup parent or touching `cgroup.subtree_control`.
#[derive(Debug)]
struct ParentProbe {
    parent: PathBuf,
    parent_exists: bool,
    /// Write access to the parent, or to the cgroup root while the parent
    /// does not exist yet.
    writable: io::Result<()>,
    /// The cgroup root's `cgroup.controllers`.
    controllers: io::Result<String>,
    /// The cgroup root's `cgroup.subtree_control` (empty when unreadable).
    subtree_control: String,
    /// The cgroup root is not the host's root cgroup (it has
    /// `cgroup.type`, e.g. a container's cgroup namespace) and has member
    /// processes. cgroup v2 then refuses to enable controllers for its
    /// children (EBUSY); the host's root cgroup is exempt from that rule.
    root_busy: bool,
    root_user: bool,
}

fn probe_parent(root: &Path) -> ParentProbe {
    let parent = root.join(havn_core::AGENT_CGROUP_PARENT);
    let parent_exists = parent.is_dir();
    let writable = writable(if parent_exists { &parent } else { root });
    let root_busy = root.join("cgroup.type").exists()
        && std::fs::read_to_string(root.join("cgroup.procs"))
            .is_ok_and(|procs| !procs.trim().is_empty());
    ParentProbe {
        parent,
        parent_exists,
        writable,
        controllers: std::fs::read_to_string(root.join("cgroup.controllers")),
        subtree_control: std::fs::read_to_string(root.join("cgroup.subtree_control"))
            .unwrap_or_default(),
        root_busy,
        // SAFETY: geteuid has no preconditions and cannot fail.
        root_user: unsafe { libc::geteuid() } == 0,
    }
}

fn absent<'a>(list: &str, wanted: &[&'a str]) -> Vec<&'a str> {
    wanted
        .iter()
        .copied()
        .filter(|c| !list.split_whitespace().any(|t| t == *c))
        .collect()
}

fn writable(path: &Path) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

const PARENT_HINT: &str = "run the gateway as root (see contrib/systemd/havn.service), or set \
                          HAVN_SPAWNER=subprocess to run agents WITHOUT isolation (dev only)";

fn parent_verdict(p: &ParentProbe) -> (Verdict, String) {
    let parent = p.parent.display();
    if let Err(e) = &p.writable {
        let denied = matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM));
        if denied && !p.root_user {
            return (
                Verdict::Warn,
                format!(
                    "cannot check {parent} as non-root ({e}); the gateway runs as root, so run \
                     `sudo havn doctor`"
                ),
            );
        }
        let what = if p.parent_exists {
            format!("{parent} is not writable")
        } else {
            format!("cannot create {parent}")
        };
        return (
            Verdict::Warn,
            format!("{what} ({e}) — the gateway refuses to start; {PARENT_HINT}"),
        );
    }
    let raw = match &p.controllers {
        Ok(raw) => raw,
        Err(e) => {
            return (
                Verdict::Warn,
                format!("cannot read cgroup.controllers at the cgroup root: {e}"),
            );
        }
    };
    let missing = absent(raw, REQUIRED_CONTROLLERS);
    if !missing.is_empty() {
        return (
            Verdict::Warn,
            format!(
                "cgroup controllers `{}` are not offered at the cgroup root — the gateway \
                 refuses to start",
                missing.join(" ")
            ),
        );
    }
    let undelegated = absent(&p.subtree_control, REQUIRED_CONTROLLERS);
    if p.root_busy && !undelegated.is_empty() {
        return (
            Verdict::Warn,
            format!(
                "the cgroup root has member processes and does not enable `{}` for its \
                 children, which cgroup v2 then refuses to change (EBUSY) — the gateway \
                 refuses to start; inside a container, move its processes into a leaf cgroup \
                 or give it a delegated cgroup",
                undelegated.join(" ")
            ),
        );
    }
    if !p.root_user {
        return (
            Verdict::Warn,
            format!(
                "{parent} is writable, but not running as root — agent isolation also needs \
                 root for namespaces and NAT; {PARENT_HINT}"
            ),
        );
    }
    let state = if p.parent_exists {
        "is writable"
    } else {
        "can be created"
    };
    (Verdict::Ok, format!("{parent} {state}"))
}

fn check_cgroup_parent() -> Verdict {
    let root = Path::new(CGROUP_ROOT);
    if !cfg!(target_os = "linux") || !root.join("cgroup.controllers").exists() {
        return Verdict::Ok;
    }
    let (v, msg) = parent_verdict(&probe_parent(root));
    line(v, "agent cgroups", &msg);
    v
}

/// Binaries the spawner execs for per-agent networking, with the Debian /
/// Ubuntu package that ships each.
const NETWORK_TOOLS: &[(&str, &str)] = &[
    ("iptables", "iptables"),
    ("ip", "iproute2"),
    ("nsenter", "util-linux"),
];

/// Same lookup as `havn_spawner::network::resolve_tool` (absolute `PATH`
/// entries, as root only root-owned ones that are not group/other-writable,
/// then these), so doctor and the gateway agree; the CLI does not link the
/// spawner.
const SBIN_DIRS: &[&str] = &["/usr/local/sbin", "/usr/sbin", "/sbin"];

fn find_tool(name: &str, path_var: Option<&OsString>, root: bool) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| d.is_absolute() && (!root || root_owned_dir(d)))
                .collect()
        })
        .unwrap_or_default();
    dirs.extend(SBIN_DIRS.iter().map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(name)).find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

fn root_owned_dir(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    dir.metadata()
        .is_ok_and(|m| m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0)
}

fn network_tools_verdict(missing: &[(&str, &str)]) -> (Verdict, String) {
    if missing.is_empty() {
        let bins: Vec<&str> = NETWORK_TOOLS.iter().map(|(bin, _)| *bin).collect();
        return (Verdict::Ok, format!("{} found", bins.join(", ")));
    }
    let bins: Vec<&str> = missing.iter().map(|(bin, _)| *bin).collect();
    let packages: Vec<&str> = missing.iter().map(|(_, pkg)| *pkg).collect();
    // The gateway checks host routes with `ip` before it starts; the other
    // two only gate agent egress.
    let effect = if bins.contains(&"ip") {
        "the gateway refuses to start"
    } else {
        "agents start without outbound network"
    };
    (
        Verdict::Warn,
        format!(
            "{} not found — {effect}; install with `apt-get install {}` (iptables with the \
             nft or legacy backend) and restart the gateway",
            bins.join(", "),
            packages.join(" ")
        ),
    )
}

fn check_network_tools() -> Verdict {
    if !cfg!(target_os = "linux") {
        return Verdict::Ok;
    }
    let path_var = std::env::var_os("PATH");
    // SAFETY: geteuid has no preconditions and cannot fail.
    let root = unsafe { libc::geteuid() } == 0;
    let missing: Vec<(&str, &str)> = NETWORK_TOOLS
        .iter()
        .copied()
        .filter(|(bin, _)| find_tool(bin, path_var.as_ref(), root).is_none())
        .collect();
    let (v, msg) = network_tools_verdict(&missing);
    line(v, "network tools", &msg);
    v
}

async fn check_db(cfg: &CliConfig) -> Verdict {
    let db_path = match cfg.db_path.clone() {
        Some(p) => p,
        None => paths::data_dir(cfg).join("havn.db"),
    };
    if !db_path.exists() {
        line(
            Verdict::Warn,
            "database",
            &format!(
                "{} not yet created — first gateway start will create it",
                db_path.display()
            ),
        );
        return Verdict::Warn;
    }
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .read_only(true);
    let pool = match SqlitePool::connect_with(opts).await {
        Ok(p) => p,
        Err(e) => {
            line(Verdict::Fail, "database", &format!("connect failed: {e}"));
            return Verdict::Fail;
        }
    };
    let r: Result<i64, _> = sqlx::query_scalar(
        r"SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name IN ('users','agents','credentials')",
    )
    .fetch_one(&pool)
    .await;
    pool.close().await;
    match r {
        Ok(n) if n == 3 => {
            line(
                Verdict::Ok,
                "database",
                &format!("schema present at {}", db_path.display()),
            );
            Verdict::Ok
        }
        Ok(n) => {
            line(
                Verdict::Fail,
                "database",
                &format!("expected 3 core tables, found {n}; migrations didn't run cleanly"),
            );
            Verdict::Fail
        }
        Err(e) => {
            line(
                Verdict::Fail,
                "database",
                &format!("schema probe failed: {e}"),
            );
            Verdict::Fail
        }
    }
}

async fn check_gateway_reachable(cfg: &CliConfig) -> Verdict {
    let pid_path = paths::pid_file(cfg);
    let pid_alive = paths::read_pid(&pid_path)
        .ok()
        .flatten()
        .is_some_and(paths::pid_alive);

    let gw = match Gateway::from_env() {
        Ok(g) => g,
        Err(e) => {
            line(Verdict::Fail, "gateway api", &format!("client init: {e}"));
            return Verdict::Fail;
        }
    };
    match gw.get_json::<serde_json::Value>("/me").await {
        Ok(_) => {
            line(Verdict::Ok, "gateway api", &format!("{} responds", gw.base));
            Verdict::Ok
        }
        Err(_) if !pid_alive => {
            line(
                Verdict::Warn,
                "gateway api",
                "not running (no pid file or pid stale) — start it with `havn gateway start --detach`",
            );
            Verdict::Warn
        }
        Err(e) => {
            line(
                Verdict::Fail,
                "gateway api",
                &format!("pid alive but API unreachable at {}: {e}", gw.base),
            );
            Verdict::Fail
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn probe(parent_exists: bool, writable: io::Result<()>, root_user: bool) -> ParentProbe {
        ParentProbe {
            parent: PathBuf::from("/sys/fs/cgroup/havn"),
            parent_exists,
            writable,
            controllers: Ok("cpuset cpu io memory hugetlb pids rdma misc\n".into()),
            subtree_control: "cpu memory pids\n".into(),
            root_busy: false,
            root_user,
        }
    }

    fn denied() -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EACCES))
    }

    #[test]
    fn uncreatable_parent_as_root_warns_with_fix_hint() {
        let (v, msg) = parent_verdict(&probe(false, denied(), true));
        assert!(matches!(v, Verdict::Warn));
        assert!(msg.contains("cannot create /sys/fs/cgroup/havn "), "{msg}");
        assert!(msg.contains("run the gateway as root"), "{msg}");
    }

    #[test]
    fn denied_as_non_root_asks_for_sudo() {
        let (v, msg) = parent_verdict(&probe(false, denied(), false));
        assert!(matches!(v, Verdict::Warn));
        assert!(msg.contains("sudo havn doctor"), "{msg}");
        assert!(!msg.contains("refuses to start"), "{msg}");
    }

    #[test]
    fn busy_namespace_root_without_controllers_warns() {
        let mut p = probe(true, Ok(()), true);
        p.subtree_control = "cpu pids\n".into();
        p.root_busy = true;
        let (v, msg) = parent_verdict(&p);
        assert!(matches!(v, Verdict::Warn));
        assert!(msg.contains("`memory`") && msg.contains("EBUSY"), "{msg}");

        p.root_busy = false;
        let (v, _) = parent_verdict(&p);
        assert!(matches!(v, Verdict::Ok));
    }

    #[test]
    fn unwritable_existing_parent_warns() {
        let (v, msg) = parent_verdict(&probe(true, denied(), true));
        assert!(matches!(v, Verdict::Warn));
        assert!(msg.contains("is not writable"), "{msg}");
    }

    #[test]
    fn read_only_cgroupfs_warns_even_as_root() {
        let ro = Err(io::Error::from_raw_os_error(libc::EROFS));
        let (v, _) = parent_verdict(&probe(false, ro, true));
        assert!(matches!(v, Verdict::Warn));
    }

    #[test]
    fn missing_controller_warns() {
        let mut p = probe(true, Ok(()), true);
        p.controllers = Ok("cpu io pids\n".into());
        let (v, msg) = parent_verdict(&p);
        assert!(matches!(v, Verdict::Warn));
        assert!(msg.contains("`memory`"), "{msg}");
    }

    #[test]
    fn writable_parent_without_root_warns() {
        let (v, msg) = parent_verdict(&probe(true, Ok(()), false));
        assert!(matches!(v, Verdict::Warn));
        assert!(msg.contains("not running as root"), "{msg}");
    }

    #[test]
    fn creatable_parent_as_root_is_ok() {
        let (v, msg) = parent_verdict(&probe(false, Ok(()), true));
        assert!(matches!(v, Verdict::Ok));
        assert!(msg.contains("can be created"), "{msg}");
    }

    #[test]
    fn missing_network_tools_warn_with_packages() {
        let (v, msg) = network_tools_verdict(&[("iptables", "iptables"), ("ip", "iproute2")]);
        assert!(matches!(v, Verdict::Warn));
        assert!(msg.starts_with("iptables, ip not found"), "{msg}");
        assert!(msg.contains("refuses to start"), "{msg}");
        assert!(msg.contains("apt-get install iptables iproute2"), "{msg}");
        let (_, msg) = network_tools_verdict(&[("iptables", "iptables")]);
        assert!(msg.contains("without outbound network"), "{msg}");
    }

    #[test]
    fn present_network_tools_are_ok() {
        let (v, _) = network_tools_verdict(&[]);
        assert!(matches!(v, Verdict::Ok));
    }

    #[test]
    fn find_tool_needs_an_executable_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("havn-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("havn-doctor-tool");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = Some(dir.clone().into_os_string());
        assert_eq!(find_tool("havn-doctor-tool", path.as_ref(), false), None);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_tool("havn-doctor-tool", path.as_ref(), false),
            Some(tool)
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(find_tool("havn-doctor-tool", path.as_ref(), true), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
