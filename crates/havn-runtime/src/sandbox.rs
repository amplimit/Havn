//! Landlock-LSM filesystem restriction for the runtime process.
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
//! Future seccomp filter (next vertical) will additionally restrict the
//! _syscall surface_ regardless of which paths the agent can see.

#![cfg(target_os = "linux")]

use std::path::Path;

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
        RulesetStatus::NotEnforced => warn!(
            "landlock NOT enforced — kernel < 5.13 or LSM disabled. Agent has unrestricted \
             filesystem access. Recommended: enable Landlock in kernel config or upgrade."
        ),
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
