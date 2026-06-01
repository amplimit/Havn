//! Agent spawner — the trait that creates isolated agent processes.
//!
//! Two implementations are provided.
//!
//! [`subprocess::SubprocessSpawner`] is the Phase 1 fallback: plain
//! `tokio::process::Command` exec without isolation. Used during development
//! and as a portable smoke-test harness.
//!
//! [`namespace::NamespaceSpawner`] is the Linux-native isolation backend
//! (namespace, Landlock, seccomp, cgroup; spec §4.1). Phase 1 ships a stub;
//! the full implementation lands in a follow-up vertical.
//!
//! The trait surface is preserved so future backends (gVisor,
//! Firecracker) can be added without touching the gateway.

use std::path::PathBuf;

use async_trait::async_trait;
use havn_core::{AgentId, Policy};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod namespace;
#[cfg(target_os = "linux")]
pub mod network;
#[cfg(target_os = "linux")]
#[allow(clippy::print_stderr)]
pub mod ns_setup;
pub mod subprocess;

/// Per-agent configuration handed to the spawner at spawn time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpawnConfig {
    pub agent_id: AgentId,
    /// Workspace dir on the host. The spawner ensures this exists with the
    /// expected layout (`SOUL.md`, `USER.md`, `HEARTBEAT.md`, `IDENTITY.md`,
    /// `skills/`, `agent.db`) before launching the runtime — see spec §5.3.
    pub workspace_dir: PathBuf,
    /// Path to the `havn-runtime` binary the spawner exec()s.
    pub runtime_binary: PathBuf,
    /// Path to the `havn-init` shim binary. Only used by the
    /// [`namespace::NamespaceSpawner`] (spec §4.1 PID-namespace dance):
    /// spawner exec's `havn-init <runtime> <args>`, init forks so one side
    /// becomes PID 1 in the new PID ns and exec's the runtime, the other
    /// side waits + `_exit`s. `None` means subprocess fallback skips the
    /// shim entirely.
    #[serde(default)]
    pub init_binary: Option<PathBuf>,
    /// Path to the gateway's listening socket. The runtime dials this on startup.
    pub gateway_socket: PathBuf,
    /// True when this is a subagent. Subagents inherit parent cgroup, are killed
    /// with parent (`PR_SET_PDEATHSIG` + `cgroup.kill` backstop), and have no
    /// `subagent_spawn` tool. See spec §4.5.
    #[serde(default)]
    pub is_subagent: bool,
    /// Operator-declared extra host paths to mirror-bind into the agent
    /// namespace at the same absolute path (spec §4.1 v0.7). Sourced from
    /// the gateway's `[[extra_mounts]]` config. **Operator-only**: an
    /// agent's per-agent config CANNOT specify mounts (would let any
    /// owner escalate to arbitrary host paths). Common case: capability
    /// packs that drop binaries under `/opt/havn/<pack>/` and need
    /// agents to see them.
    #[serde(default)]
    pub extra_mounts: Vec<ExtraMount>,
    /// Operator-declared in-namespace tmpfs mounts (spec §16.2). Mounted
    /// inside the new rootfs after pivot, with `MS_NOSUID|MS_NODEV` always
    /// on. The classic motivating case is `/dev/shm` for any workload that
    /// uses POSIX shared memory (Chromium-based browsers crash without it
    /// on non-trivial pages); havn-spawner does **not** know which
    /// workload needs it. Operator-only; same trust posture as
    /// `extra_mounts`.
    #[serde(default)]
    pub tmpfs_mounts: Vec<TmpfsMount>,
    /// Syscall names to **remove** from the runtime's seccomp blocklist
    /// (spec §16.2). Operators relax the blocklist by name — the runtime
    /// validates against `libc::SYS_*` and refuses unknown names. Carried
    /// here so the gateway has a single struct to populate and the
    /// spawner can forward to havn-init / runtime via env / Welcome.
    /// Operator-only; same trust posture as `extra_mounts`.
    #[serde(default)]
    pub seccomp_allow_extra: Vec<String>,
    /// DNS servers written into the agent namespace's `/etc/resolv.conf`
    /// when egress is allowed (spec §6.3 v0.7). Operator-configurable
    /// via `[network] agent_dns = [...]` in the gateway config; the
    /// spawner forwards via env, havn-init writes the file. Empty list
    /// is treated as "use the kernel default" — practically meaning
    /// "no resolvers, DNS will fail" — but the GatewayConfig default
    /// supplies public resolvers so this should never actually be
    /// empty in production.
    #[serde(default)]
    pub agent_dns: Vec<String>,
}

/// One operator-declared tmpfs mount carried into the agent namespace
/// (spec §16.2). Pack-agnostic — the spawner doesn't know what's going
/// to use the path. Always mounted with `MS_NOSUID|MS_NODEV` for the
/// same hardening reasons as the rootfs and `/tmp` tmpfs that
/// `pivot_root_into_minimal_rootfs` already mounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmpfsMount {
    /// Absolute path inside the agent namespace. Must already be a
    /// directory under the new rootfs (or be a path the spawner creates
    /// via `mkdir_under_newroot` — currently anything under root).
    pub path: PathBuf,
    /// Tmpfs size in megabytes. 0 means "kernel default" (typically
    /// half of physical RAM — almost certainly not what you want).
    /// Reasonable values: 64–1024 depending on workload.
    #[serde(default = "default_tmpfs_size_mb")]
    pub size_mb: u64,
}

fn default_tmpfs_size_mb() -> u64 {
    64
}

/// One operator-declared bind-mount carried into the agent namespace.
/// Same path on host and inside the namespace (mirror-bind); pack
/// authors who want the binary on the agent's PATH should symlink into
/// `/usr/local/bin` on the host (see havn-pack-browser as reference).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraMount {
    pub path: PathBuf,
    /// Read-only by default — packs almost always want their binaries +
    /// data files exposed RO. RW is opt-in for packs that need a shared
    /// state directory across agents.
    #[serde(default)]
    pub mode: MountMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    #[default]
    Ro,
    Rw,
}

impl MountMode {
    /// Wire-format used in the `HAVN_EXTRA_MOUNTS` env var the spawner
    /// hands to havn-init. Compact + dependency-free so havn-init can
    /// parse without serde_json (it has no logging stack — diagnostics
    /// go to stderr).
    #[must_use]
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Ro => "ro",
            Self::Rw => "rw",
        }
    }

    pub fn from_wire_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "ro" => Ok(Self::Ro),
            "rw" => Ok(Self::Rw),
            _ => Err("expected 'ro' or 'rw'"),
        }
    }
}

/// Encode a slice of mounts as the `HAVN_EXTRA_MOUNTS` env-var value.
/// Format: `path:mode,path:mode,...` — colon-separated within each
/// entry, comma-separated across entries. Whitespace is significant
/// (don't add any). Empty input → empty string.
///
/// Why not JSON: havn-init runs before the runtime starts and has no
/// logging stack / no serde_json dep. A trivial format keeps it
/// trivial.
#[must_use]
pub fn encode_extra_mounts(mounts: &[ExtraMount]) -> String {
    mounts
        .iter()
        .map(|m| format!("{}:{}", m.path.display(), m.mode.as_wire_str()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Encode tmpfs mounts as the `HAVN_TMPFS_MOUNTS` env-var value.
/// Format: `path:size_mb,path:size_mb,...`. Mirrors `encode_extra_mounts`
/// — same delimiter rules, same JSON-free philosophy so havn-init has
/// no need for serde_json.
#[must_use]
pub fn encode_tmpfs_mounts(mounts: &[TmpfsMount]) -> String {
    mounts
        .iter()
        .map(|m| format!("{}:{}", m.path.display(), m.size_mb))
        .collect::<Vec<_>>()
        .join(",")
}

/// Inverse of [`encode_tmpfs_mounts`]. Lenient about whitespace; rejects
/// missing or non-numeric size. Used by havn-init.
pub fn decode_tmpfs_mounts(s: &str) -> Result<Vec<TmpfsMount>, String> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    s.split(',')
        .map(str::trim)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            let (path, size) = chunk
                .rsplit_once(':')
                .ok_or_else(|| format!("missing size_mb in {chunk:?}; expected 'path:N'"))?;
            let size_mb: u64 = size
                .parse()
                .map_err(|e| format!("{chunk:?}: size_mb {size:?} not numeric: {e}"))?;
            Ok(TmpfsMount {
                path: PathBuf::from(path),
                size_mb,
            })
        })
        .collect()
}

/// Encode seccomp syscall-name allowlist as `HAVN_SECCOMP_ALLOW_EXTRA`.
/// Comma-separated names; whitespace tolerant. The runtime is the
/// consumer (havn-init doesn't need it — seccomp is a runtime-side
/// kernel call, not a mount setup), but plumbing through env keeps
/// the data path uniform with the other capability-pack primitives.
#[must_use]
pub fn encode_seccomp_allow_extra(syscalls: &[String]) -> String {
    syscalls.join(",")
}

/// Inverse of [`encode_seccomp_allow_extra`]. Empty input → empty vec.
/// Validation that the names are real syscalls is the runtime's job
/// (against the `libc::SYS_*` constants); this just splits.
#[must_use]
pub fn decode_seccomp_allow_extra(s: &str) -> Vec<String> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

/// Inverse of [`encode_extra_mounts`]. Lenient: skips empty entries,
/// trims whitespace around each. Errors on missing mode or unknown mode
/// strings. Used by havn-init to pull the mount list out of its env.
pub fn decode_extra_mounts(s: &str) -> Result<Vec<ExtraMount>, String> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    s.split(',')
        .map(str::trim)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            let (path, mode) = chunk.rsplit_once(':').ok_or_else(|| {
                format!("missing mode in {chunk:?}; expected 'path:ro' or 'path:rw'")
            })?;
            let mode = MountMode::from_wire_str(mode).map_err(|e| format!("{chunk:?}: {e}"))?;
            Ok(ExtraMount {
                path: PathBuf::from(path),
                mode,
            })
        })
        .collect()
}

/// Opaque handle to a spawned agent process.
///
/// Spawner implementations track per-agent state (process handle, cgroup path, …)
/// internally; consumers see only the agent ID and OS pid.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    pub agent_id: AgentId,
    pub pid: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceStats {
    pub memory_bytes: u64,
    pub cpu_microseconds: u64,
    pub pids: u32,
}

#[async_trait]
pub trait AgentSpawner: Send + Sync + std::fmt::Debug {
    /// Spawn an agent process; returns a handle for later lifecycle calls.
    async fn spawn(&self, config: &AgentSpawnConfig, policy: &Policy) -> Result<AgentHandle>;

    /// Stop a running agent. SIGTERM, then SIGKILL after a grace period
    /// (5 seconds for the subprocess spawner; the namespace spawner adds a
    /// `cgroup.kill` backstop for the whole subtree).
    async fn stop(&self, handle: &AgentHandle) -> Result<()>;

    /// Read current resource usage. Returns zeroes for backends that don't track it.
    async fn stats(&self, handle: &AgentHandle) -> Result<ResourceStats>;
}

pub type Result<T, E = SpawnerError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnerError {
    #[error("isolation primitive unavailable: {0}")]
    IsolationUnavailable(String),
    #[error("policy violation: {0}")]
    PolicyViolation(String),
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("agent not found: {0}")]
    NotFound(AgentId),
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn extra_mounts_codec_round_trip() {
        let mounts = vec![
            ExtraMount {
                path: PathBuf::from("/opt/havn/playwright-cli"),
                mode: MountMode::Ro,
            },
            ExtraMount {
                path: PathBuf::from("/opt/havn/shared-state"),
                mode: MountMode::Rw,
            },
        ];
        let wire = encode_extra_mounts(&mounts);
        assert_eq!(
            wire,
            "/opt/havn/playwright-cli:ro,/opt/havn/shared-state:rw"
        );
        let decoded = decode_extra_mounts(&wire).expect("decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].path, mounts[0].path);
        assert_eq!(decoded[0].mode, MountMode::Ro);
        assert_eq!(decoded[1].mode, MountMode::Rw);
    }

    #[test]
    fn extra_mounts_codec_empty_input() {
        assert!(decode_extra_mounts("").expect("ok").is_empty());
        assert!(decode_extra_mounts("   ").expect("ok").is_empty());
        assert_eq!(encode_extra_mounts(&[]), "");
    }

    #[test]
    fn extra_mounts_codec_lenient_whitespace_and_blanks() {
        // Spawner should never emit these but havn-init must not panic.
        let r = decode_extra_mounts(" /a:ro , ,/b:rw ").expect("ok");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].path, PathBuf::from("/a"));
        assert_eq!(r[1].path, PathBuf::from("/b"));
    }

    #[test]
    fn extra_mounts_codec_missing_mode_errors() {
        assert!(decode_extra_mounts("/no-mode").is_err());
    }

    #[test]
    fn extra_mounts_codec_unknown_mode_errors() {
        assert!(decode_extra_mounts("/p:rwx").is_err());
        assert!(decode_extra_mounts("/p:RO").is_err());
    }

    #[test]
    fn tmpfs_mounts_codec_round_trip() {
        let mounts = vec![
            TmpfsMount {
                path: PathBuf::from("/dev/shm"),
                size_mb: 1024,
            },
            TmpfsMount {
                path: PathBuf::from("/tmp/scratch"),
                size_mb: 64,
            },
        ];
        let wire = encode_tmpfs_mounts(&mounts);
        assert_eq!(wire, "/dev/shm:1024,/tmp/scratch:64");
        let decoded = decode_tmpfs_mounts(&wire).expect("decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].path, PathBuf::from("/dev/shm"));
        assert_eq!(decoded[0].size_mb, 1024);
    }

    #[test]
    fn tmpfs_mounts_codec_empty() {
        assert!(decode_tmpfs_mounts("").expect("ok").is_empty());
        assert!(decode_tmpfs_mounts("   ").expect("ok").is_empty());
        assert_eq!(encode_tmpfs_mounts(&[]), "");
    }

    #[test]
    fn tmpfs_mounts_codec_rejects_missing_size() {
        assert!(decode_tmpfs_mounts("/dev/shm").is_err());
    }

    #[test]
    fn tmpfs_mounts_codec_rejects_non_numeric_size() {
        assert!(decode_tmpfs_mounts("/dev/shm:huge").is_err());
    }

    #[test]
    fn seccomp_allow_extra_codec_round_trip() {
        let names = vec!["pivot_root".into(), "userfaultfd".into()];
        let wire = encode_seccomp_allow_extra(&names);
        assert_eq!(wire, "pivot_root,userfaultfd");
        let decoded = decode_seccomp_allow_extra(&wire);
        assert_eq!(decoded, names);
    }

    #[test]
    fn seccomp_allow_extra_codec_handles_whitespace_and_blanks() {
        let decoded = decode_seccomp_allow_extra(" pivot_root ,, userfaultfd ");
        assert_eq!(decoded, vec!["pivot_root", "userfaultfd"]);
    }

    #[test]
    fn extra_mounts_codec_handles_path_with_colon() {
        // rsplit_once means a path component containing ':' is supported
        // — the LAST colon separates mode. Unusual but possible (a
        // pack pinning a versioned dir like /opt/havn/foo:1.2.3).
        let r = decode_extra_mounts("/opt/havn/foo:1.2.3:ro").expect("ok");
        assert_eq!(r[0].path, PathBuf::from("/opt/havn/foo:1.2.3"));
        assert_eq!(r[0].mode, MountMode::Ro);
    }
}
