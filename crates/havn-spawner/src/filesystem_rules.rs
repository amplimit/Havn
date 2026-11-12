//! The agent's Landlock filesystem rules, applied by `havn-init` right
//! before it execs the runtime.
//!
//! Landlock confines the calling thread and everything it later creates.
//! Applying the ruleset before exec therefore covers every thread of the
//! runtime and every process its tools start; applying it from inside the
//! already multi-threaded runtime would leave the existing worker threads
//! (which run the tools) unconfined.

use std::path::{Path, PathBuf};

use havn_core::Policy;
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus,
};

use crate::{ExtraMount, MountMode, TmpfsMount};

/// Read and execute: system directories the shell tool and its programs
/// need, the agent's own `/proc` (it has a private PID namespace) and
/// harmless character devices.
const SYSTEM_READ_PATHS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/sbin",
    "/etc",
    "/proc",
    "/dev/urandom",
    "/dev/random",
    "/dev/zero",
];

/// Read and write: scratch space and `/dev/null`.
const SYSTEM_WRITE_PATHS: &[&str] = &["/tmp", "/dev/null"];

/// Environment variables carrying the lists from the spawner to
/// `havn-init`, and telling the runtime the rules are already in place.
pub const RW_ENV: &str = "HAVN_LANDLOCK_RW";
pub const RO_ENV: &str = "HAVN_LANDLOCK_RO";
pub const APPLIED_ENV: &str = "HAVN_LANDLOCK_APPLIED";

/// Paths the agent may write and paths it may only read and execute.
#[must_use]
pub fn allowlist(
    workspace: &Path,
    extra_mounts: &[ExtraMount],
    tmpfs_mounts: &[TmpfsMount],
    policy: &Policy,
    runtime_binary: &Path,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut rw: Vec<PathBuf> = vec![workspace.to_path_buf()];
    rw.extend(SYSTEM_WRITE_PATHS.iter().map(PathBuf::from));
    rw.extend(tmpfs_mounts.iter().map(|m| m.path.clone()));
    let mut ro: Vec<PathBuf> = SYSTEM_READ_PATHS.iter().map(PathBuf::from).collect();
    for mount in extra_mounts {
        match mount.mode {
            MountMode::Rw => rw.push(mount.path.clone()),
            MountMode::Ro => ro.push(mount.path.clone()),
        }
    }
    if policy.permissions.can_use_mcp {
        for server in policy.mcp_servers.values().filter(|s| s.enabled) {
            rw.extend(server.extra_paths_rw.iter().cloned());
            ro.extend(server.extra_paths_ro.iter().cloned());
        }
    }
    // The runtime binary is bind-mounted at its host directory when that
    // is outside /usr (development builds); it must stay executable.
    if let Some(dir) = runtime_binary.parent() {
        ro.push(dir.to_path_buf());
    }
    (rw, ro)
}

/// Newline-separated, the transport format for [`RW_ENV`] / [`RO_ENV`].
/// Paths that are not UTF-8 or contain a newline are dropped (they cannot
/// be represented); a dropped path is denied, never widened.
#[must_use]
pub fn encode(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .filter_map(|p| p.to_str())
        .filter(|p| !p.contains('\n'))
        .collect::<Vec<_>>()
        .join("\n")
}

#[must_use]
pub fn decode(value: &str) -> Vec<PathBuf> {
    value
        .split('\n')
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// How completely the kernel enforced the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Full,
    Partial,
    None,
}

/// Confine the calling thread (and everything it creates afterwards) to
/// `rw` and `ro`. Paths that do not exist are skipped.
pub fn apply(rw: &[PathBuf], ro: &[PathBuf]) -> Result<Enforcement, String> {
    let abi = ABI::V3;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| format!("handle_access: {e}"))?
        .create()
        .map_err(|e| format!("create: {e}"))?;
    let rules = rw
        .iter()
        .map(|p| (p, AccessFs::from_all(abi)))
        .chain(ro.iter().map(|p| (p, AccessFs::from_read(abi))));
    for (path, access) in rules {
        let Ok(fd) = PathFd::new(path) else {
            continue;
        };
        // Rules on a non-directory may only carry file rights.
        let access = if path.is_dir() {
            access
        } else {
            access & AccessFs::from_file(abi)
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|e| format!("add_rule({}): {e}", path.display()))?;
    }
    let status = ruleset
        .restrict_self()
        .map_err(|e| format!("restrict_self: {e}"))?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => Enforcement::Full,
        RulesetStatus::PartiallyEnforced => Enforcement::Partial,
        RulesetStatus::NotEnforced => Enforcement::None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn encode_round_trips_and_drops_unrepresentable_paths() {
        let paths = vec![
            PathBuf::from("/a b"),
            PathBuf::from("/c\nd"),
            PathBuf::from("/e"),
        ];
        assert_eq!(
            decode(&encode(&paths)),
            vec![PathBuf::from("/a b"), PathBuf::from("/e")]
        );
        assert!(decode("").is_empty());
    }

    #[test]
    fn allowlist_includes_workspace_mounts_and_mcp_paths() {
        let mut policy = Policy::default();
        policy.permissions.can_use_mcp = true;
        policy.mcp_servers.insert(
            "fs".into(),
            havn_core::McpServerConfig {
                enabled: true,
                extra_paths_rw: vec![PathBuf::from("/srv/data")],
                extra_paths_ro: vec![PathBuf::from("/srv/docs")],
                ..Default::default()
            },
        );
        let (rw, ro) = allowlist(
            Path::new("/ws"),
            &[ExtraMount {
                path: PathBuf::from("/opt/havn/pack"),
                mode: MountMode::Ro,
            }],
            &[TmpfsMount {
                path: PathBuf::from("/dev/shm"),
                size_mb: 64,
            }],
            &policy,
            Path::new("/work/target/release/havn-runtime"),
        );
        for p in ["/ws", "/tmp", "/dev/null", "/dev/shm", "/srv/data"] {
            assert!(rw.contains(&PathBuf::from(p)), "rw {p}");
        }
        for p in [
            "/usr",
            "/proc",
            "/opt/havn/pack",
            "/srv/docs",
            "/work/target/release",
        ] {
            assert!(ro.contains(&PathBuf::from(p)), "ro {p}");
        }
        assert!(!rw.contains(&PathBuf::from("/usr")));
    }

    /// Applies the rules in a separate copy of this test binary and checks
    /// that a thread started afterwards is confined too.
    #[test]
    fn rules_cover_threads_started_later() {
        const PROBE: &str = "HAVN_LANDLOCK_PROBE_DIR";
        if let Some(dir) = std::env::var_os(PROBE) {
            let dir = PathBuf::from(dir);
            let allowed = dir.join("allowed");
            let denied = dir.join("denied");
            let enforcement = apply(std::slice::from_ref(&allowed), &[]).expect("apply");
            if enforcement == Enforcement::None {
                std::process::exit(3);
            }
            let outcome = std::thread::spawn(move || {
                let inside = std::fs::write(allowed.join("ok"), "x").is_ok();
                let outside = std::fs::write(denied.join("no"), "x").is_ok();
                (inside, outside)
            })
            .join()
            .expect("thread");
            std::process::exit(match outcome {
                (true, false) => 0,
                _ => 1,
            });
        }
        let dir = std::env::temp_dir().join(format!("havn-landlock-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("allowed")).expect("allowed");
        std::fs::create_dir_all(dir.join("denied")).expect("denied");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "filesystem_rules::tests::rules_cover_threads_started_later",
            ])
            .env(PROBE, &dir)
            .status()
            .expect("run probe");
        std::fs::remove_dir_all(&dir).ok();
        match status.code() {
            Some(3) => eprintln!("Landlock not enforced on this kernel; skipping"),
            code => assert_eq!(code, Some(0), "{status:?}"),
        }
    }
}
