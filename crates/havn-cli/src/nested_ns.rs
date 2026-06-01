//! Nested user-namespace precheck (spec §16).
//!
//! Some workloads — anything that itself wants `unshare(CLONE_NEWUSER)`
//! from inside the agent's own user namespace — depend on the host
//! kernel allowing nested unprivileged user namespaces. The flagship
//! example is Chromium's user-ns sandbox in browser packs, but this
//! module is **not browser-specific**: any pack whose workload nests
//! user namespaces benefits from the same check, and havn itself
//! checks at setup so the operator gets advance notice rather than a
//! surprise at first agent spawn.
//!
//! The test forks once, performs `unshare(CLONE_NEWUSER)` twice
//! back-to-back inside the child, and runs `/bin/true` to confirm a
//! clean exec inside the inner ns. No havn machinery; no pack
//! dependency; ~1 ms.

#![cfg(target_os = "linux")]
#![allow(unsafe_code, reason = "fork + unshare + execve are kernel primitives")]

use std::ffi::CString;

/// Outcome of the precheck. `Ok` means the host kernel admits the
/// nested-userns pattern packs like Chromium-based ones rely on.
/// Other variants name the failing step so the operator gets actionable
/// output (e.g. which sysctl to flip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NestedUserNsResult {
    /// Both unshares succeeded and the inner exec worked.
    Ok,
    /// Outer `unshare(CLONE_NEWUSER)` failed. The kernel does not
    /// admit unprivileged user namespaces at all — havn's regular
    /// agent isolation cannot run on this host either.
    OuterFailed { errno: i32 },
    /// Outer worked, nested `unshare(CLONE_NEWUSER)` failed.
    /// `kernel.max_user_namespaces` exhausted, an LSM blocking the
    /// nesting, or `user.max_user_namespaces` set low.
    NestedFailed { errno: i32 },
    /// `execve(/bin/true)` inside the inner ns failed. Probably means
    /// `/bin/true` isn't accessible (rare) or the kernel rejected exec
    /// across the namespace boundary.
    ExecFailed { errno: i32 },
    /// `fork()` itself failed before the test could run.
    ForkFailed { errno: i32 },
    /// `waitpid()` couldn't reap the child, or the child was killed
    /// by a signal.
    WaitFailed(String),
}

impl NestedUserNsResult {
    /// `(short_message, optional_long_remediation)`. The short message
    /// goes on a single CLI verdict line (`havn doctor`-style); the
    /// long remediation is the multi-line "what to fix" the operator
    /// reads when they care.
    #[must_use]
    pub fn explain(&self) -> (String, Option<String>) {
        match self {
            Self::Ok => ("nested user-namespace clone works".into(), None),
            Self::OuterFailed { errno } => (
                format!("kernel rejected unprivileged user-namespace clone (errno {errno})"),
                Some(
                    "Set `kernel.unprivileged_userns_clone=1` (Ubuntu / Debian) or rebuild \
                     with CONFIG_USER_NS=y. Without unprivileged user namespaces, havn \
                     agents cannot be isolated at all — this host is unsuitable."
                        .into(),
                ),
            ),
            Self::NestedFailed { errno } => (
                format!("nested unprivileged user-namespace clone failed (errno {errno})"),
                Some(
                    "The kernel allows one level of unprivileged user-ns but rejects nesting. \
                     Common causes: `user.max_user_namespaces` set too low, or a security \
                     module (AppArmor / SELinux) blocking nested unshare. Capability packs \
                     whose workload nests user namespaces (e.g. browser packs running \
                     Chromium's sandbox) cannot run on this host without disabling that \
                     workload's own sandbox — which would void havn's per-agent isolation \
                     guarantee. Either fix the kernel config or skip such packs on this host."
                        .into(),
                ),
            ),
            Self::ExecFailed { errno } => (
                format!("execve(/bin/true) inside inner user-ns failed (errno {errno})"),
                Some("Unusual; check that /bin/true exists and is executable.".into()),
            ),
            Self::ForkFailed { errno } => (
                format!("fork() before precheck failed (errno {errno})"),
                Some("Out of PIDs or out of memory? Re-run after the host recovers.".into()),
            ),
            Self::WaitFailed(s) => (
                format!("precheck child did not exit cleanly: {s}"),
                Some("Re-run; if persistent, file an issue with the host kernel version.".into()),
            ),
        }
    }
}

const EXIT_OK: i32 = 0;
const EXIT_OUTER_FAILED: i32 = 10;
const EXIT_NESTED_FAILED: i32 = 11;
const EXIT_EXEC_FAILED: i32 = 12;

/// Run the precheck. Forks once. Returns within ~50 ms on a healthy
/// host. Idempotent and safe to call from `havn setup` and `havn doctor`
/// at the same time (each is its own fork).
pub fn check() -> NestedUserNsResult {
    // SAFETY: `fork` is async-signal-safe. Branch on the documented
    // return; do not touch tokio / allocator state in the child beyond
    // what the libc calls below need.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => NestedUserNsResult::ForkFailed { errno: errno_now() },
        0 => {
            // Child: do both unshares + exec.
            // SAFETY: unshare and execve are async-signal-safe.
            if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
                unsafe { libc::_exit(EXIT_OUTER_FAILED) };
            }
            if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
                unsafe { libc::_exit(EXIT_NESTED_FAILED) };
            }
            // Run /bin/true inside the inner ns. If exec succeeds, the
            // process becomes /bin/true and exits 0. We treat any exec
            // failure path (rare) as EXIT_EXEC_FAILED.
            let prog = CString::new("/bin/true").expect("static cstr");
            let argv = [prog.as_ptr(), std::ptr::null()];
            unsafe { libc::execve(prog.as_ptr(), argv.as_ptr(), std::ptr::null()) };
            unsafe { libc::_exit(EXIT_EXEC_FAILED) };
        }
        child => {
            let mut status: libc::c_int = 0;
            // SAFETY: child pid we just forked.
            let r = unsafe { libc::waitpid(child, &mut status, 0) };
            if r == -1 {
                let e = std::io::Error::last_os_error();
                return NestedUserNsResult::WaitFailed(format!("waitpid: {e}"));
            }
            if libc::WIFEXITED(status) {
                match libc::WEXITSTATUS(status) {
                    EXIT_OK => NestedUserNsResult::Ok,
                    EXIT_OUTER_FAILED => NestedUserNsResult::OuterFailed { errno: libc::EPERM },
                    EXIT_NESTED_FAILED => NestedUserNsResult::NestedFailed { errno: libc::EPERM },
                    EXIT_EXEC_FAILED => NestedUserNsResult::ExecFailed {
                        errno: libc::ENOENT,
                    },
                    other => {
                        NestedUserNsResult::WaitFailed(format!("unexpected exit code {other}"))
                    }
                }
            } else if libc::WIFSIGNALED(status) {
                NestedUserNsResult::WaitFailed(format!(
                    "child killed by signal {}",
                    libc::WTERMSIG(status)
                ))
            } else {
                NestedUserNsResult::WaitFailed("child neither exited nor signaled".into())
            }
        }
    }
}

fn errno_now() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_ok_no_remediation() {
        let (short, long) = NestedUserNsResult::Ok.explain();
        assert!(short.contains("works"));
        assert!(long.is_none());
    }

    #[test]
    fn explain_outer_failed_mentions_sysctl() {
        let (_, long) = NestedUserNsResult::OuterFailed { errno: 1 }.explain();
        let long = long.expect("remediation");
        assert!(long.contains("unprivileged_userns_clone"));
    }

    #[test]
    fn explain_nested_failed_mentions_no_sandbox_warning() {
        let (_, long) = NestedUserNsResult::NestedFailed { errno: 1 }.explain();
        let long = long.expect("remediation");
        assert!(long.contains("nesting"));
        assert!(long.contains("isolation guarantee"));
    }

    /// We don't unit-test `check()` directly — forking inside cargo's
    /// test runner causes confusing artefacts, and the outcome depends
    /// entirely on the host kernel. Operators invoke it via `havn
    /// doctor` / `havn setup` to verify their own host.
    #[test]
    fn check_fn_compiles() {
        let _: fn() -> NestedUserNsResult = check;
    }
}
