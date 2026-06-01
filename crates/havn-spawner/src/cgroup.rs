//! cgroup v2 controller for per-agent resource limits (spec §4.1).
//!
//! Layout:
//!
//! ```text
//! /sys/fs/cgroup/                          ← unified cgroup v2 mount
//!   havn.slice/                            ← gateway's slice (created on init)
//!     agent-<uuid>/                        ← per-agent leaf
//!       memory.max
//!       cpu.max
//!       pids.max
//!       cgroup.procs   ← agent PID is written here after spawn
//!       cgroup.kill    ← writing "1" SIGKILLs every PID in this cgroup
//! ```
//!
//! Phase 1.5 deliberately uses *only* cgroup; namespace + Landlock + seccomp
//! arrive in subsequent verticals. The controller takes the cgroup root as a
//! parameter so unit tests can substitute a tempdir; production binds
//! `/sys/fs/cgroup` via [`Self::system`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use havn_core::ResourceLimits;
use thiserror::Error;
use tracing::{debug, warn};

/// Default systemd-friendly slice name. We don't manage the slice lifecycle —
/// systemd or the operator does that — but we create the directory if it's
/// missing and writable.
const SLICE: &str = "havn.slice";

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
    #[error("cannot create slice {0}")]
    SliceCreate(PathBuf),
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
    /// Resolved slice path: `<root>/havn.slice`.
    slice: PathBuf,
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
        let slice = root.join(SLICE);
        if !slice.exists()
            && let Err(e) = fs::create_dir(&slice)
        {
            warn!(
                slice = %slice.display(),
                error = %e,
                "cannot create havn.slice — install systemd unit with Delegate=yes or run as root"
            );
            return Err(CgroupError::SliceCreate(slice));
        }
        Ok(Self { root, slice })
    }

    /// Slice path that the controller is operating under. Useful for tests.
    pub fn slice(&self) -> &Path {
        &self.slice
    }

    /// Returns the cgroup root path supplied at construction time.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the per-agent cgroup directory and write its resource limits.
    /// Returns the cgroup path, ready for [`Self::add_pid`] after the agent
    /// process has been spawned.
    pub fn create_agent(
        &self,
        agent_id: &str,
        limits: &ResourceLimits,
    ) -> Result<PathBuf, CgroupError> {
        let path = self.slice.join(format!("agent-{agent_id}"));
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

    /// Move a PID into the agent's cgroup. Per cgroup v2 single-writer rules,
    /// the PID may only belong to one cgroup at a time — moving from the slice
    /// or root cgroup into the agent's leaf is the standard pattern.
    pub fn add_pid(&self, cgroup: &Path, pid: u32) -> Result<(), CgroupError> {
        let path = cgroup.join("cgroup.procs");
        write_file(&path, &pid.to_string())
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

/// Files we touch in [`CgroupController::create_agent`] / [`CgroupController::add_pid`] /
/// [`CgroupController::kill_all`]. Listed here so [`CgroupController::cleanup`] can
/// pre-unlink them in test environments where they're real on-disk files.
const CONTROL_FILES: &[&str] = &[
    "memory.max",
    "cpu.max",
    "pids.max",
    "cgroup.procs",
    "cgroup.kill",
];

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
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::fs::File;

    /// Build a minimal "cgroup v2 root" in a tempdir: contains
    /// `cgroup.controllers` so [`CgroupController::with_root`] is happy.
    fn fake_v2_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("havn-cg-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&p).expect("mkdir");
        File::create(p.join("cgroup.controllers")).expect("touch cgroup.controllers");
        p
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
    fn with_root_creates_slice_when_missing() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        assert!(ctl.slice().exists());
        assert_eq!(
            ctl.slice().file_name().and_then(|s| s.to_str()),
            Some(SLICE)
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
    fn add_pid_writes_to_cgroup_procs() {
        let root = fake_v2_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let path = ctl.create_agent("pid", &ResourceLimits::default()).unwrap();
        ctl.add_pid(&path, 12345).expect("add");
        let procs = std::fs::read_to_string(path.join("cgroup.procs")).expect("cgroup.procs");
        assert_eq!(procs, "12345");
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
    fn cpu_max_format_handles_one_core() {
        assert_eq!(format_cpu_max(1.0), "100000 100000");
    }

    #[test]
    fn cpu_max_format_rounds_fractional() {
        assert_eq!(format_cpu_max(0.25), "25000 100000");
    }
}
