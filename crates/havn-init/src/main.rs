//! `havn-init` — PID-1 shim for the agent runtime (spec §4.1).
//!
//! The spawner exec's this binary with the runtime path + runtime args:
//!
//! ```text
//! havn-init /path/to/havn-runtime --agent-id ... --gateway-socket ... --workspace ...
//! ```
//!
//! At this point the spawner's `pre_exec` has already entered fresh USER /
//! MNT / NET namespaces and unshared a PID namespace. The kernel rule for
//! `unshare(CLONE_NEWPID)` is "the next fork lands the child in the new
//! PID ns" — so we fork once here:
//!
//! - **child** (PID 1 in the new PID ns): builds a minimal read-only
//!   rootfs via [`ns_setup::pivot_root_into_minimal_rootfs`] (binds /usr,
//!   symlinks /lib, /lib64, /bin, /sbin → /usr/*, mounts a fresh `/proc`
//!   inside the new ns, mirror-binds the agent's workspace at the same
//!   absolute path the runtime was told, optionally surfaces
//!   `/etc/resolv.conf` when network is allowed), `pivot_root`s into it,
//!   applies the agent's Landlock rules (passed by the spawner in
//!   `HAVN_LANDLOCK_RW` / `HAVN_LANDLOCK_RO`) so that every thread of the
//!   runtime and every process its tools start inherits them, then exec's
//!   the runtime. The runtime IS PID 1 — it installs its own
//!   SIGCHLD reaper / shutdown handlers (see `havn-runtime/src/init.rs`).
//! - **parent** (still in host PID ns): forwards `SIGTERM` / `SIGINT` /
//!   `SIGHUP` to the inner runtime so the spawner's graceful-stop path
//!   reaches it, blocks in `waitpid`, and `_exit`s with the runtime's exit
//!   code. Never returns.
//!
//! Both sides die with their parent: the spawner sets `PR_SET_PDEATHSIG`
//! (SIGKILL) for this process before exec, and the child sets it again
//! towards the parent, so a dead gateway takes the whole agent with it.
//!
//! Why a separate binary instead of doing the fork in `pre_exec`:
//! `std::process::Command::spawn` uses a cloexec pipe to detect exec
//! success — if `pre_exec` forks and the post-fork parent never exec's,
//! the pipe never closes and the gateway's `spawn()` blocks until the
//! runtime exits. By doing the fork inside an exec'd binary, the cloexec
//! pipe closes on `havn-init`'s own exec and the gateway proceeds
//! normally.

#![cfg_attr(not(unix), allow(unused))]
// `havn-init` is a startup shim with no logging stack — diagnostics go to
// stderr via raw `eprintln!`, which the gateway captures into the agent's
// log. The workspace lint denies `print_stderr` for normal libraries; this
// binary is the legitimate exception.
#![allow(clippy::print_stderr)]

use std::ffi::OsString;
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
use havn_spawner::ns_setup;

fn main() {
    #[cfg(unix)]
    {
        let mut argv = std::env::args_os();
        let _self_name = argv.next();
        let Some(runtime_binary) = argv.next() else {
            eprintln!(
                "havn-init: missing runtime binary argv\nusage: havn-init <runtime-path> [runtime args...]"
            );
            std::process::exit(2);
        };
        let runtime_binary: OsString = runtime_binary;
        let runtime_args: Vec<OsString> = argv.collect();

        // Pull the workspace + gateway socket paths out of the runtime
        // args so we can mirror-bind them into the new rootfs. Without
        // these, the runtime's --workspace and --gateway-socket
        // arguments would point at paths that don't exist post-pivot.
        let workspace = parse_value_arg(&runtime_args, "--workspace").unwrap_or_else(|| {
            eprintln!(
                "havn-init: missing --workspace <path> in runtime args; rootfs build will fail"
            );
            PathBuf::from("/")
        });
        let gateway_socket = parse_value_arg(&runtime_args, "--gateway-socket").unwrap_or_else(
            || {
                eprintln!(
                    "havn-init: missing --gateway-socket <path> in runtime args; runtime will fail to connect"
                );
                PathBuf::from("/dev/null")
            },
        );

        // Network policy bit (spec §6.3 v0.7). Spawner sets
        // HAVN_NETWORK_OUTBOUND=1 only when policy.network_policy
        // .egress_allowed is true; absent / "0" means the agent ns
        // stays empty (no /etc/resolv.conf, no /etc/ssl bind). Mirrors
        // the gate the spawner uses for `configure_agent_network`,
        // keeping the two layers in sync — operator changing one and
        // not the other shouldn't be possible because they're driven
        // by the same policy bit.
        let network_allowed = matches!(
            std::env::var("HAVN_NETWORK_OUTBOUND").as_deref(),
            Ok("1") | Ok("true")
        );
        // Operator-configured DNS for the agent ns when egress is on
        // (spec §6.3 v0.7). Comma-separated list. Empty/unset means
        // we don't write a curated resolv.conf — DNS will fail unless
        // the operator's network setup somehow provides one. Gateway
        // defaults this to public resolvers, so empty is a misconfig
        // signal worth surfacing.
        let agent_dns: Vec<String> = std::env::var("HAVN_AGENT_DNS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        // Operator-declared extra bind-mounts (spec §4.1 v0.7). The
        // spawner sets `HAVN_EXTRA_MOUNTS` from the gateway's
        // `[[extra_mounts]]` config; we parse it via the spawner's
        // shared codec. Malformed env values surface as a stderr line
        // and proceed with an empty list (the agent then can't see the
        // pack binaries — visible in tool_result `command not found`,
        // not catastrophic at spawn).
        let extra_mounts = match std::env::var("HAVN_EXTRA_MOUNTS") {
            Ok(s) => havn_spawner::decode_extra_mounts(&s).unwrap_or_else(|e| {
                eprintln!("havn-init: HAVN_EXTRA_MOUNTS malformed ({e}); skipping all");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };

        // Operator-declared in-namespace tmpfs mounts (spec §16.2).
        // Same env-var transport pattern as HAVN_EXTRA_MOUNTS. Malformed
        // input → empty list and a stderr line; the workload that
        // needed the tmpfs will see "filesystem not mounted" later but
        // the agent itself still spawns.
        let tmpfs_mounts = match std::env::var("HAVN_TMPFS_MOUNTS") {
            Ok(s) => havn_spawner::decode_tmpfs_mounts(&s).unwrap_or_else(|e| {
                eprintln!("havn-init: HAVN_TMPFS_MOUNTS malformed ({e}); skipping all");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };

        // Landlock lists from the spawner. Without them the agent would
        // only get the runtime's own, single-thread rules, so refuse.
        let landlock_rules = match (
            std::env::var(havn_spawner::filesystem_rules::RW_ENV),
            std::env::var(havn_spawner::filesystem_rules::RO_ENV),
        ) {
            (Ok(rw), Ok(ro)) => (
                havn_spawner::filesystem_rules::decode(&rw),
                havn_spawner::filesystem_rules::decode(&ro),
            ),
            _ => {
                eprintln!(
                    "havn-init: {} / {} not set (havn-init and the gateway are from different \
                     releases?); refusing to start the agent",
                    havn_spawner::filesystem_rules::RW_ENV,
                    havn_spawner::filesystem_rules::RO_ENV,
                );
                std::process::exit(1);
            }
        };

        // SAFETY: caller (NamespaceSpawner) has already done
        //   unshare(CLONE_NEWUSER|NEWNS|NEWNET|NEWPID),
        //   write_user_namespace_maps, and mark_mounts_private. The next
        //   fork will land the child in the new PID ns as PID 1.
        let runtime_path = PathBuf::from(&runtime_binary);
        let exit_code = unsafe {
            fork_and_exec_runtime(
                &runtime_binary,
                &runtime_args,
                &workspace,
                &runtime_path,
                &gateway_socket,
                network_allowed,
                &extra_mounts,
                &tmpfs_mounts,
                &agent_dns,
                &landlock_rules,
            )
        };
        // SAFETY: `_exit` is async-signal-safe and never returns.
        unsafe { libc::_exit(exit_code) };
    }

    #[cfg(not(unix))]
    {
        eprintln!("havn-init: unix-only");
        std::process::exit(2);
    }
}

/// Fork into the new PID namespace; the child mounts `/proc` and exec's
/// the runtime; the parent installs the signal forwarder, blocks in
/// `waitpid`, and returns the runtime's exit code (or `128 + signal` if
/// the runtime was killed by a signal). Never returns to the caller from
/// the parent path.
///
/// # Safety
///
/// Must run after the spawner's `pre_exec` has unshared the PID
/// namespace; otherwise the child lands in the host PID ns and the
/// `mount_proc` call would shadow the host's `/proc`.
#[cfg(unix)]
unsafe fn fork_and_exec_runtime(
    runtime_binary: &OsString,
    runtime_args: &[OsString],
    workspace: &Path,
    runtime_path: &Path,
    gateway_socket: &Path,
    network_allowed: bool,
    extra_mounts: &[havn_spawner::ExtraMount],
    tmpfs_mounts: &[havn_spawner::TmpfsMount],
    agent_dns: &[String],
    landlock_rules: &(Vec<PathBuf>, Vec<PathBuf>),
) -> i32 {
    let lifeline = match ns_setup::ParentLifeline::new() {
        Ok(lifeline) => lifeline,
        Err(e) => {
            eprintln!("havn-init: pipe failed: {e}");
            return 1;
        }
    };
    // SAFETY: `fork` is async-signal-safe; we branch immediately on the
    // documented return values without touching any inherited mutex.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => {
            let err = std::io::Error::last_os_error();
            eprintln!("havn-init: fork failed: {err}");
            lifeline.close();
            1
        }
        0 => {
            // The runtime must not outlive this reaper: if the reaper is
            // killed (with the gateway, or by hand), so is the runtime and,
            // with it as PID 1, everything in its namespace.
            // The lifeline covers a reaper that died before the prctl.
            if let Err(e) = ns_setup::kill_with_parent(None) {
                eprintln!("havn-init: PR_SET_PDEATHSIG failed: {e}");
                // SAFETY: `_exit` is async-signal-safe.
                unsafe { libc::_exit(1) };
            }
            if !lifeline.parent_alive() {
                eprintln!("havn-init: reaper exited before the runtime started");
                // SAFETY: `_exit` is async-signal-safe.
                unsafe { libc::_exit(1) };
            }
            // Child: PID 1 in the new PID ns. Build the minimal rootfs
            // (mounts /proc inside it as part of the operation), pivot
            // into it, then exec the runtime.
            //
            // SAFETY: ns_setup::pivot_root_into_minimal_rootfs requires
            // being PID 1 in a fresh PID/MNT/USER ns — exactly our state
            // after the parent's unshare + this fork.
            if let Err(e) = unsafe {
                ns_setup::pivot_root_into_minimal_rootfs(
                    workspace,
                    runtime_path,
                    gateway_socket,
                    network_allowed,
                    extra_mounts,
                    tmpfs_mounts,
                    agent_dns,
                )
            } {
                eprintln!("havn-init: pivot_root setup failed: {e}");
                // SAFETY: `_exit` is async-signal-safe.
                unsafe { libc::_exit(1) };
            }

            // Confine this process before exec: Landlock is inherited by
            // every thread the runtime starts and every process its tools
            // run, which a call from inside the multi-threaded runtime
            // would not achieve.
            let (rw, ro) = landlock_rules;
            apply_landlock_or_exit(rw, ro);
            // The agent's uid 0 is the gateway's host uid: without this
            // it would run with a full capability set in its namespaces.
            // SAFETY: this forked child is single-threaded.
            if let Err(e) = unsafe { ns_setup::drop_all_capabilities() } {
                eprintln!("havn-init: dropping capabilities failed: {e}");
                // SAFETY: `_exit` is async-signal-safe.
                unsafe { libc::_exit(1) };
            }
            let mut command = std::process::Command::new(runtime_binary);
            command
                .args(runtime_args)
                .env_remove(havn_spawner::filesystem_rules::RW_ENV)
                .env_remove(havn_spawner::filesystem_rules::RO_ENV)
                .env(havn_spawner::filesystem_rules::APPLIED_ENV, "1");

            // exec the runtime. We use `std::process::Command` here for
            // ergonomics — we're past pre_exec land and a normal exec is
            // fine in this context (no parent thread state to worry about).
            let err = command.exec_replace();
            let display = runtime_binary.to_string_lossy();
            eprintln!("havn-init: exec runtime {display} failed: {err}");
            // SAFETY: `_exit` is async-signal-safe.
            unsafe { libc::_exit(1) };
        }
        child_pid => {
            lifeline.hold();
            // Parent: install signal forwarder so spawner.stop() reaches
            // the inner runtime, then block in waitpid for the inner.
            // SAFETY: ns_setup::install_signal_forwarder writes a static
            // atomic and calls sigaction; both async-signal-safe.
            unsafe { ns_setup::install_signal_forwarder_for(child_pid) };
            ns_setup::wait_for_child_exit_code(child_pid)
        }
    }
}

/// Apply the agent's Landlock rules, or exit: an agent never runs without
/// them unless the operator set HAVN_ALLOW_NO_LANDLOCK=1.
#[cfg(target_os = "linux")]
fn apply_landlock_or_exit(rw: &[PathBuf], ro: &[PathBuf]) {
    use havn_spawner::filesystem_rules::{Enforcement, apply};
    let allow_without = std::env::var("HAVN_ALLOW_NO_LANDLOCK").as_deref() == Ok("1");
    match apply(rw, ro) {
        Ok(Enforcement::Full) => {}
        Ok(Enforcement::Partial) => {
            eprintln!(
                "havn-init: Landlock only partially enforced (the kernel supports a subset of \
                 the requested access rights)"
            );
        }
        Ok(Enforcement::None) if allow_without => {
            eprintln!(
                "havn-init: Landlock not enforced; HAVN_ALLOW_NO_LANDLOCK=1, starting the agent \
                 without filesystem restriction"
            );
        }
        Ok(Enforcement::None) => {
            eprintln!(
                "havn-init: Landlock is not enforced on this kernel; refusing to start the agent \
                 (set HAVN_ALLOW_NO_LANDLOCK=1 to run it without filesystem restriction)"
            );
            // SAFETY: `_exit` is async-signal-safe.
            unsafe { libc::_exit(1) };
        }
        Err(e) => {
            eprintln!("havn-init: applying Landlock failed: {e}");
            // SAFETY: `_exit` is async-signal-safe.
            unsafe { libc::_exit(1) };
        }
    }
}

/// Pull the value of `--<flag> <VALUE>` out of the runtime arg vector.
/// We don't parse the rest — the runtime owns its full clap surface.
#[cfg(unix)]
fn parse_value_arg(args: &[OsString], flag: &str) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(PathBuf::from);
        }
    }
    None
}

#[cfg(unix)]
trait CommandExecReplace {
    /// Replace this process via `execvp`. Only returns on failure.
    fn exec_replace(&mut self) -> std::io::Error;
}

#[cfg(unix)]
impl CommandExecReplace for std::process::Command {
    fn exec_replace(&mut self) -> std::io::Error {
        use std::os::unix::process::CommandExt as _;
        // `exec` returns `io::Error` on failure and never returns on success.
        self.exec()
    }
}
