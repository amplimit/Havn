//! PID-1-aware init hooks for the agent runtime (spec §4.1).
//!
//! When the runtime is launched inside a fresh PID namespace by the
//! [`havn_spawner::namespace::NamespaceSpawner`], the kernel makes it PID 1
//! of that namespace. PID 1 has two pieces of special semantics that bite
//! a non-aware process:
//!
//! 1. **Default signal disposition is "ignore"**. Signals like `SIGTERM`
//!    that would normally terminate a process are silently dropped unless
//!    a handler is installed. Without the handler the spawner's graceful
//!    `SIGTERM` step is wasted and the cgroup.kill backstop has to do all
//!    the work. Fixed by [`install_pid_one_handlers_if_needed`].
//! 2. **Orphan reparenting**. Any grandchild whose direct parent dies gets
//!    reparented to PID 1. If PID 1 doesn't `waitpid` them, they pile up as
//!    zombies and eventually hit the cgroup `pids.max` limit, self-DoSing
//!    the agent. The shell tool spawning `sh -c "..."` is the realistic
//!    source of orphans (Chromium spawns ~10 helper processes; gcc forks
//!    the linker; etc.).
//!
//! ## Why the orphan reaper is NOT a synchronous SIGCHLD handler
//!
//! An earlier version installed `waitpid(-1, ..., WNOHANG)` in a libc
//! signal handler. That broke `tokio::process::Command::output()` for
//! any subprocess that itself spawned children: tokio uses `pidfd` to
//! track its direct children, but the kernel still delivers SIGCHLD to
//! the process group when those children exit. Our handler raced
//! tokio, reaped the zombie first via `waitpid(-1)`, and tokio's
//! subsequent `waitid(P_PIDFD, fd, ...)` returned `ECHILD`. The user-
//! facing symptom was `exec failed: No child processes (os error 10)`
//! whenever a shell command's child fanned out (Chromium, gcc, etc.)
//! — found while e2e-testing havn-pack-browser through extra_mounts.
//!
//! Fix: drop the synchronous handler entirely. Reaping happens in
//! [`spawn_orphan_reaper_task`] — a tokio task that listens for
//! SIGCHLD via `tokio::signal::unix::signal(SignalKind::child())`,
//! gives tokio 100 ms to reap its own children through the pidfd path
//! (kernel ≥ 5.3, sub-millisecond in practice), then walks `/proc`
//! and reaps **only** zombies whose `PPid` equals our PID *and* are
//! still zombies after the grace window. That set is, by elimination,
//! true orphans tokio doesn't track.
//!
//! No race with tokio: by the time the reaper looks, any tokio-managed
//! child has already been reaped by tokio's own pidfd plumbing. The
//! reaper's `waitpid(pid, WNOHANG)` is targeted at a specific pid —
//! never `-1` — so we cannot accidentally consume something tokio is
//! about to.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::signal::unix::SignalKind;
use tracing::{debug, warn};

/// Set by the `SIGTERM` handler so the main loop can observe a shutdown
/// request and exit cleanly. `run_reader_loop` (main.rs) polls it via a
/// pinned interval future and breaks out of the frame loop when it flips,
/// letting the runtime exit before the spawner's `cgroup.kill` deadline.
/// The `cgroup.kill` backstop still catches anything stuck mid-flight past
/// the grace window (e.g. a long in-progress LLM call).
pub(crate) static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True iff this process is PID 1 in its (possibly host) PID namespace.
/// Cheap and side-effect-free; safe to call repeatedly.
#[must_use]
pub fn is_pid_one() -> bool {
    // SAFETY: `getpid` is async-signal-safe and infallible.
    (unsafe { libc::getpid() }) == 1
}

/// Install the PID-1 termination signal handlers if (and only if) we're
/// PID 1. Idempotent.
///
/// `SIGTERM` / `SIGINT` / `SIGHUP` → set [`SHUTDOWN_REQUESTED`]; the
/// kernel's mere act of installing a handler restores default-terminate
/// behavior for PID 1 (the kernel only treats unset handlers as "ignore"
/// for PID 1), so once the runtime's own loops finish they exit cleanly.
///
/// **No SIGCHLD handler is installed here.** Orphan reaping happens in
/// [`spawn_orphan_reaper_task`] — see this module's doc comment for why.
pub fn install_pid_one_handlers_if_needed() {
    if !is_pid_one() {
        return;
    }

    // Termination handlers — install for SIGTERM, SIGINT, SIGHUP. We do
    // not set SA_RESTART so any in-flight syscall returns EINTR and the
    // surrounding tokio task can observe the shutdown flag.
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: `sigaction` with a zeroed sigset and a C-ABI handler is
        // the canonical install path; the call is async-signal-safe.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = shutdown_handler as *const () as usize;
            sa.sa_flags = 0;
            libc::sigemptyset(&raw mut sa.sa_mask);
            libc::sigaction(sig, &raw const sa, std::ptr::null_mut());
        }
    }
}

/// Mark shutdown as requested. Async-signal-safe: only an atomic store.
extern "C" fn shutdown_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

/// Spawn a tokio task that reaps reparented orphans without racing
/// `tokio::process::Command`. Call once at runtime startup, after the
/// tokio runtime is constructed but before any tool can spawn a
/// subprocess. No-op when not PID 1 (under `SubprocessSpawner`, where
/// the host's init reaps for us).
///
/// **How it avoids races with tokio.** Tokio's `Command` uses `pidfd`
/// (kernel ≥ 5.3) to track and reap its direct children, sub-millisecond
/// after exit. The reaper here:
///
/// 1. Awaits a SIGCHLD via `tokio::signal::unix::signal(SignalKind::child())`
///    — same kernel signal tokio sees, but in our own listener (multiple
///    listeners are fine; each gets a copy).
/// 2. Sleeps 100 ms — generous time for tokio's pidfd path to consume any
///    of its tracked children.
/// 3. Walks `/proc`, identifies pids whose `State:` is `Z` and whose
///    `PPid:` equals our pid. Anything tokio tracked has been gone for
///    ~100 ms, so what remains here is, by elimination, a true orphan.
/// 4. Calls `waitpid(pid, ..., WNOHANG)` on each — *targeted at the
///    specific pid*, never `-1`. If tokio reaps between our /proc read
///    and the syscall, `waitpid` returns -1 ECHILD which we ignore.
///
/// Worst-case zombie lifetime is ~100 ms (one SIGCHLD round-trip plus
/// the grace window). pids.max accounting tolerates that easily.
pub fn spawn_orphan_reaper_task() {
    if !is_pid_one() {
        return;
    }
    tokio::spawn(orphan_reaper_loop());
}

/// Grace window before scanning. Long enough for tokio's pidfd reaping
/// to land (sub-millisecond in practice on Linux ≥ 5.3); short enough
/// that orphan zombies don't accumulate against the cgroup `pids.max`.
const REAPER_GRACE: Duration = Duration::from_millis(100);

async fn orphan_reaper_loop() {
    let our_pid = unsafe { libc::getpid() };
    let mut sig = match tokio::signal::unix::signal(SignalKind::child()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not register SIGCHLD listener — orphan zombies will accumulate");
            return;
        }
    };
    debug!("orphan reaper task started (PID 1)");
    while sig.recv().await.is_some() {
        tokio::time::sleep(REAPER_GRACE).await;
        let reaped = reap_orphan_zombies(our_pid);
        if reaped > 0 {
            debug!(count = reaped, "reaped orphan zombies");
        }
    }
}

/// Synchronous /proc walker. Returns the number of zombies actually
/// reaped (vs. observed-and-vanished, which we don't count).
fn reap_orphan_zombies(our_pid: i32) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let Ok(name_str) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<i32>() else {
            continue;
        };
        if pid == our_pid || !is_zombie_with_ppid(pid, our_pid) {
            continue;
        }
        // Targeted waitpid. If a concurrent reaper (tokio, in theory)
        // gets there first, this returns -1 ECHILD which we ignore —
        // never `-1` as the pid argument so we can't reap the wrong
        // process.
        // SAFETY: `pid` is i32-clean (parsed from /proc dir name);
        // `status` is a valid stack pointer; `WNOHANG` is the canonical
        // non-blocking flag.
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if r > 0 {
            count += 1;
        }
    }
    count
}

/// Read `/proc/<pid>/status` and answer "is this a zombie whose direct
/// parent is `expected_ppid`?". Lines matter: `State:` and `PPid:` are
/// the only two we look at; everything else is skipped.
fn is_zombie_with_ppid(pid: i32, expected_ppid: i32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        // ESRCH or EACCES — pid vanished or we lack permission. Either
        // way, nothing for us to reap.
        return false;
    };
    let mut zombie = false;
    let mut ppid_match = false;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            // Format: "State:\tZ (zombie)" — letter is the first non-
            // whitespace char of the value.
            zombie = rest.trim_start().starts_with('Z');
        } else if let Some(rest) = line.strip_prefix("PPid:") {
            ppid_match = rest.trim().parse::<i32>().ok() == Some(expected_ppid);
        }
    }
    zombie && ppid_match
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_pid_one_returns_false_in_test_runner() {
        // Test runner is never PID 1 on any reasonable host. This pins the
        // contract — if a future change makes `is_pid_one` always return
        // true, the install function would clobber the cargo test runner's
        // signal handlers and tests would mysteriously hang.
        assert!(!is_pid_one());
    }

    #[test]
    fn install_is_a_noop_when_not_pid_one() {
        // Must be safe to call repeatedly when not PID 1 — exercised on
        // every runtime startup under SubprocessSpawner.
        install_pid_one_handlers_if_needed();
        install_pid_one_handlers_if_needed();
        assert!(!SHUTDOWN_REQUESTED.load(Ordering::Relaxed));
    }

    #[test]
    fn spawn_orphan_reaper_task_is_a_noop_when_not_pid_one() {
        // Same posture as install — under SubprocessSpawner this gets
        // called every runtime startup and must not spawn anything.
        // We can't assert "no task spawned" directly, but we can assert
        // it returns synchronously (no panic, no hang) and that
        // is_pid_one is false (so the early-return path was taken).
        spawn_orphan_reaper_task();
        assert!(!is_pid_one());
    }

    #[test]
    fn is_zombie_with_ppid_self_check_returns_false() {
        // Test runner pid is not its own parent and not a zombie.
        let me = unsafe { libc::getpid() };
        assert!(!is_zombie_with_ppid(me, me));
    }

    #[test]
    fn is_zombie_with_ppid_handles_missing_pid() {
        // PID 0x7fff_fffe is reserved and never exists.
        let absent_pid = 0x7fff_fffe;
        let me = unsafe { libc::getpid() };
        assert!(!is_zombie_with_ppid(absent_pid, me));
    }

    /// End-to-end orphan-reaping smoke test: fork a grandchild that
    /// outlives its parent (a true orphan), verify the parser sees it
    /// as a zombie with PPid == us, then `waitpid` it.
    ///
    /// Trick: a regular cargo process isn't PID 1, so orphans normally
    /// reparent to the host's init, not to us. We set
    /// `PR_SET_CHILD_SUBREAPER` first, which makes us the nearest
    /// subreaper for our descendants — orphans land here instead.
    /// That's the same posture the runtime has when it's actually PID 1
    /// in an agent namespace (PID 1 is implicitly a subreaper).
    ///
    /// Linux-only because PR_SET_CHILD_SUBREAPER is a Linux prctl.
    #[cfg(target_os = "linux")]
    #[test]
    fn reap_orphan_zombies_consumes_a_real_orphan() {
        // Become a subreaper so orphans of our descendants come to us
        // instead of host init. PR_SET_CHILD_SUBREAPER is process-wide
        // and idempotent.
        // SAFETY: prctl(PR_SET_CHILD_SUBREAPER, 1) takes a single ulong
        // arg and never touches user memory.
        let r = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1_u64, 0_u64, 0_u64, 0_u64) };
        assert_eq!(
            r,
            0,
            "PR_SET_CHILD_SUBREAPER failed: {}",
            std::io::Error::last_os_error()
        );

        let our_pid = unsafe { libc::getpid() };
        // Fork a child that immediately forks AGAIN and exits, leaving
        // the grandchild orphaned. With the subreaper bit set, the
        // grandchild's PPid becomes us (not host init).
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // First-level child: fork the grandchild and exit immediately.
            let grand = unsafe { libc::fork() };
            if grand == 0 {
                // Grandchild: pause briefly, then exit.
                unsafe {
                    libc::nanosleep(
                        &libc::timespec {
                            tv_sec: 0,
                            tv_nsec: 50_000_000,
                        } as *const _,
                        std::ptr::null_mut(),
                    );
                    libc::_exit(0);
                }
            }
            unsafe { libc::_exit(0) }; // Parent of grandchild dies → reparent.
        }
        // Reap the first-level child to consume its zombie ourselves.
        let mut s: libc::c_int = 0;
        unsafe { libc::waitpid(pid, &raw mut s, 0) };

        // Wait for the grandchild to exit and surface as a PPid==us zombie.
        let mut found = false;
        for _ in 0..50 {
            // up to 1 s
            std::thread::sleep(Duration::from_millis(20));
            let count = reap_orphan_zombies(our_pid);
            if count > 0 {
                found = true;
                break;
            }
        }
        assert!(found, "expected to reap at least one orphan grandchild");
    }
}
