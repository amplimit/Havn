//! Namespace setup primitives executed in the agent's pre-exec hook.
//!
//! ## Why this is its own file
//!
//! The functions below run **between fork and exec** in the child process.
//! At that point the child has only one thread (us); other gateway threads
//! continue in the parent. The C standard says only async-signal-safe
//! functions are guaranteed-correct in this state because locks held by
//! sibling threads in the parent are still observed as held in the child,
//! and trying to acquire one would deadlock.
//!
//! In practice, well-controlled allocations are fine — the gateway's
//! allocator doesn't hold long-lived locks across our spawn path — but we
//! prefer raw libc syscalls for the I/O. Each helper is small, side-effect
//! local, and either succeeds or errors with the kernel's `errno`.
//!
//! ## What we do
//!
//! 1. `unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET)` — three new
//!    namespaces atomically. The child enters them; the parent is unaffected.
//! 2. Map the host UID/GID to UID/GID 0 inside the user namespace
//!    (`/proc/self/setgroups` → "deny" — required for unprivileged
//!    user-ns to write `gid_map` — then `/proc/self/uid_map` and
//!    `/proc/self/gid_map`). After this the agent runs as UID 0 in its
//!    own namespace, which is the host's unprivileged UID.
//! 3. Mark mount propagation `MS_PRIVATE` so any future mount/unmount the
//!    agent does inside its mount namespace doesn't escape to the host.
//!
//! ## PID namespace fork dance
//!
//! `unshare(CLONE_NEWPID)` does NOT move the caller into the new PID
//! namespace — only the **next** fork creates a child that lands as PID 1
//! inside it. We therefore fork inside ``pre_exec`` after the unshare:
//!
//! - **Parent** (still in the host PID ns): becomes a thin reaper. It
//!   `waitpid`s the child and `_exit`s with the child's status, so the
//!   spawner sees the right return code without needing a separate init
//!   binary. Never returns from `fork_into_pid_namespace_and_become_init`.
//! - **Child** (PID 1 in the new PID ns): returns from the helper, then
//!   mounts a fresh `proc` via [`mount_proc`] (so `/proc` reflects the new
//!   PID ns rather than the host's), and finally exec's the runtime.
//!
//! ## Minimal-rootfs pivot
//!
//! [`pivot_root_into_minimal_rootfs`] flips the calling mount namespace's
//! root from the host's `/` to a freshly-built tmpfs that exposes only:
//! `/usr` (ro bind), `/lib`/`/lib64`/`/bin`/`/sbin` (symlinks → `/usr/*`),
//! `/proc` (PID-ns view), `/tmp` (tmpfs), `/dev/{null,urandom,zero,random}`,
//! `/etc/{resolv.conf,ssl}` (when network allowed), and the agent's
//! workspace mirror-bound at its host path. Everything else (`/var`,
//! `/home`, `/root`, `/srv`, `/sys`, `/etc/{shadow,passwd,...}`) is
//! invisible to the agent — Landlock no longer the only thing keeping
//! the host fs out of view.

use std::ffi::{CStr, CString};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

/// Pid the outer (host-PID-ns) reaper forwards `SIGTERM` / `SIGINT` /
/// `SIGHUP` to. Set once in [`fork_into_pid_namespace_and_become_init`]'s
/// parent branch right before installing handlers; read in the
/// `forward_signal_handler` (which is async-signal-safe and can only touch
/// atomics + `kill`).
static FORWARD_TARGET_PID: AtomicI32 = AtomicI32::new(0);

/// Flags applied via [`unshare_namespaces`].
///
/// USER: agent runs as UID 0 in its own ns, mapped to the host's unprivileged
/// UID. Provides the privilege barrier even without seccomp.
///
/// NS (mount): future mount/unmount calls (and any `pivot_root` we add) are
/// scoped; the host's mount table is unaffected.
///
/// NET: agent has its own loopback-only network stack; no external traffic
/// possible until policy explicitly grants a veth pair (Phase 2).
///
/// PID: agent cannot see any other process on the host or in any other
/// agent's namespace via `/proc` (spec §4.1). Has the unusual semantic that
/// the calling process is NOT moved into the new ns — only the first fork
/// after the unshare lands a child as PID 1 inside it. The caller pairs
/// `unshare_namespaces` with [`fork_into_pid_namespace_and_become_init`] to
/// complete the transition.
const NAMESPACE_FLAGS: libc::c_int =
    libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWPID;

/// Apply the namespace `unshare`. Returns `io::Error` carrying the kernel
/// `errno` on failure — typically `EPERM` when unprivileged user-ns is
/// disabled (`/proc/sys/kernel/unprivileged_userns_clone = 0`).
///
/// # Safety
///
/// Must be called in the child after `fork()` and before `execve()`. Calling
/// it elsewhere will retroactively put existing threads of the gateway into
/// new namespaces, which is not what we want.
pub unsafe fn unshare_namespaces() -> io::Result<()> {
    // SAFETY: `unshare` is async-signal-safe; we pass a defined flag set.
    let rc = unsafe { libc::unshare(NAMESPACE_FLAGS) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Build the single-line `uid_map` payload mapping `host_uid` outside to
/// UID 0 inside. Trailing newline is required by the kernel.
pub fn uid_map_line(host_uid: u32) -> String {
    format!("0 {host_uid} 1\n")
}

/// Build the single-line `gid_map` payload, same shape as the uid map.
pub fn gid_map_line(host_gid: u32) -> String {
    format!("0 {host_gid} 1\n")
}

/// Write the user-namespace mapping files. Order matters:
///
/// 1. `/proc/self/setgroups` ← "deny"  (required for unprivileged user-ns
///    before writing `gid_map`; without it the kernel rejects the write).
/// 2. `/proc/self/uid_map`   ← `0 <host_uid> 1\n`
/// 3. `/proc/self/gid_map`   ← `0 <host_gid> 1\n`
///
/// # Safety
///
/// Must be called inside the child after a successful
/// [`unshare_namespaces`]. Outside that context it has no effect (the
/// `/proc/self/uid_map` writes go to the calling process's existing
/// namespace, which is usually the host's user namespace where the
/// kernel rejects them).
#[allow(
    clippy::similar_names,
    reason = "uid/gid is the canonical spelling for these Linux primitives"
)]
pub unsafe fn write_user_namespace_maps(host_uid: u32, host_gid: u32) -> io::Result<()> {
    // SAFETY: `write_proc_file`'s contract requires being called inside the
    // child after `unshare(CLONE_NEWUSER)`. That's exactly our caller's
    // contract (this function is unsafe with the same precondition).
    unsafe {
        write_proc_file(c"/proc/self/setgroups", b"deny")?;
        let uid = uid_map_line(host_uid);
        write_proc_file(c"/proc/self/uid_map", uid.as_bytes())?;
        let gid = gid_map_line(host_gid);
        write_proc_file(c"/proc/self/gid_map", gid.as_bytes())?;
    }
    Ok(())
}

/// Mark the entire mount tree under `/` as `MS_PRIVATE` so the agent's
/// mount-ns mount/unmount calls don't propagate back to the host.
///
/// # Safety
///
/// Must be called after [`unshare_namespaces`]; otherwise we'd be
/// modifying the host's mount propagation, which is observably wrong.
pub unsafe fn mark_mounts_private() -> io::Result<()> {
    // SAFETY: NULL source/fstype/data with MS_REC|MS_PRIVATE is the
    // canonical recursive private-propagation invocation.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Mount a fresh `proc` filesystem on `/proc` so the child sees only its
/// own PID namespace through `/proc/<pid>` (otherwise it would inherit the
/// host's procfs view via the mount-namespace clone).
///
/// Public for callers that don't pivot into a minimal rootfs (subagent
/// flows in Phase 2, debugging). The standard production path uses
/// [`pivot_root_into_minimal_rootfs`] instead, which mounts `proc` inside
/// the new rootfs as part of the same operation.
///
/// # Safety
///
/// Must be called in the child after [`unshare_namespaces`] has established
/// the new PID namespace. Mounting elsewhere either fails (no privileges in
/// the host user ns) or shadows the host's `/proc` for the calling process.
pub unsafe fn mount_proc() -> io::Result<()> {
    // SAFETY: source / target / fstype are static C strings; flags = 0 and
    // data = NULL are the canonical "mount procfs read-write" invocation.
    let rc = unsafe {
        libc::mount(
            c"proc".as_ptr(),
            c"/proc".as_ptr(),
            c"proc".as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Path used as the construction site for the agent's new rootfs (spec §4.1).
/// Lives in the agent's mount namespace only — invisible to the host once
/// `pivot_root` flips it into `/`. Per-process unique because each agent gets
/// its own mount ns.
const NEW_ROOT_DIR: &CStr = c"/tmp/.havn-newroot";
const NEW_ROOT_PATH: &str = "/tmp/.havn-newroot";

/// Build a minimal read-only rootfs and `pivot_root` the calling process
/// into it (spec §4.1 mount-namespace requirements).
///
/// **Layout produced** (the agent sees this as `/` after the call returns):
///
/// ```text
/// /
/// ├── usr/                      bind from host /usr (ro)         — binaries + libs
/// ├── lib    -> usr/lib         symlink                          — modern Ubuntu/Debian layout
/// ├── lib64  -> usr/lib64       symlink
/// ├── bin    -> usr/bin         symlink
/// ├── sbin   -> usr/sbin        symlink
/// ├── proc/                     fresh procfs (new PID ns view)
/// ├── tmp/                      tmpfs (rw)
/// ├── dev/{null,zero,urandom,random}   bind from host /dev/<node>
/// ├── etc/resolv.conf           bind from host (only when network_allowed)
/// └── <workspace_dir>/          bind from host workspace dir (rw)   — mirrored
///                                                                     at the same
///                                                                     path the
///                                                                     runtime
///                                                                     was told
/// ```
///
/// The workspace is mirrored at its host path so the runtime's
/// `--workspace <abs-path>` argument resolves identically inside the
/// rootfs (no path-rewriting needed).
///
/// **What the agent can no longer see** (vs. plain mount-ns +
/// `MS_PRIVATE`): `/etc/{shadow,passwd,group,...}`, `/root`, `/home`,
/// `/var/lib/havn/agents/<other-id>/...`, `/opt`, `/srv`, `/sys`, the
/// host's `/tmp`, the host's `/proc`. Landlock (applied later by the
/// runtime) is the primary access control; `pivot_root` removes the
/// paths from view entirely so even a Landlock bypass would hit ENOENT.
///
/// # Safety
///
/// - Must run after a successful [`unshare_namespaces`] and the inner
///   fork (so the calling process is PID 1 in the new PID ns and owns
///   its own mount ns + `MS_PRIVATE` propagation).
/// - Must run before any `exec`, because the path manipulations rely on
///   the calling process's `chdir` + cleanup of `/old_root`.
/// - Single-threaded only — the function performs many `mount`/`mkdir`/
///   `symlink` syscalls that are safe individually but assume no other
///   thread is touching the same paths.
#[allow(
    clippy::too_many_lines,
    reason = "linear sequence of 13 numbered isolation steps; splitting it obscures the order"
)]
pub unsafe fn pivot_root_into_minimal_rootfs(
    workspace_dir: &Path,
    runtime_binary: &Path,
    gateway_socket: &Path,
    network_allowed: bool,
    extra_mounts: &[crate::ExtraMount],
    tmpfs_mounts: &[crate::TmpfsMount],
    agent_dns: &[String],
) -> io::Result<()> {
    // SAFETY: each block below documents its own contract. Errors short-circuit
    // back to the caller (havn-init), which logs and `_exit`s.
    unsafe {
        // 1. Create `/tmp/.havn-newroot` (idempotent — `EEXIST` is fine).
        if libc::mkdir(NEW_ROOT_DIR.as_ptr(), 0o755) != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EEXIST) {
                return Err(err);
            }
        }

        // 2. tmpfs on the new root. `MS_NOSUID|MS_NODEV` reduce the
        //    attack surface inside the rootfs (no setuid/devnodes).
        check(libc::mount(
            c"tmpfs".as_ptr(),
            NEW_ROOT_DIR.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            std::ptr::null(),
        ))?;

        // 3. Skeleton directories. /old_root is the pivot's "put_old"
        //    target; /usr is the bind target; /proc, /tmp, /dev, /etc
        //    are mountpoints / parent dirs for the binds below.
        for dir in ["old_root", "usr", "proc", "tmp", "dev", "etc"] {
            mkdir_under_newroot(dir, 0o755)?;
        }

        // 4. Bind /usr ro. The 2-step (BIND, then REMOUNT|RDONLY) is the
        //    standard libc dance — the kernel ignores `MS_RDONLY` in the
        //    initial bind for backwards compatibility, so we have to
        //    remount to take it.
        bind_ro(c"/usr", &cstr_under_newroot("usr"))?;

        // 5. Symlinks for /lib, /lib64, /bin, /sbin → /usr/*.
        //    Modern Ubuntu / Debian / Fedora put everything in /usr;
        //    legacy distros that have real /lib etc. would need a
        //    different path here, but our deployment target (Ubuntu 24.04
        //    per spec §1.4) uses the unified layout.
        symlink_under_newroot("usr/lib", "lib")?;
        symlink_under_newroot("usr/lib64", "lib64")?;
        symlink_under_newroot("usr/bin", "bin")?;
        symlink_under_newroot("usr/sbin", "sbin")?;

        // 6. tmpfs on /tmp inside the new rootfs. Same hardening flags.
        check(libc::mount(
            c"tmpfs".as_ptr(),
            cstr_under_newroot("tmp").as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            std::ptr::null(),
        ))?;

        // 7. Bind /dev nodes individually. Best-effort — agents that
        //    don't shell out won't notice if any of these aren't on the
        //    host. /dev/null is the one most things assume exists.
        for node in ["null", "urandom", "zero", "random"] {
            let host = CString::new(format!("/dev/{node}")).map_err(io_invalid)?;
            let target = cstr_under_newroot(&format!("dev/{node}"));
            // Touch the bind target as an empty file (bind requires it to exist).
            let fd = libc::open(target.as_ptr(), libc::O_CREAT | libc::O_RDONLY, 0o600);
            if fd >= 0 {
                libc::close(fd);
            }
            // Failure here is non-fatal; log via stderr handled by caller.
            libc::mount(
                host.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            );
        }

        // 8. Optional /etc/resolv.conf + /etc/ssl/certs. Only meaningful
        //    when the agent is allowed network access — without them HTTPS
        //    fails ("No CA certificates were loaded") and DNS resolution
        //    can't happen. The runtime's reqwest stack picks up CA bundles
        //    from `/etc/ssl/certs/ca-certificates.crt` on Debian/Ubuntu.
        //
        //    /etc/resolv.conf strategy: write a curated upstream-resolver
        //    file inside the new rootfs rather than bind-mounting the
        //    host's. Host's resolv.conf often points at `127.0.0.53`
        //    (systemd-resolved stub), which is unreachable from the agent's
        //    fresh netns — DNS would silently break for every agent that
        //    needs outbound. The `agent_dns` list is supplied by the
        //    operator via gateway config `[network] agent_dns`; defaults
        //    to public resolvers (1.1.1.1, 8.8.8.8, 9.9.9.9) so the
        //    out-of-the-box experience works, but enterprises with
        //    internal DNS / Pi-hole / split-horizon override.
        if network_allowed && !agent_dns.is_empty() {
            let resolv = cstr_under_newroot("etc/resolv.conf");
            // Build "nameserver A\nnameserver B\n…\noptions edns0\n".
            let mut body = String::new();
            for ip in agent_dns {
                // Reject anything that's not plausibly an IP literal —
                // we don't want to write hostnames into resolv.conf
                // (libc resolver wouldn't query them anyway, but a
                // hostname-shaped entry would silently be ignored).
                if ip
                    .chars()
                    .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == ':'))
                {
                    eprintln!("havn-init: agent_dns entry {ip:?} is not an IP literal; skipping");
                    continue;
                }
                body.push_str("nameserver ");
                body.push_str(ip);
                body.push('\n');
            }
            body.push_str("options edns0\n");
            if let Ok(c_body) = CString::new(body) {
                let fd = libc::open(
                    resolv.as_ptr(),
                    libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
                    0o644,
                );
                if fd >= 0 {
                    let bytes = c_body.as_bytes();
                    let _ = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                    libc::close(fd);
                }
            }
        } else if network_allowed {
            // Egress is on but operator supplied no DNS servers —
            // surface as a stderr line so the agent's "DNS resolution
            // failing" failure mode is debuggable.
            eprintln!(
                "havn-init: network_allowed but agent_dns list is empty; agent ns has no DNS"
            );
        }
        // System CA bundle — always mounted, regardless of egress.
        // The runtime unconditionally builds a reqwest HTTP client at
        // startup (used by tools, MCP, embedding providers), and reqwest
        // refuses to construct without a CA bundle ("No CA certificates
        // were loaded from the system"). Mounting the bundle into the
        // agent ns is a no-op for security: it's read-only public data
        // that grants nothing on its own. The egress gate is the netns
        // (no veth = no packets out), not the absence of CA certs.
        mkdir_under_newroot("etc/ssl", 0o755)?;
        let ssl_target = cstr_under_newroot("etc/ssl");
        libc::mount(
            c"/etc/ssl".as_ptr(),
            ssl_target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        );
        // Best-effort RO remount.
        libc::mount(
            std::ptr::null(),
            ssl_target.as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            std::ptr::null(),
        );

        // 9. Mirror-bind workspace (rw) at its host path so the
        //    runtime's `--workspace <abs-path>` argument resolves
        //    identically inside the new rootfs.
        mirror_bind_into_newroot(workspace_dir, false)?;

        // 9b. Mirror-bind the runtime binary's parent directory (ro) so
        //     `exec(runtime_binary)` from havn-init's child resolves
        //     after pivot. In production install.sh puts the runtime
        //     under /usr/local/bin (already covered by the /usr bind in
        //     step 4); the `path_starts_with_any` check skips the
        //     redundant remount in that case. In dev (cargo build →
        //     target/debug/havn-runtime under $HOME), this is the only
        //     way the runtime path stays valid post-pivot.
        if let Some(parent) = runtime_binary.parent()
            && !path_starts_with_any(parent, &[Path::new("/usr")])
        {
            mirror_bind_into_newroot(parent, true)?;
        }

        // 9c. Mirror-bind the gateway's Unix socket file at its host
        //     path. The runtime opens it via `UnixStream::connect`
        //     immediately after startup; without this bind the
        //     connect() returns ENOENT post-pivot. Single-file bind —
        //     creates an empty placeholder at the target then binds.
        mirror_bind_file_into_newroot(gateway_socket)?;

        // 9d. Operator-declared extra mounts (spec §4.1 v0.7). Each
        //     gets a mirror-bind at the same absolute path. RO is the
        //     common case; capability packs that drop binaries under
        //     /opt/havn/<pack>/ use this so agents can exec them. RW
        //     is opt-in for packs sharing state across agents.
        //     Non-existent host paths are best-effort skipped — a pack
        //     installer that didn't run leaves its agent failing at
        //     `command not found` (visible in tool_result), not at
        //     spawn time.
        for mount in extra_mounts {
            if !mount.path.exists() {
                eprintln!(
                    "havn-init: extra_mounts entry {} doesn't exist on host — skipping",
                    mount.path.display()
                );
                continue;
            }
            let read_only = matches!(mount.mode, crate::MountMode::Ro);
            // Reuse the workspace-bind helper. It walks parents,
            // mkdirs each, and binds the leaf; the RO remount dance
            // happens inside.
            mirror_bind_into_newroot(&mount.path, read_only)?;
        }

        // 9e. Operator-declared tmpfs mounts (spec §16.2). Pack-agnostic;
        //     `mount -t tmpfs` at the declared path with the declared
        //     size_mb. `MS_NOSUID|MS_NODEV` mirror the rootfs / `/tmp`
        //     tmpfs hardening posture. Path must be relative to the
        //     new rootfs (leading `/` stripped) — operators specify
        //     absolute paths in config (`/dev/shm`); we walk parents
        //     to mkdir as needed.
        for mount in tmpfs_mounts {
            // Walk parents under newroot, mkdir each (idempotent).
            // The path components are bytes after the leading `/`.
            let abs = mount.path.as_path();
            if !abs.is_absolute() {
                eprintln!(
                    "havn-init: tmpfs path {} is not absolute — skipping",
                    abs.display()
                );
                continue;
            }
            // Strip leading `/` and walk components, mkdir each below newroot.
            let mut acc = std::path::PathBuf::new();
            for comp in abs.components().skip(1) {
                acc.push(comp);
                let acc_str = acc
                    .to_str()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-utf8 path"))?;
                // Tolerate EEXIST; anything else is an error.
                let target = cstr_under_newroot(acc_str);
                let r = libc::mkdir(target.as_ptr(), 0o1777);
                if r != 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EEXIST) {
                        return Err(err);
                    }
                }
            }
            let target = cstr_under_newroot(
                acc.to_str()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-utf8 path"))?,
            );
            let opts = if mount.size_mb == 0 {
                CString::new("mode=1777").map_err(io_invalid)?
            } else {
                CString::new(format!("size={}m,mode=1777", mount.size_mb)).map_err(io_invalid)?
            };
            check(libc::mount(
                c"tmpfs".as_ptr(),
                target.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                opts.as_ptr().cast(),
            ))?;
        }

        // 10. Mount fresh procfs on the new rootfs's /proc (PID-ns view).
        check(libc::mount(
            c"proc".as_ptr(),
            cstr_under_newroot("proc").as_ptr(),
            c"proc".as_ptr(),
            0,
            std::ptr::null(),
        ))?;

        // 11. The pivot itself. `pivot_root(2)` flips the calling mount
        //     namespace's root from `/` to NEW_ROOT, with the old root
        //     stashed under `put_old` (= NEW_ROOT/old_root, which we
        //     mkdir'd in step 3).
        let put_old = cstr_under_newroot("old_root");
        let rc = libc::syscall(
            libc::SYS_pivot_root,
            NEW_ROOT_DIR.as_ptr(),
            put_old.as_ptr(),
        );
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        // 12. From this point on, the calling process's view of `/` is
        //     the new rootfs. `chdir("/")` so the working directory
        //     follows; otherwise relative paths still resolve against
        //     wherever cwd was before pivot.
        check(libc::chdir(c"/".as_ptr()))?;

        // 13. Detach the old root (still visible at `/old_root`) and
        //     remove the mountpoint. `MNT_DETACH` is a lazy unmount —
        //     the old root's contents drop away as soon as nothing
        //     refers to them.
        check(libc::umount2(c"/old_root".as_ptr(), libc::MNT_DETACH))?;
        let _ = libc::rmdir(c"/old_root".as_ptr());

        Ok(())
    }
}

/// Mirror-bind `host_path` into the new rootfs at the same absolute path.
///
/// Walks the host path one component at a time, `mkdir`s each prefix
/// inside the new root, then `mount --bind`s the leaf. When `read_only`
/// is true, follows up with the standard `MS_REMOUNT|MS_BIND|MS_RDONLY`
/// dance.
///
/// # Safety
///
/// Same contract as the surrounding `pivot_root_into_minimal_rootfs`:
/// must run inside the new mount namespace, single-threaded.
unsafe fn mirror_bind_into_newroot(host_path: &Path, read_only: bool) -> io::Result<()> {
    let host_str = host_path.to_str().ok_or_else(|| {
        io::Error::other(format!("path is not valid UTF-8: {}", host_path.display()))
    })?;
    let trimmed = host_str.trim_start_matches('/');
    let mut acc = String::new();
    for component in trimmed.split('/').filter(|c| !c.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(component);
        // SAFETY: helper documents its own contract (idempotent mkdir
        // under NEW_ROOT_PATH).
        unsafe { mkdir_under_newroot(&acc, 0o755) }?;
    }
    let target = cstr_under_newroot(trimmed);
    let host_c = CString::new(host_str).map_err(io_invalid)?;
    // SAFETY: src/dest are valid CStrings; null fstype/data with MS_BIND
    // is the canonical bind invocation.
    unsafe {
        check(libc::mount(
            host_c.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ))?;
        if read_only {
            check(libc::mount(
                std::ptr::null(),
                target.as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
                std::ptr::null(),
            ))?;
        }
    }
    Ok(())
}

/// True iff `candidate` is `prefix` or one of `prefix`'s descendants for
/// any `prefix` in `prefixes`. Used to skip redundant mirror binds when
/// the path is already inside `/usr` (or another path we always bind).
fn path_starts_with_any(candidate: &Path, prefixes: &[&Path]) -> bool {
    prefixes.iter().any(|p| candidate.starts_with(p))
}

/// Mirror-bind a single file (vs. [`mirror_bind_into_newroot`] which
/// targets a directory). Creates parent dirs + an empty placeholder at
/// the in-rootfs path, then `mount --bind`s the host file onto it.
/// Used for the gateway Unix socket — the runtime opens it via
/// `UnixStream::connect` and the path must exist inside the rootfs.
unsafe fn mirror_bind_file_into_newroot(host_path: &Path) -> io::Result<()> {
    let host_str = host_path.to_str().ok_or_else(|| {
        io::Error::other(format!("path is not valid UTF-8: {}", host_path.display()))
    })?;
    let trimmed = host_str.trim_start_matches('/');

    // Walk and mkdir each PARENT component (everything except the leaf).
    let mut acc = String::new();
    let parts: Vec<&str> = trimmed.split('/').filter(|c| !c.is_empty()).collect();
    if parts.is_empty() {
        return Err(io::Error::other("file path has no components"));
    }
    for component in &parts[..parts.len() - 1] {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(component);
        // SAFETY: helper documents idempotent mkdir under NEW_ROOT_PATH.
        unsafe { mkdir_under_newroot(&acc, 0o755) }?;
    }

    // Touch the placeholder file at the leaf path.
    let target = cstr_under_newroot(trimmed);
    // SAFETY: open with CREAT on a known path is well-defined.
    let fd = unsafe { libc::open(target.as_ptr(), libc::O_CREAT | libc::O_RDONLY, 0o600) };
    if fd >= 0 {
        // SAFETY: closing an owned valid fd.
        unsafe { libc::close(fd) };
    }
    let host_c = CString::new(host_str).map_err(io_invalid)?;
    // SAFETY: bind-mount file onto file is well-defined; both pointers valid.
    unsafe {
        check(libc::mount(
            host_c.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        ))?;
    }
    Ok(())
}

/// `mkdir <NEW_ROOT_PATH>/<rel>` with the given mode. Idempotent.
unsafe fn mkdir_under_newroot(rel: &str, mode: libc::mode_t) -> io::Result<()> {
    let path = cstr_under_newroot(rel);
    // SAFETY: `path` is a valid C string; mode is a constant.
    let rc = unsafe { libc::mkdir(path.as_ptr(), mode) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    Err(err)
}

/// `symlink(target, NEW_ROOT_PATH/<linkname>)`. Idempotent (`EEXIST` is fine).
unsafe fn symlink_under_newroot(target: &str, linkname: &str) -> io::Result<()> {
    let target_c = CString::new(target).map_err(io_invalid)?;
    let link = cstr_under_newroot(linkname);
    // SAFETY: both pointers are valid C strings.
    let rc = unsafe { libc::symlink(target_c.as_ptr(), link.as_ptr()) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    Err(err)
}

/// Bind `src` onto `dest` read-only. The 2-step is required because the
/// initial `MS_BIND` ignores `MS_RDONLY` (kernel compatibility quirk).
unsafe fn bind_ro(src: &CStr, dest: &CStr) -> io::Result<()> {
    // SAFETY: src and dest are valid C strings; the flag set is well-defined.
    unsafe {
        check(libc::mount(
            src.as_ptr(),
            dest.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ))?;
        check(libc::mount(
            std::ptr::null(),
            dest.as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            std::ptr::null(),
        ))
    }
}

/// Build a `CString` for `<NEW_ROOT_PATH>/<rel>`. `rel` must NOT start with `/`.
fn cstr_under_newroot(rel: &str) -> CString {
    let mut joined = String::with_capacity(NEW_ROOT_PATH.len() + 1 + rel.len());
    joined.push_str(NEW_ROOT_PATH);
    joined.push('/');
    joined.push_str(rel);
    // The strings we pass in (constants in this file + sanitised workspace
    // path components) cannot contain interior NUL bytes.
    // The strings we pass in (constants in this file + sanitised path
    // components) cannot contain interior NUL bytes; if they ever do,
    // fall back to a path inside our own dir rather than panicking in
    // a fork-after-unshare context where unwinding is unsafe.
    CString::new(joined).unwrap_or_else(|_| {
        CString::new(NEW_ROOT_PATH).unwrap_or_else(|_| {
            // Static literal cannot contain NUL — this is a defence
            // against a future caller passing a NUL-laden constant.
            CString::default()
        })
    })
}

/// Map a syscall return code (`0` ok, `-1` error) to `io::Result<()>`.
unsafe fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn io_invalid<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
}

/// Forward `SIGTERM` / `SIGINT` / `SIGHUP` from the outer reaper to the
/// inner PID 1 in the new namespace. Reads the target pid from a static
/// atomic so the handler stays async-signal-safe.
///
/// # Safety
///
/// Must be called only by the outer reaper after `fork()` succeeds. The
/// signal handler reads `FORWARD_TARGET_PID` and calls `kill(2)`; both are
/// async-signal-safe. We don't set `SA_RESTART` on the handlers so the
/// outer's `waitpid` returns `EINTR` after each forwarded signal — the
/// surrounding loop simply retries until the child actually exits.
extern "C" fn forward_signal_to_inner(sig: libc::c_int) {
    let pid = FORWARD_TARGET_PID.load(Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: `kill` is async-signal-safe; ESRCH on a vanished child
        // is a benign race we ignore (the waitpid will report exit).
        unsafe {
            let _ = libc::kill(pid, sig);
        }
    }
}

/// Public entry used by `havn-init` (the PID-1 shim). Same contract as the
/// internal helper — installs `SIGTERM` / `SIGINT` / `SIGHUP` forwarders
/// from the calling process to `child_pid`.
///
/// # Safety
///
/// Must be called from a single-threaded post-fork context (the
/// `havn-init` outer reaper qualifies). Concurrent callers from multiple
/// threads would race on `FORWARD_TARGET_PID`.
pub unsafe fn install_signal_forwarder_for(child_pid: libc::pid_t) {
    // SAFETY: contract delegated to caller; matches inner helper.
    unsafe { install_signal_forwarder(child_pid) };
}

/// Public entry used by `havn-init`. Block in `waitpid` for `child_pid`
/// (handling `EINTR`), translate the wait status into a process exit
/// code, and return it. Pure helper — no `_exit`, the caller decides
/// when to terminate.
#[must_use]
pub fn wait_for_child_exit_code(child_pid: libc::pid_t) -> i32 {
    wait_for_child_then_exit_code(child_pid)
}

unsafe fn install_signal_forwarder(child_pid: libc::pid_t) {
    FORWARD_TARGET_PID.store(child_pid, Ordering::Relaxed);

    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: `sigaction` with a zeroed `mask` and a pointer to a
        // C-ABI function is the standard way to install a signal handler.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = forward_signal_to_inner as *const () as usize;
            // SA_RESTART deliberately omitted — see doc.
            sa.sa_flags = 0;
            libc::sigemptyset(&raw mut sa.sa_mask);
            libc::sigaction(sig, &raw const sa, std::ptr::null_mut());
        }
    }
}

/// `waitpid` in a loop, restarting on `EINTR`, and translate the wait
/// status into a process exit code (`exit code` for normal exit, `128 +
/// signal` for signal termination, `1` on any other failure mode).
///
/// Pure helper — no global state, no allocations.
fn wait_for_child_then_exit_code(child_pid: libc::pid_t) -> libc::c_int {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: status is a valid pointer to a stack `c_int`; pid is the
        // child we just forked.
        let r = unsafe { libc::waitpid(child_pid, &raw mut status, 0) };
        if r == -1 {
            // EINTR is benign; any other errno means the child is gone or
            // unreachable, so we surface a generic failure exit code.
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return 1;
        }
        if r == child_pid {
            break;
        }
    }
    // `WIFEXITED` / `WEXITSTATUS` / etc. are exposed as safe inline helpers
    // by the `libc` crate; they read the status integer with no syscall.
    if libc::WIFEXITED(status) {
        return libc::WEXITSTATUS(status);
    }
    if libc::WIFSIGNALED(status) {
        return 128 + libc::WTERMSIG(status);
    }
    1
}

/// Open + write + close a `/proc/self/...` control file using only libc
/// primitives. We intentionally avoid `std::fs::write` here — it allocates
/// internally and would be unsound across `fork` if any sibling thread held
/// the allocator lock when we forked.
///
/// # Safety
///
/// `path` must be a valid `CStr` pointing at a writable proc/sys file. The
/// byte slice `data` must outlive the syscall.
unsafe fn write_proc_file(path: &CStr, data: &[u8]) -> io::Result<()> {
    // SAFETY: open with a constant CStr is well-defined.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut written = 0usize;
    while written < data.len() {
        // SAFETY: fd is valid; pointer + length describe an in-bounds slice.
        let n = unsafe { libc::write(fd, data.as_ptr().add(written).cast(), data.len() - written) };
        if n < 0 {
            let err = io::Error::last_os_error();
            // SAFETY: closing an owned valid fd.
            let _ = unsafe { libc::close(fd) };
            return Err(err);
        }
        if n == 0 {
            // SAFETY: closing an owned valid fd.
            let _ = unsafe { libc::close(fd) };
            return Err(io::Error::other("short write"));
        }
        #[allow(clippy::cast_sign_loss, reason = "n >= 0 guarded above")]
        let n_usize = n as usize;
        written += n_usize;
    }
    // SAFETY: closing an owned valid fd.
    let _ = unsafe { libc::close(fd) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_map_line_format() {
        assert_eq!(uid_map_line(1000), "0 1000 1\n");
        assert_eq!(uid_map_line(0), "0 0 1\n");
    }

    #[test]
    fn gid_map_line_format() {
        assert_eq!(gid_map_line(1000), "0 1000 1\n");
        assert_eq!(gid_map_line(65534), "0 65534 1\n");
    }

    #[test]
    fn namespace_flags_include_user_mount_net_pid() {
        assert_ne!(NAMESPACE_FLAGS & libc::CLONE_NEWUSER, 0);
        assert_ne!(NAMESPACE_FLAGS & libc::CLONE_NEWNS, 0);
        assert_ne!(NAMESPACE_FLAGS & libc::CLONE_NEWNET, 0);
        // PID namespace pinned ON: spec §4.1 requires "Agent cannot see any
        // other process on the host or any other agent. PID 1 inside the
        // namespace is the agent process itself." Pair with
        // `fork_into_pid_namespace_and_become_init` to make the agent PID 1.
        assert_ne!(NAMESPACE_FLAGS & libc::CLONE_NEWPID, 0);
    }

    #[test]
    fn wait_status_translation_normal_exit() {
        // Construct a status value the way the kernel would for a normal
        // exit code 42: low 8 bits = 0 (no signal), bits 8-15 = exit code.
        let status: libc::c_int = 42 << 8;
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 42);
    }

    #[test]
    fn wait_status_translation_signal_kill() {
        // Status for "killed by SIGTERM" (15): low 7 bits = signal, bit 7 = 0.
        let status: libc::c_int = libc::SIGTERM;
        assert!(libc::WIFSIGNALED(status));
        assert_eq!(libc::WTERMSIG(status), libc::SIGTERM);
    }
}
