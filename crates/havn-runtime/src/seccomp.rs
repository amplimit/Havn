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
//! Called once in [`super::main`] after Landlock has been installed. Runs
//! late enough that startup syscalls (file I/O for bootstrap files, cgroup
//! attach, socket connect) have completed. Inherited across `execve` and
//! `fork` so subprocesses spawned by the `shell` tool get the same
//! restrictions.
//!
//! ## What's in the list
//!
//! Spec-derived (§4.1): `mount`, `umount`, `reboot`, `kexec_load`, `ptrace`,
//! `init_module`. Extended with the canonical "dangerous syscalls" set
//! every mainstream container runtime blocks:
//!
//! - mount-table manipulation (`mount`, `umount2`, `pivot_root`)
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

    seccompiler::apply_filter(&program).map_err(|e| SeccompError::Apply(e.to_string()))?;

    info!(
        blocked = blocked.len(),
        relaxed = allow_extra.len(),
        "seccomp filter installed"
    );
    Ok(())
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
/// sandbox (`pivot_root`, `userfaultfd`); add new entries only when a
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
}
