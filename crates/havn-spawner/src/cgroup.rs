//! cgroup v2 controller for per-agent resource limits (spec §4.1).
//!
//! Layout:
//!
//! ```text
//! /sys/fs/cgroup/                          ← unified cgroup v2 mount
//!   havn/                                  ← parent of all agents (created on init)
//!     agent-<uuid>/                        ← per-agent leaf
//!       memory.max
//!       cpu.max
//!       pids.max
//!       cgroup.procs   ← the agent joins by writing 0 here before exec
//!       cgroup.kill    ← writing "1" SIGKILLs every PID in this cgroup
//! ```
//!
//! This module covers only the cgroup layer; namespaces live in
//! `namespace` / `ns_setup`, Landlock in `filesystem_rules` (applied by
//! `havn-init`), seccomp in the runtime. The
//! controller takes the cgroup root as a parameter so unit tests can
//! substitute a tempdir; production binds
//! `/sys/fs/cgroup` via [`CgroupController::system`].
//!
//! Gateways before #66 used `havn.slice` as the parent, which is also the
//! cgroup of a systemd slice unit of that name. [`RETIRED_PARENT`] is kept
//! only so start-up can remove what such a gateway left behind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use havn_core::{AGENT_CGROUP_PARENT, AGENT_CGROUP_PREFIX, ResourceLimits};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Parent cgroup of agents under gateways before #66.
pub const RETIRED_PARENT: &str = "havn.slice";

const REQUIRED_CONTROLLERS: &[&str] = &["cpu", "memory", "pids"];

/// cgroup v2 cpu.max period — the standard 100 ms scheduler window. Quota is
/// derived per-agent from `cpu_cores * PERIOD_US`.
const PERIOD_US: u64 = 100_000;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CgroupError {
    #[error("cgroup v2 root {0} does not exist or is not a directory")]
    RootMissing(PathBuf),
    #[error("cgroup root {root} is not cgroup v2 (missing cgroup.controllers)")]
    NotV2 { root: PathBuf },
    #[error("cannot create agent cgroup parent {0}")]
    ParentCreate(PathBuf),
    #[error("cannot write {file}: {source}")]
    WriteFailed {
        file: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot read {file}: {source}")]
    ReadFailed {
        file: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot remove cgroup {0}")]
    Cleanup(PathBuf),
    #[error(
        "cgroup controllers `{controllers}` are not offered by {file} (kernel built \
         without them, or the parent cgroup does not delegate them)"
    )]
    ControllerUnavailable { file: PathBuf, controllers: String },
    #[error("cannot enable cgroup controllers `{controllers}` in {file}: {source} ({hint})")]
    ControllerDelegation {
        file: PathBuf,
        controllers: String,
        hint: &'static str,
        #[source]
        source: io::Error,
    },
}

/// Stats snapshot for a single agent cgroup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentResourceStats {
    pub memory_bytes: u64,
    pub cpu_microseconds: u64,
    pub pids: u32,
}

#[derive(Debug, Clone)]
pub struct CgroupController {
    /// cgroup v2 mount root, typically `/sys/fs/cgroup`.
    root: PathBuf,
    /// Parent of the agent cgroups: `<root>/havn`.
    parent: PathBuf,
}

/// The state systemd reports for a `havn.slice` unit. While the unit is
/// active systemd owns `/havn.slice`, and the gateway must leave it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceUnit {
    Inactive,
    Active,
    Unknown,
}

impl SliceUnit {
    /// Map systemd's `ActiveState` property.
    pub fn from_active_state(state: &str) -> Self {
        match state.trim() {
            "inactive" | "failed" => Self::Inactive,
            "active" | "activating" | "deactivating" | "reloading" | "refreshing" => Self::Active,
            _ => Self::Unknown,
        }
    }
}

impl CgroupController {
    /// Construct a controller rooted at the production `/sys/fs/cgroup` mount.
    /// Returns an error if the path doesn't look like cgroup v2 — caller should
    /// fall back to a non-cgroup spawner with a clear log.
    pub fn system() -> Result<Self, CgroupError> {
        Self::with_root(PathBuf::from("/sys/fs/cgroup"))
    }

    /// Construct a controller rooted at an arbitrary path. Used by tests with
    /// a tempdir, and by deployments where cgroup v2 is mounted elsewhere.
    pub fn with_root(root: PathBuf) -> Result<Self, CgroupError> {
        if !root.is_dir() {
            return Err(CgroupError::RootMissing(root));
        }
        // cgroup v2 advertises itself via cgroup.controllers at the root. v1
        // hierarchies have per-controller subdirectories instead.
        if !root.join("cgroup.controllers").exists() {
            return Err(CgroupError::NotV2 { root });
        }
        let parent = root.join(AGENT_CGROUP_PARENT);
        if !parent.exists()
            && let Err(e) = fs::create_dir(&parent)
        {
            warn!(
                parent = %parent.display(),
                error = %e,
                "cannot create the agent cgroup parent — run the gateway as root (contrib/systemd/havn.service)"
            );
            return Err(CgroupError::ParentCreate(parent));
        }
        delegate_controllers(&root, &parent)?;
        Ok(Self { root, parent })
    }

    /// Parent cgroup of every agent cgroup.
    pub fn parent(&self) -> &Path {
        &self.parent
    }

    /// Returns the cgroup root path supplied at construction time.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the per-agent cgroup directory and write its resource limits.
    /// Returns the cgroup path; the spawned child joins it itself before exec.
    pub fn create_agent(
        &self,
        agent_id: &str,
        limits: &ResourceLimits,
    ) -> Result<PathBuf, CgroupError> {
        let path = self.agent_path(agent_id);
        if !path.exists() {
            fs::create_dir(&path).map_err(|e| CgroupError::WriteFailed {
                file: path.clone(),
                source: e,
            })?;
        }
        write_file(
            &path.join("memory.max"),
            &format_memory_max(limits.memory_mb),
        )?;
        write_file(&path.join("cpu.max"), &format_cpu_max(limits.cpu_cores))?;
        write_file(&path.join("pids.max"), &format_pids_max(limits.pids_max))?;
        debug!(agent_id, path = %path.display(), "agent cgroup created");
        Ok(path)
    }

    pub fn agent_path(&self, agent_id: &str) -> PathBuf {
        self.parent.join(format!("{AGENT_CGROUP_PREFIX}{agent_id}"))
    }

    /// Every per-agent cgroup currently under the parent.
    pub fn agent_cgroups(&self) -> Vec<PathBuf> {
        agent_dirs(&self.parent)
    }

    /// `<root>/havn.slice`, if a gateway before #66 left it.
    pub fn retired_parent(&self) -> Option<PathBuf> {
        Some(self.root.join(RETIRED_PARENT)).filter(|p| p.is_dir())
    }

    /// Agent cgroups a gateway before #66 left under `<root>/havn.slice`.
    pub fn retired_agent_cgroups(&self) -> Vec<PathBuf> {
        agent_dirs(&self.root.join(RETIRED_PARENT))
    }

    /// Remove `<root>/havn.slice` once its agent cgroups are gone. It is
    /// kept while systemd has, or may have, an active unit for it, and while
    /// anything else lives in it. Returns whether it was removed.
    pub fn remove_retired_parent(&self, unit: SliceUnit) -> bool {
        let Some(parent) = self.retired_parent() else {
            return false;
        };
        match unit {
            SliceUnit::Inactive => {}
            SliceUnit::Active => {
                info!(
                    parent = %parent.display(),
                    "keeping the agent cgroup parent of older gateways: systemd manages it as havn.slice"
                );
                return false;
            }
            SliceUnit::Unknown => {
                warn!(
                    parent = %parent.display(),
                    "keeping the agent cgroup parent of older gateways: cannot tell whether \
                     systemd manages it as havn.slice; remove it by hand once it is empty"
                );
                return false;
            }
        }
        let children = fs::read_dir(&parent).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .count()
        });
        if children > 0 {
            warn!(
                parent = %parent.display(),
                children,
                "keeping the agent cgroup parent of older gateways: it still holds other cgroups"
            );
            return false;
        }
        match self.cleanup(&parent) {
            Ok(()) => {
                info!(parent = %parent.display(), "removed the agent cgroup parent of older gateways");
                true
            }
            Err(e) => {
                warn!(
                    parent = %parent.display(),
                    error = %e,
                    "cannot remove the agent cgroup parent of older gateways"
                );
                false
            }
        }
    }

    /// True while any process is left in the cgroup or below it. A cgroup
    /// without `cgroup.events` (already removed) counts as empty.
    pub fn is_populated(&self, cgroup: &Path) -> bool {
        fs::read_to_string(cgroup.join("cgroup.events"))
            .is_ok_and(|events| events.lines().any(|l| l.trim() == "populated 1"))
    }

    /// Read current resource usage. Missing files yield zeroes — this happens
    /// briefly during cgroup teardown when the agent has just been removed.
    pub fn stats(&self, cgroup: &Path) -> Result<AgentResourceStats, CgroupError> {
        let memory_bytes = read_u64(&cgroup.join("memory.current")).unwrap_or(0);
        let pids = read_u64(&cgroup.join("pids.current")).unwrap_or(0);
        let cpu_microseconds = parse_cpu_stat(&cgroup.join("cpu.stat")).unwrap_or(0);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "pids.current realistically << u32::MAX"
        )]
        Ok(AgentResourceStats {
            memory_bytes,
            cpu_microseconds,
            pids: pids as u32,
        })
    }

    /// Atomically `SIGKILL` every PID inside the cgroup (and its children).
    /// Standard cgroup v2 facility — preferred over iterating PIDs because it
    /// handles fork-bombs without races.
    pub fn kill_all(&self, cgroup: &Path) -> Result<(), CgroupError> {
        write_file(&cgroup.join("cgroup.kill"), "1")
    }

    /// Remove an empty cgroup. Caller must ensure no PIDs remain (typically
    /// after [`Self::kill_all`] + waitpid). On non-empty cgroups the kernel
    /// returns `EBUSY`, which is surfaced as [`CgroupError::Cleanup`].
    ///
    /// Pre-unlinks the control files we wrote in [`Self::create_agent`].
    /// On real cgroupfs these are kernel-virtual and the unlink calls are
    /// silently ignored (the kernel removes them when `rmdir` succeeds);
    /// on the tempdir-backed test fixture, where everything is a real file,
    /// these unlinks make the subsequent `rmdir` actually empty.
    pub fn cleanup(&self, cgroup: &Path) -> Result<(), CgroupError> {
        for f in CONTROL_FILES {
            let _ = fs::remove_file(cgroup.join(f));
        }
        fs::remove_dir(cgroup).map_err(|_| CgroupError::Cleanup(cgroup.to_path_buf()))
    }
}

/// Files we touch in [`CgroupController::create_agent`] / the spawner's pre-exec join /
/// [`CgroupController::kill_all`] / controller delegation. Listed here so
/// [`CgroupController::cleanup`] can pre-unlink them in test environments
/// where they're real on-disk files.
const CONTROL_FILES: &[&str] = &[
    "memory.max",
    "cpu.max",
    "pids.max",
    "cgroup.procs",
    "cgroup.kill",
    "cgroup.events",
    "cgroup.subtree_control",
];

fn agent_dirs(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(AGENT_CGROUP_PREFIX)
        })
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn format_memory_max(memory_mb: u64) -> String {
    if memory_mb == 0 {
        "max".to_string()
    } else {
        (memory_mb * 1024 * 1024).to_string()
    }
}

fn format_cpu_max(cpu_cores: f64) -> String {
    if cpu_cores <= 0.0 {
        return format!("max {PERIOD_US}");
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "cpu_cores is bounded to small positive values; quota is microseconds, far below 2^52"
    )]
    let quota = (cpu_cores * PERIOD_US as f64).round() as u64;
    format!("{quota} {PERIOD_US}")
}

fn format_pids_max(pids_max: u32) -> String {
    if pids_max == 0 {
        "max".to_string()
    } else {
        pids_max.to_string()
    }
}

fn delegate_controllers(root: &Path, parent: &Path) -> Result<(), CgroupError> {
    let root_available = root.join("cgroup.controllers");
    let unavailable = absent_from(&root_available, REQUIRED_CONTROLLERS)?;
    if !unavailable.is_empty() {
        return Err(CgroupError::ControllerUnavailable {
            file: root_available,
            controllers: unavailable.join(" "),
        });
    }
    let root_control = root.join("cgroup.subtree_control");
    let missing = absent_from(&root_control, REQUIRED_CONTROLLERS)?;
    if !missing.is_empty() {
        enable_controllers(&root_control, &missing)?;
        info!(
            root = %root.display(),
            controllers = %missing.join(" "),
            "enabled cgroup controllers on the cgroup root for the agent cgroups; this \
             applies to every top-level cgroup on the host"
        );
    }
    enable_controllers(&parent.join("cgroup.subtree_control"), REQUIRED_CONTROLLERS)
}

fn absent_from<'a>(list: &Path, controllers: &[&'a str]) -> Result<Vec<&'a str>, CgroupError> {
    let raw = fs::read_to_string(list).map_err(|e| CgroupError::ReadFailed {
        file: list.to_path_buf(),
        source: e,
    })?;
    Ok(controllers
        .iter()
        .copied()
        .filter(|wanted| !raw.split_whitespace().any(|token| token == *wanted))
        .collect())
}

fn enable_controllers(control: &Path, controllers: &[&str]) -> Result<(), CgroupError> {
    let request = controllers
        .iter()
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>()
        .join(" ");
    fs::write(control, &request).map_err(|e| CgroupError::ControllerDelegation {
        file: control.to_path_buf(),
        controllers: request,
        hint: delegation_hint(&e),
        source: e,
    })
}

fn delegation_hint(err: &io::Error) -> &'static str {
    match err.raw_os_error() {
        Some(libc::EBUSY) => {
            "this cgroup has member processes, which cgroup v2 forbids once it delegates \
             controllers; move them into a leaf cgroup (inside a container, its own \
             processes usually sit in the container's root cgroup)"
        }
        Some(libc::EACCES | libc::EPERM) => {
            "the gateway cannot write this cgroupfs file; run it as root or in a cgroup \
             delegated to its user"
        }
        Some(libc::ENOENT) => "the parent cgroup does not offer one of these controllers",
        _ => "unexpected cgroupfs error",
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), CgroupError> {
    fs::write(path, contents).map_err(|e| CgroupError::WriteFailed {
        file: path.to_path_buf(),
        source: e,
    })
}

fn read_u64(path: &Path) -> Result<u64, CgroupError> {
    let raw = fs::read_to_string(path).map_err(|e| CgroupError::ReadFailed {
        file: path.to_path_buf(),
        source: e,
    })?;
    Ok(raw.trim().parse().unwrap_or(0))
}

/// `cpu.stat` is a multi-line key-value file. We extract the `usage_usec`
/// row for total CPU time used by the cgroup.
fn parse_cpu_stat(path: &Path) -> Result<u64, CgroupError> {
    let raw = fs::read_to_string(path).map_err(|e| CgroupError::ReadFailed {
        file: path.to_path_buf(),
        source: e,
    })?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("usage_usec ") {
            return Ok(rest.trim().parse().unwrap_or(0));
        }
    }
    Ok(0)
}

#[cfg(test)]
pub(crate) const ALL_CONTROLLERS: &str = "cpu io memory pids";

#[cfg(test)]
pub(crate) fn fake_root(available: &str, enabled: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("havn-cg-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("cgroup.controllers"), available).expect("write cgroup.controllers");
    fs::write(root.join("cgroup.subtree_control"), enabled).expect("write subtree_control");
    root
}

/// `<root>/havn.slice` as a gateway before #66 left it, with one agent
/// cgroup per id.
#[cfg(test)]
pub(crate) fn retired_parent_with(root: &Path, agents: &[&str]) -> Vec<PathBuf> {
    let parent = root.join(RETIRED_PARENT);
    fs::create_dir_all(&parent).expect("mkdir havn.slice");
    fs::write(parent.join("cgroup.subtree_control"), "+cpu +memory +pids")
        .expect("write subtree_control");
    agents
        .iter()
        .map(|id| {
            let agent = parent.join(format!("{AGENT_CGROUP_PREFIX}{id}"));
            fs::create_dir(&agent).expect("mkdir agent");
            fs::write(agent.join("memory.max"), "536870912").expect("write memory.max");
            agent
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    /// Build a minimal "cgroup v2 root" in a tempdir: contains
    /// `cgroup.controllers` so [`CgroupController::with_root`] is happy.
    fn fake_v2_root() -> PathBuf {
        fake_root(ALL_CONTROLLERS, ALL_CONTROLLERS)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read")
    }

    #[test]
    fn with_root_enables_required_controllers_on_parent() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        assert_eq!(
            read(&ctl.parent().join("cgroup.subtree_control")),
            "+cpu +memory +pids"
        );
        assert_eq!(read(&root.join("cgroup.subtree_control")), ALL_CONTROLLERS);
    }

    #[test]
    fn with_root_enables_missing_controllers_on_root_then_parent() {
        let root = fake_root(ALL_CONTROLLERS, "cpu io");
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        assert_eq!(read(&root.join("cgroup.subtree_control")), "+memory +pids");
        assert_eq!(
            read(&ctl.parent().join("cgroup.subtree_control")),
            "+cpu +memory +pids"
        );
    }

    #[test]
    fn with_root_writes_root_before_parent() {
        let root = fake_root(ALL_CONTROLLERS, "cpu io");
        let parent_control = root
            .join(AGENT_CGROUP_PARENT)
            .join("cgroup.subtree_control");
        std::fs::create_dir_all(&parent_control).expect("mkdir");
        let err = CgroupController::with_root(root.clone()).expect_err("parent write must fail");
        let failed = match err {
            CgroupError::ControllerDelegation { file, .. } => file,
            other => panic!("expected ControllerDelegation, got {other:?}"),
        };
        assert_eq!(failed, parent_control);
        assert_eq!(read(&root.join("cgroup.subtree_control")), "+memory +pids");
    }

    #[test]
    fn with_root_is_idempotent_for_existing_parent() {
        let root = fake_v2_root();
        let first = CgroupController::with_root(root.clone()).expect("first");
        first
            .create_agent("live", &ResourceLimits::default())
            .expect("agent");
        let second = CgroupController::with_root(root).expect("second");
        assert_eq!(first.parent(), second.parent());
        assert_eq!(
            read(&second.parent().join("cgroup.subtree_control")),
            "+cpu +memory +pids"
        );
        assert!(second.parent().join("agent-live").is_dir());
    }

    #[test]
    fn with_root_rejects_controller_missing_from_root() {
        let root = fake_root("cpu io pids", "cpu io pids");
        let err = CgroupController::with_root(root.clone()).expect_err("must fail");
        let unavailable = match err {
            CgroupError::ControllerUnavailable { controllers, .. } => controllers,
            other => panic!("expected ControllerUnavailable, got {other:?}"),
        };
        assert_eq!(unavailable, "memory");
        assert_eq!(read(&root.join("cgroup.subtree_control")), "cpu io pids");
    }

    #[test]
    fn with_root_rejects_parent_without_writable_subtree_control() {
        let root = fake_v2_root();
        let control = root
            .join(AGENT_CGROUP_PARENT)
            .join("cgroup.subtree_control");
        std::fs::create_dir_all(&control).expect("mkdir");
        let err = CgroupController::with_root(root).expect_err("must fail");
        assert!(
            matches!(err, CgroupError::ControllerDelegation { ref file, .. } if *file == control),
            "{err:?}"
        );
    }

    #[test]
    fn with_root_rejects_root_without_subtree_control() {
        let root = fake_v2_root();
        std::fs::remove_file(root.join("cgroup.subtree_control")).expect("rm");
        let err = CgroupController::with_root(root).expect_err("must fail");
        assert!(matches!(err, CgroupError::ReadFailed { .. }), "{err:?}");
    }

    #[test]
    fn delegation_hint_names_no_internal_process_rule_for_ebusy() {
        let hint = delegation_hint(&io::Error::from_raw_os_error(libc::EBUSY));
        assert!(hint.contains("member processes"), "{hint}");
    }

    #[test]
    fn rejects_path_that_is_not_a_directory() {
        let p = std::env::temp_dir().join(format!("havn-bad-{}", uuid::Uuid::now_v7()));
        let err = CgroupController::with_root(p.clone()).expect_err("must fail");
        assert!(matches!(err, CgroupError::RootMissing(_)));
    }

    #[test]
    fn rejects_root_without_cgroup_controllers() {
        let mut p = std::env::temp_dir();
        p.push(format!("havn-v1-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&p).expect("mkdir");
        let err = CgroupController::with_root(p).expect_err("must fail");
        assert!(matches!(err, CgroupError::NotV2 { .. }));
    }

    #[test]
    fn with_root_creates_parent_when_missing() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        assert!(ctl.parent().exists());
        assert_eq!(
            ctl.parent().file_name().and_then(|s| s.to_str()),
            Some(AGENT_CGROUP_PARENT)
        );
    }

    #[test]
    fn create_agent_writes_limits() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let limits = ResourceLimits {
            memory_mb: 256,
            cpu_cores: 0.5,
            pids_max: 32,
        };
        let path = ctl.create_agent("abc", &limits).expect("create");

        assert!(path.exists());
        let mem = std::fs::read_to_string(path.join("memory.max")).expect("memory.max");
        assert_eq!(mem, (256 * 1024 * 1024).to_string());
        let cpu = std::fs::read_to_string(path.join("cpu.max")).expect("cpu.max");
        assert_eq!(cpu, "50000 100000");
        let pids = std::fs::read_to_string(path.join("pids.max")).expect("pids.max");
        assert_eq!(pids, "32");
    }

    #[test]
    fn create_agent_zero_means_unlimited() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let limits = ResourceLimits {
            memory_mb: 0,
            cpu_cores: 0.0,
            pids_max: 0,
        };
        let path = ctl.create_agent("z", &limits).expect("create");

        assert_eq!(
            std::fs::read_to_string(path.join("memory.max")).unwrap(),
            "max"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("pids.max")).unwrap(),
            "max"
        );
        // cpu.max format: `max <period>`
        assert_eq!(
            std::fs::read_to_string(path.join("cpu.max")).unwrap(),
            "max 100000"
        );
    }

    #[test]
    fn create_agent_is_idempotent() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let limits = ResourceLimits::default();
        let p1 = ctl.create_agent("idem", &limits).expect("first");
        let p2 = ctl.create_agent("idem", &limits).expect("second");
        assert_eq!(p1, p2);
    }

    #[test]
    fn stats_reads_memory_pids_and_cpu_stat() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let path = ctl
            .create_agent("stats", &ResourceLimits::default())
            .unwrap();
        std::fs::write(path.join("memory.current"), "65536").unwrap();
        std::fs::write(path.join("pids.current"), "7").unwrap();
        std::fs::write(
            path.join("cpu.stat"),
            "usage_usec 1234567\nuser_usec 800000\nsystem_usec 434567\n",
        )
        .unwrap();
        let stats = ctl.stats(&path).expect("stats");
        assert_eq!(
            stats,
            AgentResourceStats {
                memory_bytes: 65536,
                cpu_microseconds: 1_234_567,
                pids: 7
            }
        );
    }

    #[test]
    fn stats_handles_missing_files() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let path = ctl
            .create_agent("noteardown", &ResourceLimits::default())
            .unwrap();
        let stats = ctl.stats(&path).expect("stats");
        assert_eq!(stats, AgentResourceStats::default());
    }

    #[test]
    fn kill_all_writes_one_to_cgroup_kill() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let path = ctl
            .create_agent("kill", &ResourceLimits::default())
            .unwrap();
        ctl.kill_all(&path).expect("kill");
        let kill = std::fs::read_to_string(path.join("cgroup.kill")).expect("cgroup.kill");
        assert_eq!(kill, "1");
    }

    #[test]
    fn cleanup_removes_cgroup_with_control_files() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let path = ctl.create_agent("rm", &ResourceLimits::default()).unwrap();
        // cleanup() must pre-unlink the control files we wrote so the rmdir
        // succeeds on tempdir; on real cgroupfs the unlinks are no-ops and
        // the kernel cleans up the virtual files when rmdir lands.
        ctl.cleanup(&path).expect("cleanup");
        assert!(!path.exists());
    }

    #[test]
    fn populated_follows_cgroup_events() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let path = ctl.create_agent("ev", &ResourceLimits::default()).unwrap();
        assert!(!ctl.is_populated(&path), "no cgroup.events: nothing left");
        std::fs::write(path.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(ctl.is_populated(&path));
        std::fs::write(path.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert!(!ctl.is_populated(&path));
        ctl.cleanup(&path).expect("cleanup");
    }

    #[test]
    fn agent_cgroups_lists_only_agent_directories() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let a = ctl.create_agent("a", &ResourceLimits::default()).unwrap();
        let b = ctl.create_agent("b", &ResourceLimits::default()).unwrap();
        std::fs::create_dir(ctl.parent().join("other")).unwrap();
        let mut found = ctl.agent_cgroups();
        found.sort();
        assert_eq!(found, vec![a, b]);
    }

    #[test]
    fn agent_path_matches_the_peer_cgroup() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        let relative = ctl
            .agent_path("abc")
            .strip_prefix(&root)
            .expect("under root")
            .to_string_lossy()
            .into_owned();
        assert_eq!(format!("/{relative}"), havn_core::agent_cgroup("abc"));
        assert_eq!(ctl.parent(), root.join("havn"));
    }

    #[test]
    fn retired_agent_cgroups_are_found_apart_from_current_ones() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        assert!(ctl.retired_parent().is_none());
        assert!(ctl.retired_agent_cgroups().is_empty());
        let current = ctl.create_agent("now", &ResourceLimits::default()).unwrap();
        let mut old = retired_parent_with(&root, &["a", "b"]);
        let mut found = ctl.retired_agent_cgroups();
        found.sort();
        old.sort();
        assert_eq!(found, old);
        assert_eq!(ctl.agent_cgroups(), vec![current]);
        assert_eq!(ctl.retired_parent(), Some(root.join("havn.slice")));
    }

    #[test]
    fn retired_parent_is_removed_only_when_empty_and_not_a_systemd_unit() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        let parent = root.join(RETIRED_PARENT);
        let agents = retired_parent_with(&root, &["a"]);

        assert!(
            !ctl.remove_retired_parent(SliceUnit::Inactive),
            "agent cgroup left"
        );
        for agent in &agents {
            ctl.cleanup(agent).unwrap();
        }
        assert!(!ctl.remove_retired_parent(SliceUnit::Active));
        assert!(!ctl.remove_retired_parent(SliceUnit::Unknown));
        assert!(parent.is_dir());
        assert!(ctl.remove_retired_parent(SliceUnit::Inactive));
        assert!(!parent.exists());
        assert!(
            !ctl.remove_retired_parent(SliceUnit::Inactive),
            "already gone"
        );
    }

    #[test]
    fn slice_unit_follows_systemd_active_state() {
        assert_eq!(
            SliceUnit::from_active_state("inactive\n"),
            SliceUnit::Inactive
        );
        assert_eq!(SliceUnit::from_active_state("failed"), SliceUnit::Inactive);
        assert_eq!(SliceUnit::from_active_state("active"), SliceUnit::Active);
        assert_eq!(
            SliceUnit::from_active_state("deactivating"),
            SliceUnit::Active
        );
        assert_eq!(SliceUnit::from_active_state(""), SliceUnit::Unknown);
        assert_eq!(
            SliceUnit::from_active_state("maintenance"),
            SliceUnit::Unknown
        );
    }

    #[test]
    fn cpu_max_format_handles_one_core() {
        assert_eq!(format_cpu_max(1.0), "100000 100000");
    }

    #[test]
    fn cpu_max_format_rounds_fractional() {
        assert_eq!(format_cpu_max(0.25), "25000 100000");
    }
}
