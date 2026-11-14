//! seccomp-bpf syscall blocklist for the runtime process (spec §4.1).
//!
//! ## Why blocklist, not whitelist
//!
//! Spec §4.1 describes "syscall whitelist" — the *intent* is to forbid a
//! specific set of dangerous syscalls. A literal whitelist is impractical:
//! Rust + tokio + sqlx + reqwest issue several hundred different syscalls
//! across glibc versions and architectures, and an incomplete whitelist
//! crashes the runtime on its first uncovered syscall in production.
//!
//! What we actually want is the security property: **the agent cannot
//! call `mount`, `ptrace`, `bpf`, `kexec_load`, `init_module`, …**. A blocklist
//! delivers that with bounded review surface and is what every production
//! container runtime ships (Docker / containerd / Podman default profiles
//! all blocklist a similar set). Default action is `Allow`; listed syscalls
//! get `KillProcess`.
//!
//! ## When it applies
//!
//! Called once in [`super::main`] after Landlock is in place. Runs late
//! enough that startup syscalls (file I/O for bootstrap files, cgroup
//! attach, socket connect) have completed. Installed on every thread of
//! the process at once (TSYNC), since the tokio workers that run tools
//! already exist, and inherited across `execve` and `fork` so subprocesses
//! spawned by the `shell` tool get the same restrictions.
//!
//! ## What's in the list
//!
//! Spec-derived (§4.1): `mount`, `umount`, `reboot`, `kexec_load`, `ptrace`,
//! `init_module`. Extended with the canonical "dangerous syscalls" set
//! every mainstream container runtime blocks:
//!
//! - mount-table manipulation (`mount`, `umount2`, `pivot_root`, `chroot`, and
//!   the new mount API: `fsopen`, `fsconfig`, `fsmount`, `fspick`,
//!   `move_mount`, `open_tree`, `open_tree_attr`, `mount_setattr`)
//! - kernel-module loading (`init_module`, `finit_module`, `delete_module`)
//! - kernel reboot / replacement (`reboot`, `kexec_load`, `kexec_file_load`)
//! - process inspection / injection (`ptrace`, `process_vm_readv`,
//!   `process_vm_writev`)
//! - BPF and perf hooks (`bpf`, `perf_event_open`)
//! - swap, syslog, time, sysctl (`swapon`, `swapoff`, `syslog`,
//!   `settimeofday`, `clock_settime`, `clock_adjtime`, `adjtimex`,
//!   `_sysctl`)
//! - misc local-privilege escalation footguns (`modify_ldt`,
//!   `userfaultfd`, `lookup_dcookie`, `vmsplice`, `quotactl`)
//!
//! New user namespaces are refused too, with `EPERM` rather than a kill
//! ([`user_namespace_guard`]), unless the operator allows them with
//! [`USER_NAMESPACES`].

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use thiserror::Error;
use tracing::info;

#[cfg(target_arch = "x86_64")]
const TARGET_ARCH: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
const TARGET_ARCH: TargetArch = TargetArch::aarch64;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("havn-runtime seccomp filter requires x86_64 or aarch64");

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SeccompError {
    #[error("building seccomp filter: {0}")]
    Build(String),
    #[error("compiling BPF program: {0}")]
    Compile(String),
    #[error("applying BPF program: {0}")]
    Apply(String),
    #[error(
        "operator-declared seccomp.allow_extra contains unknown or unrelaxable syscall {0:?}; \
         see havn-runtime::seccomp::OPERATOR_RELAXABLE_SYSCALLS for the curated list"
    )]
    UnknownSyscall(String),
}

/// Apply the syscall blocklist to the running process. Idempotent and
/// inherited across `fork` / `execve`. Calling this twice in a process is
/// permitted by the kernel — the resulting filter chain is intersected.
///
/// `allow_extra` lets the operator relax the blocklist by syscall name
/// (spec §16.2). Names are matched against the static
/// [`OPERATOR_RELAXABLE_SYSCALLS`] table — a curated subset of names the
/// operator is allowed to permit. Unknown / unrelaxable names refuse
/// the call with `SeccompError::UnknownSyscall`. When that happens the
/// caller (`main.rs`) currently logs and continues with the **full**
/// blocklist (fail-closed) — better an over-restrictive runtime than
/// silently honouring a typo.
pub fn restrict_syscalls(allow_extra: &[String]) -> Result<(), SeccompError> {
    let blocked = effective_blocklist(allow_extra)?;
    let user_namespaces = allow_extra.iter().any(|n| n.trim() == USER_NAMESPACES);
    let mut rules = BTreeMap::new();
    for syscall in &blocked {
        rules.insert(*syscall, vec![match_any_rule()?]);
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,       // syscalls NOT in the blocklist run normally
        SeccompAction::KillProcess, // listed syscalls hard-kill the agent
        TARGET_ARCH,
    )
    .map_err(|e| SeccompError::Build(e.to_string()))?;

    let program: BpfProgram = filter
        .try_into()
        .map_err(|e| SeccompError::Compile(format!("{e:?}")))?;

    // TSYNC: install on every thread of the process (the tokio workers
    // already exist), not only the calling one; it also sets
    // no_new_privs on all of them.
    seccompiler::apply_filter_all_threads(&program)
        .map_err(|e| SeccompError::Apply(e.to_string()))?;
    #[cfg(target_arch = "x86_64")]
    seccompiler::apply_filter_all_threads(&x32_guard())
        .map_err(|e| SeccompError::Apply(e.to_string()))?;
    if !user_namespaces {
        for program in user_namespace_guard()? {
            seccompiler::apply_filter_all_threads(&program)
                .map_err(|e| SeccompError::Apply(e.to_string()))?;
        }
    }

    info!(
        blocked = blocked.len(),
        relaxed = allow_extra.len(),
        user_namespaces,
        "seccomp filter installed"
    );
    Ok(())
}

/// x32 system calls report the x86_64 audit architecture with bit 30 set
/// in the number, so the blocklist above (which matches exact numbers)
/// would not see them. This filter kills any such call. It runs before
/// the kernel looks the number up, so it applies whether or not the
/// kernel has x32 support.
#[cfg(target_arch = "x86_64")]
fn x32_guard() -> BpfProgram {
    use seccompiler::sock_filter;
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const RET_K: u16 = 0x06;
    const ARCH_OFFSET: u32 = 4;
    const NR_OFFSET: u32 = 0;
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    let insn = |code, jt, jf, k| sock_filter { code, jt, jf, k };
    vec![
        insn(LD_W_ABS, 0, 0, ARCH_OFFSET),
        // Not x86_64 (the main filter kills those): allow.
        insn(JEQ_K, 0, 2, AUDIT_ARCH_X86_64),
        insn(LD_W_ABS, 0, 0, NR_OFFSET),
        insn(JGE_K, 1, 0, X32_SYSCALL_BIT),
        insn(RET_K, 0, 0, RET_ALLOW),
        insn(RET_K, 0, 0, RET_KILL_PROCESS),
    ]
}

/// `[seccomp] allow_extra` entry that lets the agent create user
/// namespaces, for capability packs whose workload nests them (Chromium's
/// sandbox).
pub const USER_NAMESPACES: &str = "user_namespaces";

/// `clone3` (Linux 5.3): the same number on x86_64 and aarch64.
const SYS_CLONE3: i64 = 435;

/// Filters that refuse new user namespaces. A process gets every
/// capability in a user namespace it creates, which opens kernel code
/// otherwise reserved to privileged callers (nf_tables, other namespaced
/// subsystems) and a long line of local privilege escalations to the
/// agent's tools. `unshare` and `clone` with `CLONE_NEWUSER` fail with
/// `EPERM`, which callers that try a user namespace expect. `clone3`
/// takes its flags in memory seccomp cannot read, so it fails with
/// `ENOSYS`, on which glibc falls back to `clone`.
fn user_namespace_guard() -> Result<Vec<BpfProgram>, SeccompError> {
    let new_user = || -> Result<SeccompRule, SeccompError> {
        let flag = u64::try_from(libc::CLONE_NEWUSER)
            .map_err(|e| SeccompError::Build(format!("CLONE_NEWUSER: {e}")))?;
        let cond = SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(flag),
            flag,
        )
        .map_err(|e| SeccompError::Build(format!("condition: {e:?}")))?;
        SeccompRule::new(vec![cond]).map_err(|e| SeccompError::Build(format!("rule: {e:?}")))
    };
    let errno = |code: i32| {
        u32::try_from(code)
            .map(SeccompAction::Errno)
            .map_err(|e| SeccompError::Build(e.to_string()))
    };
    let refuse = SeccompFilter::new(
        BTreeMap::from([
            (libc::SYS_unshare, vec![new_user()?]),
            (libc::SYS_clone, vec![new_user()?]),
        ]),
        SeccompAction::Allow,
        errno(libc::EPERM)?,
        TARGET_ARCH,
    )
    .map_err(|e| SeccompError::Build(e.to_string()))?;
    let hide_clone3 = SeccompFilter::new(
        BTreeMap::from([(SYS_CLONE3, vec![match_any_rule()?])]),
        SeccompAction::Allow,
        errno(libc::ENOSYS)?,
        TARGET_ARCH,
    )
    .map_err(|e| SeccompError::Build(e.to_string()))?;
    [refuse, hide_clone3]
        .into_iter()
        .map(|filter| {
            BpfProgram::try_from(filter).map_err(|e| SeccompError::Compile(format!("{e:?}")))
        })
        .collect()
}

/// Build the blocklist with operator-declared relaxations applied.
/// Pure function — separable for unit tests, no kernel calls.
fn effective_blocklist(allow_extra: &[String]) -> Result<Vec<i64>, SeccompError> {
    if allow_extra.is_empty() {
        return Ok(DANGEROUS_SYSCALLS.to_vec());
    }
    let mut to_remove: Vec<i64> = Vec::with_capacity(allow_extra.len());
    for name in allow_extra {
        let trimmed = name.trim();
        if trimmed == USER_NAMESPACES {
            continue;
        }
        let id = OPERATOR_RELAXABLE_SYSCALLS
            .iter()
            .find(|(n, _)| *n == trimmed)
            .map(|(_, id)| *id)
            .ok_or_else(|| SeccompError::UnknownSyscall(trimmed.to_string()))?;
        to_remove.push(id);
    }
    Ok(DANGEROUS_SYSCALLS
        .iter()
        .copied()
        .filter(|s| !to_remove.contains(s))
        .collect())
}

/// Curated subset of blocked syscalls operators are allowed to relax
/// (spec §16.2). Names are matched against `[seccomp.allow_extra]` in
/// gateway config. Each entry must already be present in
/// [`DANGEROUS_SYSCALLS`] — a syscall not blocked by default cannot be
/// "relaxed" (it's already allowed); listing one here would be dead
/// data.
///
/// **Curated**, not "every name in DANGEROUS_SYSCALLS": deliberately
/// keeps the operator's relaxation surface narrower than the full
/// blocklist. `mount` / `init_module` / `kexec_load` etc. are NOT
/// relaxable — even an operator deliberately trying to make a hostile
/// pack cannot let an agent insert a kernel module. Today the only
/// motivating case is browser packs needing Chromium's user-namespace
/// sandbox (`pivot_root`, `userfaultfd`, along with [`USER_NAMESPACES`],
/// which is not a syscall name); add new entries only when a
/// concrete pack requires them and the security impact is well
/// understood.
const OPERATOR_RELAXABLE_SYSCALLS: &[(&str, i64)] = &[
    ("pivot_root", libc::SYS_pivot_root),
    ("userfaultfd", libc::SYS_userfaultfd),
];

/// Build a [`SeccompRule`] that matches every invocation of its syscall,
/// regardless of arguments. seccompiler 0.5 rejects rules with zero
/// conditions, so we use a tautological one (argument 0's low 32 bits
/// `>= 0`, which is always true for `u32`).
fn match_any_rule() -> Result<SeccompRule, SeccompError> {
    let cond = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ge, 0)
        .map_err(|e| SeccompError::Build(format!("condition: {e:?}")))?;
    SeccompRule::new(vec![cond]).map_err(|e| SeccompError::Build(format!("rule: {e:?}")))
}

/// `libc` exposes `SYS_kexec_file_load` for x86_64 but omits it for the
/// aarch64 *musl* target — a gap in the crate's musl syscall tables, even
/// though the kernel implements the call (generic uapi `__NR_kexec_file_load`
/// = 294). Supply the number directly on aarch64 so the block still applies;
/// dropping it would silently weaken the boundary on exactly the arch we
/// build statically for.
#[cfg(target_arch = "aarch64")]
const SYS_KEXEC_FILE_LOAD: i64 = 294;
#[cfg(not(target_arch = "aarch64"))]
const SYS_KEXEC_FILE_LOAD: i64 = libc::SYS_kexec_file_load;

/// `open_tree_attr` (Linux 6.15) is open_tree plus mount_setattr in one call;
/// libc has no constant for it yet. The number is the same on x86_64 and
/// aarch64.
const SYS_OPEN_TREE_ATTR: i64 = 467;

/// Syscalls that hard-kill the agent process if invoked. Listed by
/// architecture-portable `libc::SYS_*` constants where the kernel name
/// matches across `x86_64` + `aarch64`. Any syscall not in this list runs
/// without seccomp interference (default Allow).
const DANGEROUS_SYSCALLS: &[i64] = &[
    // ── mount-table manipulation ─────────────────────────────────────
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    // The Linux 5.2+ mount API reaches the same ground as mount(2).
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_mount_setattr,
    SYS_OPEN_TREE_ATTR,
    // ── kernel modules ──────────────────────────────────────────────
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    // ── kernel reboot / replacement ─────────────────────────────────
    libc::SYS_reboot,
    libc::SYS_kexec_load,
    SYS_KEXEC_FILE_LOAD,
    // ── process inspection / injection ──────────────────────────────
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    // ── BPF + perf hooks ────────────────────────────────────────────
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    // ── swap, syslog, time, sysctl ──────────────────────────────────
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_syslog,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    // ── local-privilege-escalation footguns ─────────────────────────
    libc::SYS_userfaultfd,
    libc::SYS_quotactl,
    // ── fd-handle escapes (re-open via handle bypasses LSM checks) ──
    libc::SYS_name_to_handle_at,
    libc::SYS_open_by_handle_at,
];

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn spec_named_syscalls_are_blocked() {
        // Spec §4.1 explicitly names these. Don't let an edit silently drop
        // any of them — that's the whole point of the security boundary.
        for required in &[
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_fsopen,
            libc::SYS_fsmount,
            libc::SYS_move_mount,
            libc::SYS_open_tree,
            libc::SYS_reboot,
            libc::SYS_kexec_load,
            libc::SYS_ptrace,
            libc::SYS_init_module,
        ] {
            assert!(
                DANGEROUS_SYSCALLS.contains(required),
                "spec-mandated syscall {required} missing from blocklist"
            );
        }
    }

    #[test]
    fn dangerous_syscall_list_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for sc in DANGEROUS_SYSCALLS {
            assert!(seen.insert(*sc), "syscall {sc} listed twice");
        }
    }

    #[test]
    fn allow_extra_empty_keeps_full_blocklist() {
        let blocked = effective_blocklist(&[]).expect("ok");
        assert_eq!(blocked.len(), DANGEROUS_SYSCALLS.len());
    }

    #[test]
    fn allow_extra_strips_named_syscalls_only() {
        let blocked =
            effective_blocklist(&["pivot_root".into(), "userfaultfd".into()]).expect("ok");
        assert!(!blocked.contains(&libc::SYS_pivot_root));
        assert!(!blocked.contains(&libc::SYS_userfaultfd));
        // Mount, ptrace, bpf, init_module remain blocked. The relaxable
        // table is intentionally narrow — operators cannot let a pack
        // call `mount` even if they paste it in config.
        assert!(blocked.contains(&libc::SYS_mount));
        assert!(blocked.contains(&libc::SYS_fsmount));
        assert!(blocked.contains(&libc::SYS_open_tree));
        assert!(blocked.contains(&libc::SYS_ptrace));
        assert!(blocked.contains(&libc::SYS_bpf));
        assert!(blocked.contains(&libc::SYS_init_module));
        assert_eq!(blocked.len(), DANGEROUS_SYSCALLS.len() - 2);
    }

    #[test]
    fn allow_extra_rejects_unknown_name() {
        let r = effective_blocklist(&["definitely_not_a_syscall".into()]);
        assert!(matches!(r, Err(SeccompError::UnknownSyscall(_))));
    }

    #[test]
    fn allow_extra_rejects_non_relaxable_syscall() {
        // `mount` is in DANGEROUS_SYSCALLS but NOT in the relaxable
        // table. Operators cannot relax it. This test pins that.
        let r = effective_blocklist(&["mount".into()]);
        assert!(
            matches!(r, Err(SeccompError::UnknownSyscall(ref s)) if s == "mount"),
            "expected UnknownSyscall(mount), got {r:?}"
        );
    }

    #[test]
    fn allow_extra_tolerates_whitespace() {
        let blocked = effective_blocklist(&[" pivot_root ".into()]).expect("ok");
        assert!(!blocked.contains(&libc::SYS_pivot_root));
    }

    #[test]
    fn relaxable_entries_are_actually_blocked() {
        // Sanity: every entry in OPERATOR_RELAXABLE_SYSCALLS must be in
        // DANGEROUS_SYSCALLS. A name not blocked by default cannot be
        // "relaxed" — listing one would be dead config.
        for (name, id) in OPERATOR_RELAXABLE_SYSCALLS {
            assert!(
                DANGEROUS_SYSCALLS.contains(id),
                "{name} is in OPERATOR_RELAXABLE_SYSCALLS but not in DANGEROUS_SYSCALLS"
            );
        }
    }

    #[test]
    fn filter_compiles_to_a_bpf_program() {
        // Sanity check: the static blocklist must produce a valid BPF
        // program. We don't apply it (would kill the test process if a
        // listed syscall fires elsewhere in the test runner).
        let mut rules = BTreeMap::new();
        for syscall in DANGEROUS_SYSCALLS {
            rules.insert(*syscall, vec![match_any_rule().expect("rule")]);
        }
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::KillProcess,
            TARGET_ARCH,
        )
        .expect("filter");
        let _: BpfProgram = filter.try_into().expect("compile to BPF");
    }

    /// The probe children below die from SIGSYS on purpose; keep them from
    /// leaving core files behind.
    fn no_core_dump() {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: setrlimit reads one valid rlimit struct.
        unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const limit) };
    }

    /// Applies the real filter in a separate copy of this test binary and
    /// checks that the kernel kills it on the new mount API.
    #[test]
    fn new_mount_api_kills_the_agent() {
        const PROBE: &str = "HAVN_SECCOMP_PROBE";
        if std::env::var_os(PROBE).is_some() {
            no_core_dump();
            restrict_syscalls(&[]).expect("install filter");
            // SAFETY: fsopen with a valid NUL-terminated name and no flags;
            // the filter is expected to kill the process before it returns.
            unsafe { libc::syscall(libc::SYS_fsopen, c"tmpfs".as_ptr(), 0) };
            std::process::exit(0);
        }
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "seccomp::tests::new_mount_api_kills_the_agent"])
            .env(PROBE, "1")
            .status()
            .expect("run probe");
        assert_eq!(status.signal(), Some(libc::SIGSYS), "{status:?}");
    }

    /// With the filter in place `unshare` and `clone` asking for a new
    /// user namespace fail with EPERM and `clone3` with ENOSYS, while
    /// threads and processes still start; `user_namespaces` in allow_extra
    /// lets all three through. Each call carries an argument the kernel
    /// rejects with EINVAL, so only the filter can make it fail otherwise,
    /// whether or not the host allows user namespaces, and none of them
    /// creates anything.
    #[test]
    fn new_user_namespaces_are_refused_unless_allowed() {
        const PROBE: &str = "HAVN_SECCOMP_USERNS_PROBE";
        // Not 0: a probe that ran no test exits 0.
        const PROBE_PASSED: i32 = 42;
        if let Some(mode) = std::env::var_os(PROBE) {
            let allowed = mode == "allow";
            let allow: Vec<String> = if allowed {
                vec![USER_NAMESPACES.into()]
            } else {
                Vec::new()
            };
            restrict_syscalls(&allow).expect("install filter");
            let errno = |rc: i64| {
                assert_eq!(rc, -1);
                std::io::Error::last_os_error().raw_os_error()
            };
            // CLONE_SETTLS is not an unshare flag.
            // SAFETY: rejected by the kernel or the filter before any effect.
            let unshare = errno(i64::from(unsafe {
                libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_SETTLS)
            }));
            // CLONE_THREAD needs CLONE_SIGHAND.
            // SAFETY: as above; no child is created.
            let clone = errno(unsafe {
                libc::syscall(
                    libc::SYS_clone,
                    libc::CLONE_NEWUSER | libc::CLONE_THREAD,
                    0,
                    0,
                    0,
                    0,
                )
            });
            // A zero-sized clone_args.
            // SAFETY: as above.
            let clone3 = errno(unsafe { libc::syscall(SYS_CLONE3, std::ptr::null::<u8>(), 0) });
            let expected = if allowed {
                [Some(libc::EINVAL); 3]
            } else {
                [Some(libc::EPERM), Some(libc::EPERM), Some(libc::ENOSYS)]
            };
            assert_eq!([unshare, clone, clone3], expected, "unshare, clone, clone3");
            // A thread and a process still start (glibc falls back from
            // clone3 to clone).
            std::thread::spawn(|| ()).join().expect("thread");
            assert!(
                std::process::Command::new("true")
                    .status()
                    .expect("spawn")
                    .success()
            );
            std::process::exit(PROBE_PASSED);
        }
        for mode in ["deny", "allow"] {
            let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "seccomp::tests::new_user_namespaces_are_refused_unless_allowed",
                ])
                .env(PROBE, mode)
                .status()
                .expect("run probe");
            assert_eq!(status.code(), Some(PROBE_PASSED), "{mode}: {status:?}");
        }
    }

    #[test]
    fn user_namespaces_is_accepted_in_allow_extra() {
        let blocked = effective_blocklist(&[USER_NAMESPACES.into()]).expect("ok");
        assert_eq!(blocked.len(), DANGEROUS_SYSCALLS.len());
        user_namespace_guard().expect("filters compile");
    }

    /// 32-bit system calls (int 0x80) use a different syscall table, so the
    /// filter must kill on an architecture mismatch rather than allow it.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn i386_syscalls_are_refused() {
        const PROBE: &str = "HAVN_SECCOMP_I386_PROBE";
        if std::env::var_os(PROBE).is_some() {
            no_core_dump();
            restrict_syscalls(&[]).expect("install filter");
            let mut nr: i64 = 20; // getpid in the i386 table
            // SAFETY: int 0x80 with eax=20 is the i386 getpid syscall; it takes
            // no arguments. Kernels before 4.17 also zeroed r8-r11.
            unsafe {
                std::arch::asm!(
                    "int 0x80",
                    inout("rax") nr,
                    lateout("r8") _,
                    lateout("r9") _,
                    lateout("r10") _,
                    lateout("r11") _,
                );
            }
            // Reaching this line means the call was allowed.
            std::process::exit(i32::from(nr < 0));
        }
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "seccomp::tests::i386_syscalls_are_refused"])
            .env(PROBE, "1")
            .status()
            .expect("run probe");
        // SIGSYS: the filter killed it. SIGSEGV: the kernel has no 32-bit
        // syscall entry at all (ia32 emulation off), which also refuses it.
        assert!(
            matches!(status.signal(), Some(libc::SIGSYS | libc::SIGSEGV)),
            "{status:?}"
        );
    }

    /// x32 system calls (number | 0x4000_0000) must be killed even though
    /// the blocklist matches exact numbers.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_syscalls_are_refused() {
        const PROBE: &str = "HAVN_SECCOMP_X32_PROBE";
        if std::env::var_os(PROBE).is_some() {
            no_core_dump();
            restrict_syscalls(&[]).expect("install filter");
            // SAFETY: getpid takes no arguments; with the x32 bit set it is
            // either killed by the filter or rejected with ENOSYS.
            let rc = unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
            // Reaching this line means the call was allowed.
            std::process::exit(i32::from(rc < 0));
        }
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "seccomp::tests::x32_syscalls_are_refused"])
            .env(PROBE, "1")
            .status()
            .expect("run probe");
        assert_eq!(status.signal(), Some(libc::SIGSYS), "{status:?}");
    }
}
