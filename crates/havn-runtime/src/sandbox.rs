//! Landlock-LSM filesystem restriction for the runtime process.
//!
//! Used only without `havn-init` (the subprocess spawner, which is not an
//! isolation boundary): a ruleset applied from here confines the calling
//! thread only. Under the namespace spawner `havn-init` applies the rules
//! (`havn_spawner::filesystem_rules`) before exec, covering every thread.
//!
//! Applied late in startup — by the time we call [`restrict_filesystem`]:
//!
//! - The per-agent `agent.db` is already open (the file descriptor survives
//!   even if the path becomes inaccessible to fresh `open()` calls).
//! - The Unix socket to the gateway is already connected.
//! - Workspace skills have already been loaded into the FTS index
//!   (the file reads happened during `skills::load_workspace`).
//!
//! From here on, the agent has read-write access to its workspace and `/tmp`,
//! and read-only access to a small set of system paths required for the shell
//! tool's subprocesses to find their dynamic libraries. Everything else
//! (`/home/<other-user>`, `/root`, `/var`, `/opt`, …) is denied at the
//! kernel level — even for tools the LLM hasn't yet been wired to.
//!
//! Layered on top of:
//! - the namespace `unshare()` which already isolates the agent's mount
//!   table and network stack,
//! - the cgroup which already caps memory/CPU/pids.
//!
//! The seccomp filter in [`crate::seccomp`] additionally restricts the
//! _syscall surface_ regardless of which paths the agent can see.
//!
//! Every process a tool starts runs in a Landlock domain of its own nested
//! under the runtime's ([`create_tool_domain`], [`confine_tool`]).

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::Path;
use std::sync::OnceLock;

use landlock::{
    ABI, Access as _, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus,
};
use thiserror::Error;
use tracing::{info, warn};

/// Read-execute system paths the runtime grants to itself + any subprocess
/// it spawns (e.g. `sh -c` from the `shell` tool). Tight enough that
/// `/home`, `/root`, `/var`, and `/opt` stay denied.
const SYSTEM_READ_PATHS: &[&str] = &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"];

/// Operator opt-out, forwarded by the spawner from the gateway's
/// environment, for running agents on kernels without Landlock.
pub const ALLOW_NO_LANDLOCK: &str = "HAVN_ALLOW_NO_LANDLOCK";

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SandboxError {
    #[error("landlock api error: {0}")]
    Landlock(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Apply the Landlock ruleset to the running process. Returns `Ok(())`
/// even when the kernel doesn't fully enforce — see logs for the actual
/// status. Calling this twice in one process is harmless: the kernel
/// intersects rulesets, narrowing access further.
///
/// `extra_rw` / `extra_ro` are the union of every enabled MCP server's
/// declared paths (spec §13 Phase 3). Each agent gets one Landlock
/// ruleset for the whole process — child processes (the MCP server
/// subprocesses) inherit it — so per-server isolation isn't possible
/// without a per-server reaper. The trade-off is documented on
/// `havn_core::McpServerConfig`. For the typical case (paths under
/// the workspace), the union is a no-op because the workspace is
/// already granted full access below.
pub fn restrict_filesystem(
    workspace: &Path,
    extra_rw: &[&Path],
    extra_ro: &[&Path],
) -> Result<(), SandboxError> {
    // ABI v3 covers the access bits we need (read, write, execute, refer,
    // truncate, makers). Newer kernels add network restrictions; we'll
    // pick those up by upgrading ABI when seccomp + network gating land.
    let abi = ABI::V3;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .handle_access(access_all)
        .map_err(|e| SandboxError::Landlock(format!("handle_access: {e}")))?
        .create()
        .map_err(|e| SandboxError::Landlock(format!("create: {e}")))?;

    // Workspace gets full read+write+execute. Skills under it may include
    // shell scripts the agent invokes, so execute is required.
    ruleset = add_path_beneath(ruleset, workspace, access_all)
        .map_err(|e| SandboxError::Landlock(format!("workspace {}: {e}", workspace.display())))?;

    // /tmp gets full access — scratch space for tools.
    match add_path_beneath_optional(ruleset, Path::new("/tmp"), access_all) {
        Ok(r) => ruleset = r,
        Err(e) => {
            warn!(error = %e, "/tmp not accessible; agent will not be able to write scratch files");
            ruleset = e.ruleset;
        }
    }

    // System paths get read+execute. Missing paths (some distros lack /sbin)
    // are silently skipped — the rule simply isn't added, denying access
    // the same as if it had never been requested.
    for path in SYSTEM_READ_PATHS {
        match add_path_beneath_optional(ruleset, Path::new(path), access_read) {
            Ok(r) => ruleset = r,
            Err(e) => ruleset = e.ruleset,
        }
    }

    // MCP-server-declared extras (spec §13 Phase 3). v1 requires these
    // paths to already exist on the host — typically under the
    // workspace or under /usr (e.g. /usr/share/havn/mcp-servers/<bin>'s
    // own data dir). Missing paths are skipped just like the system
    // ones above; an MCP server that genuinely needs an absent path
    // will fail at its first I/O, which surfaces in tool_result errors.
    for path in extra_rw {
        match add_path_beneath_optional(ruleset, path, access_all) {
            Ok(r) => ruleset = r,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "mcp extra_paths_rw not added");
                ruleset = e.ruleset;
            }
        }
    }
    for path in extra_ro {
        match add_path_beneath_optional(ruleset, path, access_read) {
            Ok(r) => ruleset = r,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "mcp extra_paths_ro not added");
                ruleset = e.ruleset;
            }
        }
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::Landlock(format!("restrict_self: {e}")))?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => info!("landlock fully enforced"),
        RulesetStatus::PartiallyEnforced => {
            warn!("landlock partially enforced (kernel supports a subset of requested ABI)");
        }
        RulesetStatus::NotEnforced => {
            if std::env::var_os(ALLOW_NO_LANDLOCK).as_deref() != Some(std::ffi::OsStr::new("1")) {
                return Err(SandboxError::Landlock(format!(
                    "Landlock is not enforced (kernel < 5.13 or LSM disabled), so the agent \
                     would have unrestricted filesystem access; enable Landlock, or set \
                     {ALLOW_NO_LANDLOCK}=1 for the gateway to run agents without it"
                )));
            }
            warn!(
                "landlock NOT enforced and {ALLOW_NO_LANDLOCK}=1: the agent has unrestricted \
                 filesystem access"
            );
        }
    }
    Ok(())
}

type CreatedRuleset = landlock::RulesetCreated;
type AccessFlags = BitFlags<AccessFs>;

fn add_path_beneath(
    ruleset: CreatedRuleset,
    path: &Path,
    access: AccessFlags,
) -> Result<CreatedRuleset, String> {
    let fd = PathFd::new(path).map_err(|e| format!("PathFd::new({}): {e}", path.display()))?;
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|e| format!("add_rule({}): {e}", path.display()))
}

/// Internal error type that gives the original ruleset back so the caller
/// can keep building when an optional path doesn't exist on this host.
struct OptionalRuleError {
    ruleset: CreatedRuleset,
    inner: String,
}

impl std::fmt::Display for OptionalRuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.inner)
    }
}

fn add_path_beneath_optional(
    ruleset: CreatedRuleset,
    path: &Path,
    access: AccessFlags,
) -> Result<CreatedRuleset, OptionalRuleError> {
    let Ok(fd) = PathFd::new(path) else {
        return Err(OptionalRuleError {
            ruleset,
            inner: format!("PathFd::new({}): missing", path.display()),
        });
    };
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|e| OptionalRuleError {
            // `add_rule` consumes the ruleset on success; on failure the
            // landlock crate returns it via the error. We can't reconstruct
            // it cheaply here, so callers treat this as "skip path" by
            // creating a fresh ruleset for subsequent paths is not an option
            // either — so we use a sentinel CreatedRuleset wrapper. In
            // practice this branch only fires for genuinely broken paths
            // (e.g. /tmp deleted mid-startup), which is a fatal-ish state.
            ruleset: rebuild_or_die(),
            inner: format!("add_rule({}): {e}", path.display()),
        })
}

/// Last-resort fresh ruleset for the [`OptionalRuleError`] path — only
/// reached when `add_rule` has consumed the original ruleset and then
/// failed. In practice this is unreachable on healthy hosts; we panic so
/// the failure is loud rather than producing a silently broken sandbox.
fn rebuild_or_die() -> CreatedRuleset {
    panic!("landlock ruleset became unrecoverable after add_rule failure");
}

/// Ruleset of the Landlock domain tool processes run in, once
/// [`create_tool_domain`] has made it.
static TOOL_DOMAIN: OnceLock<OwnedFd> = OnceLock::new();

/// `LANDLOCK_ACCESS_FS_MAKE_BLOCK`.
const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;

/// The first field of `struct landlock_ruleset_attr`, all a Landlock ABI 1
/// kernel reads.
#[repr(C)]
struct ToolRulesetAttr {
    handled_access_fs: u64,
}

/// Create the ruleset of the domain every process a tool starts runs in,
/// nested under the runtime's. Landlock lets a process ptrace another,
/// which reading its `/proc/<pid>/environ`, `mem`, `fd/` or `cwd` takes,
/// only when that one is in the same domain or one nested under it. MCP
/// servers stay in the runtime's domain, so a tool, though it runs as the
/// same uid, cannot read the secrets the operator passed to them. The
/// ruleset handles only creating block devices, which a process without
/// capabilities cannot do anyway, and allows it nowhere.
///
/// Fails with `ENOSYS` or `EOPNOTSUPP` on a kernel without Landlock.
pub fn create_tool_domain() -> std::io::Result<()> {
    let attr = ToolRulesetAttr {
        handled_access_fs: ACCESS_FS_MAKE_BLOCK,
    };
    // SAFETY: a valid attribute struct of the size passed; no flags.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            std::mem::size_of::<ToolRulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = libc::c_int::try_from(fd).map_err(std::io::Error::other)?;
    // SAFETY: the kernel just returned this descriptor (close-on-exec), and
    // nothing else owns it.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };
    // A second call keeps the first ruleset; they are the same.
    let _ = TOOL_DOMAIN.set(ruleset);
    Ok(())
}

/// Have the process `command` starts enter the tools' domain before it
/// execs, once [`create_tool_domain`] has made it; the spawn fails if it
/// cannot. Sets `no_new_privs`, which Landlock requires and the runtime's
/// seccomp filter has already set.
pub fn confine_tool(command: &mut tokio::process::Command) {
    let Some(ruleset) = TOOL_DOMAIN.get() else {
        return;
    };
    let fd = ruleset.as_raw_fd();
    // SAFETY: the hook makes two raw syscalls with integer arguments, both
    // async-signal-safe, on a descriptor that lives as long as the process.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_read_paths_cover_dynamic_loader_dependencies() {
        // The runtime spawns subprocesses (shell tool); they need /lib or
        // /lib64 for ld.so + libc, /bin or /usr/bin for `sh`. Don't let an
        // accidental edit drop these without notice.
        for required in &["/lib", "/usr", "/bin"] {
            assert!(
                SYSTEM_READ_PATHS.contains(required),
                "SYSTEM_READ_PATHS missing required {required}"
            );
        }
    }

    /// A tool's process cannot read the environment of a process outside
    /// its domain, such as an MCP server, though both run as this uid.
    #[tokio::test]
    async fn tools_cannot_read_the_environment_of_processes_outside_their_domain() {
        match create_tool_domain() {
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EOPNOTSUPP)) => {
                return;
            }
            other => other.expect("tool domain"),
        }
        let mut server = tokio::process::Command::new("sleep")
            .arg("30")
            .env("MCP_SECRET", "s3cret")
            .kill_on_drop(true)
            .spawn()
            .expect("sleep");
        let environ = format!("/proc/{}/environ", server.id().expect("pid"));
        let read = |confined: bool| {
            let mut cmd = tokio::process::Command::new("cat");
            cmd.arg(&environ);
            if confined {
                confine_tool(&mut cmd);
            }
            cmd.output()
        };
        let outside = read(false).await.expect("cat");
        assert!(String::from_utf8_lossy(&outside.stdout).contains("MCP_SECRET=s3cret"));
        let tool = read(true).await.expect("cat");
        assert!(!tool.status.success(), "{tool:?}");
        assert!(!String::from_utf8_lossy(&tool.stdout).contains("s3cret"));
        server.kill().await.expect("kill");
    }

    #[test]
    fn system_read_paths_exclude_user_data_locations() {
        // These paths are precisely what we DO NOT want the agent reading.
        // Locking them out is the entire point of this vertical.
        for forbidden in &["/home", "/root", "/var", "/opt", "/srv", "/mnt"] {
            assert!(
                !SYSTEM_READ_PATHS.contains(forbidden),
                "SYSTEM_READ_PATHS must not include {forbidden}"
            );
        }
    }
}
