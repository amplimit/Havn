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
//! 0. Write `0` to the agent cgroup's `cgroup.procs` so the child, and
//!    everything it later forks, is inside the agent cgroup before exec.
//! 1. `unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET)` — three new
//!    namespaces atomically. The child enters them; the parent is unaffected.
//! 2. Map UID/GID 0 inside the user namespace to the agent's host ids.
//!    Under a root gateway these are the agent's own unprivileged ids,
//!    which only a process outside the namespace may map
//!    ([`unshare_with_host_ids`]); `havn-init` then switches to them
//!    ([`become_namespace_root`]). An unprivileged gateway maps its own
//!    ids from inside ([`write_user_namespace_maps`]).
//! 3. Mark mount propagation `MS_PRIVATE` so any future mount/unmount the
//!    agent does inside its mount namespace doesn't escape to the host.
//! 4. `PR_SET_PDEATHSIG` ([`kill_with_parent`]) so the agent is SIGKILLed
//!    when the gateway dies instead of outliving it.
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
//! `/etc/ssl/certs` (ro), `/etc/resolv.conf` (when network allowed), and the agent's
//! workspace mirror-bound at its host path. Everything else (`/var`,
//! `/home`, `/root`, `/srv`, `/sys`, `/etc/{shadow,passwd,...}`) is
//! invisible to the agent — Landlock no longer the only thing keeping
//! the host fs out of view.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
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

/// Write the user-namespace mapping files, mapping the caller's own host
/// ids (the only ones it may map from inside). Order matters:
///
/// 1. `/proc/self/setgroups` ← "deny"  (required for unprivileged user-ns
///    before writing `gid_map`; without it the kernel rejects the write).
/// 2. `/proc/self/uid_map`   ← `uid_map`, a [`uid_map_line`]
/// 3. `/proc/self/gid_map`   ← `gid_map`, a [`gid_map_line`]
///
/// # Safety
///
/// Must be called inside the child after a successful
/// [`unshare_namespaces`]. Outside that context it has no effect (the
/// `/proc/self/uid_map` writes go to the calling process's existing
/// namespace, which is usually the host's user namespace where the
/// kernel rejects them).
pub unsafe fn write_user_namespace_maps(uid_map: &[u8], gid_map: &[u8]) -> io::Result<()> {
    // SAFETY: `write_proc_file`'s contract requires being called inside the
    // child after `unshare(CLONE_NEWUSER)`. That's exactly our caller's
    // contract (this function is unsafe with the same precondition).
    unsafe {
        write_proc_file(c"/proc/self/setgroups", b"deny")?;
        write_proc_file(c"/proc/self/uid_map", uid_map)?;
        write_proc_file(c"/proc/self/gid_map", gid_map)?;
    }
    Ok(())
}

/// Leave the gateway's supplementary groups, which for root often include
/// gid 0, so the agent holds no host group but its own.
///
/// # Safety
///
/// Between fork and exec in a child of a root process, before
/// [`unshare_with_host_ids`].
pub unsafe fn clear_supplementary_groups() -> io::Result<()> {
    // SAFETY: an empty list needs no buffer.
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Unshare [`NAMESPACE_FLAGS`] and map uid 0 and gid 0 of the new user
/// namespace to the host ids in `uid_map` and `gid_map` ([`uid_map_line`]
/// and [`gid_map_line`]). A process inside the namespace may map only its
/// own ids, so a helper forked first, which stays outside with the
/// gateway's root privileges, writes the maps once the unshare is done.
///
/// The caller keeps its host uid 0, which the namespace does not map: it
/// holds every capability in the namespace but owns nothing there until
/// it switches to the namespace's root ([`become_namespace_root`]).
///
/// # Safety
///
/// Between fork and exec in a child of a root process. Only
/// `socketpair`, `fork`, `unshare`, `send`, `read`, `open`, `write`,
/// `waitpid` and `_exit`, none of which allocate.
pub unsafe fn unshare_with_host_ids(uid_map: &[u8], gid_map: &[u8]) -> io::Result<()> {
    let mut fds: [libc::c_int; 2] = [-1; 2];
    // A socket rather than a pipe: `send` with MSG_NOSIGNAL cannot raise
    // SIGPIPE in this child if the helper is gone.
    // SAFETY: `fds` is a valid two-element array for socketpair to fill.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let [ours, helpers] = fds;
    // SAFETY: getpid cannot fail.
    let target = unsafe { libc::getpid() };
    // SAFETY: the helper below makes only async-signal-safe calls.
    let helper = unsafe { libc::fork() };
    if helper == -1 {
        let err = io::Error::last_os_error();
        // SAFETY: closing both ends created above.
        unsafe {
            libc::close(ours);
            libc::close(helpers);
        }
        return Err(err);
    }
    if helper == 0 {
        // SAFETY: closing the end the helper does not use.
        unsafe { libc::close(ours) };
        let mut go = 0u8;
        let received = loop {
            // SAFETY: one byte into a live stack byte.
            let n = unsafe { libc::read(helpers, (&raw mut go).cast(), 1) };
            if n < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break n;
        };
        // Nothing arrives when the unshare failed; there is nothing to map.
        let code = if received == 1 {
            write_id_maps(target, uid_map, gid_map)
                .err()
                .map_or(0, |e| e.raw_os_error().unwrap_or(libc::EIO))
        } else {
            0
        };
        // SAFETY: `_exit` is async-signal-safe and never returns.
        unsafe { libc::_exit(code) };
    }
    // SAFETY: closing the helper's end in this process.
    unsafe { libc::close(helpers) };
    // SAFETY: a defined flag set.
    let unshared = unsafe { libc::unshare(NAMESPACE_FLAGS) } == 0;
    let unshare_error = (!unshared).then(io::Error::last_os_error);
    if unshared {
        // SAFETY: one byte from a static; a gone helper shows in its status.
        unsafe { libc::send(ours, b"1".as_ptr().cast(), 1, libc::MSG_NOSIGNAL) };
    }
    // SAFETY: closing our end, which also tells a waiting helper to stop.
    unsafe { libc::close(ours) };
    let status = wait_for_child_exit_code(helper);
    if let Some(err) = unshare_error {
        return Err(err);
    }
    match status {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

/// The helper of [`unshare_with_host_ids`]: write `pid`'s id maps from
/// outside its user namespace.
fn write_id_maps(pid: libc::pid_t, uid_map: &[u8], gid_map: &[u8]) -> io::Result<()> {
    let mut buf = [0u8; 48];
    for (leaf, data) in [
        (&b"setgroups"[..], &b"deny"[..]),
        (&b"uid_map"[..], uid_map),
        (&b"gid_map"[..], gid_map),
    ] {
        let path = proc_pid_path(&mut buf, pid, leaf)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENAMETOOLONG))?;
        // SAFETY: a proc control file of a process this one forked from.
        unsafe { write_proc_file(path, data) }?;
    }
    Ok(())
}

/// `/proc/<pid>/<leaf>` as a C string in `buf`, without allocating.
fn proc_pid_path<'a>(buf: &'a mut [u8; 48], pid: libc::pid_t, leaf: &[u8]) -> Option<&'a CStr> {
    let mut digits = [0u8; 10];
    let mut rest = u32::try_from(pid).ok()?;
    let mut count = 0;
    loop {
        *digits.get_mut(count)? = b'0' + u8::try_from(rest % 10).ok()?;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let mut at = 0;
    for &byte in b"/proc/"
        .iter()
        .chain(digits[..count].iter().rev())
        .chain(b"/")
        .chain(leaf)
    {
        *buf.get_mut(at)? = byte;
        at += 1;
    }
    *buf.get_mut(at)? = 0;
    CStr::from_bytes_with_nul(&buf[..=at]).ok()
}

/// Carry the namespace's capabilities across the exec of `havn-init`.
/// After [`unshare_with_host_ids`] the caller's host uid 0 is not the
/// namespace's root, so exec would clear them; raised into the ambient set
/// (through the inheritable set) they survive it. `havn-init` empties
/// every set before it execs the runtime.
///
/// # Safety
///
/// Only `capget`, `capset` and `prctl`, on the calling thread.
pub unsafe fn keep_capabilities_across_exec() -> io::Result<()> {
    let mut header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut words = [CapabilityWords {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: capget fills two capability words for the calling thread.
    if unsafe { libc::syscall(libc::SYS_capget, &raw mut header, words.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for word in &mut words {
        word.inheritable = word.permitted;
    }
    // SAFETY: capset reads the header and the two words, live for the call.
    if unsafe { libc::syscall(libc::SYS_capset, &raw mut header, words.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for cap in 0..=63_u32 {
        // SAFETY: prctl with integer arguments only.
        if unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_RAISE,
                libc::c_ulong::from(cap),
                0,
                0,
            )
        } != 0
        {
            let err = io::Error::last_os_error();
            // EINVAL: past the kernel's last capability.
            if err.raw_os_error() == Some(libc::EINVAL) && cap > 0 {
                break;
            }
            return Err(err);
        }
    }
    Ok(())
}

/// Switch to uid 0 and gid 0 of the agent's user namespace, keeping the
/// namespace's capabilities: the agent's own host ids under a root
/// gateway, the ids already held otherwise. Like every credential change
/// it clears `PR_SET_PDEATHSIG`.
///
/// # Safety
///
/// In the agent's user namespace, single-threaded.
pub unsafe fn become_namespace_root() -> io::Result<()> {
    // SAFETY: plain credential calls.
    unsafe {
        if libc::setresgid(0, 0, 0) != 0 || libc::setresuid(0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Host paths the agent's rootfs binds, opened while `havn-init` still
/// has the gateway's host uid. The agent's own uid may not be able to walk
/// to them (a workspace under `/root`); a bind from the open handle
/// (`/proc/self/fd/<n>`) does not walk the path again.
#[derive(Debug, Default)]
pub struct HostPaths {
    opened: Vec<(PathBuf, OwnedFd)>,
}

impl HostPaths {
    /// Open each path that exists, following symlinks as a bind would. A
    /// missing one is left out: binding it then fails, or, for an extra
    /// mount, is skipped.
    pub fn open<'a>(paths: impl IntoIterator<Item = &'a Path>) -> io::Result<Self> {
        let mut opened = Vec::new();
        for path in paths {
            let c_path = CString::new(path.as_os_str().as_bytes()).map_err(io_invalid)?;
            // SAFETY: a valid C string; O_PATH opens without reading.
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENOENT) {
                    continue;
                }
                return Err(io::Error::new(
                    err.kind(),
                    format!("opening {}: {err}", path.display()),
                ));
            }
            // SAFETY: `fd` was just opened and nothing else owns it.
            opened.push((path.to_path_buf(), unsafe { OwnedFd::from_raw_fd(fd) }));
        }
        Ok(Self { opened })
    }

    fn contains(&self, path: &Path) -> bool {
        self.opened.iter().any(|(p, _)| p == path)
    }

    /// What to bind for `path`: its open handle, or the path itself.
    fn source(&self, path: &Path) -> io::Result<CString> {
        match self.opened.iter().find(|(p, _)| p == path) {
            Some((_, fd)) => {
                CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(io_invalid)
            }
            None => CString::new(path.as_os_str().as_bytes()).map_err(io_invalid),
        }
    }
}

/// # Safety
///
/// Must be called in the child between fork and exec, before
/// [`unshare_namespaces`], so the write runs with the gateway's own host
/// credentials (the same ones the cgroup was created with).
pub unsafe fn join_cgroup(cgroup_procs: &CStr) -> io::Result<()> {
    // SAFETY: same contract as `write_proc_file`; writing "0" moves the
    // calling process into the cgroup that owns `cgroup.procs`.
    unsafe { write_proc_file(cgroup_procs, b"0") }
}

/// Have the kernel SIGKILL the calling process when its parent dies.
/// Strictly the kernel watches the parent *thread* that forked us; the
/// gateway spawns from tokio worker threads, which live as long as the
/// gateway (nothing in it calls `block_in_place`, which can retire one).
/// `expected_parent` closes the race where the parent died before this
/// call; pass `None` inside a new PID namespace, where the parent is not
/// visible. Async-signal-safe, so callable between fork and exec, and
/// preserved across exec of a binary without set-id bits or file caps.
pub fn kill_with_parent(expected_parent: Option<libc::pid_t>) -> io::Result<()> {
    // SAFETY: PR_SET_PDEATHSIG takes one signal number; the trailing
    // arguments are ignored and passed as zero.
    let rc = unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getppid has no preconditions.
    if expected_parent.is_some_and(|parent| unsafe { libc::getppid() } != parent) {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

/// Start a new session, so the calling process leaves the gateway's
/// process group and controlling terminal. A Ctrl-C in the terminal that
/// runs the gateway then reaches the gateway alone, which lets its agents
/// finish their work before stopping them, instead of the agents too.
/// Async-signal-safe, so callable between fork and exec, where the child
/// is never a process group leader and `setsid` cannot fail with EPERM.
pub fn leave_terminal_session() -> io::Result<()> {
    // SAFETY: setsid has no preconditions.
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Tells a forked child whether its parent is still alive where `getppid`
/// cannot (the parent is outside the child's PID namespace): the parent
/// keeps the write end open for as long as it lives.
#[derive(Debug)]
pub struct ParentLifeline {
    read: libc::c_int,
    write: libc::c_int,
}

impl ParentLifeline {
    /// Create before `fork`.
    pub fn new() -> io::Result<Self> {
        let mut fds: [libc::c_int; 2] = [-1; 2];
        // SAFETY: `fds` is a valid two-element array for pipe2 to fill.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read: fds[0],
            write: fds[1],
        })
    }

    /// When `fork` failed.
    pub fn close(self) {
        // SAFETY: closing both ends this value owns.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }

    /// Parent side after `fork`: the write end stays open until this
    /// process exits.
    pub fn hold(self) {
        // SAFETY: closing the read end this value owns.
        unsafe { libc::close(self.read) };
    }

    /// Child side after `fork`: false once the parent has exited. Call it
    /// after `PR_SET_PDEATHSIG`, which covers the parent dying later.
    pub fn parent_alive(self) -> bool {
        // SAFETY: closing the write end this value owns; the parent's copy
        // is now the only one, so its exit hangs up the pipe.
        unsafe { libc::close(self.write) };
        let mut entry = libc::pollfd {
            fd: self.read,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, zero timeout.
        let rc = unsafe { libc::poll(&raw mut entry, 1, 0) };
        // SAFETY: closing the read end this value owns.
        unsafe { libc::close(self.read) };
        rc == 0
    }
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
/// host's `/tmp`, the host's `/proc`. Landlock (applied by `havn-init`
/// after this, before exec) is the primary access control; `pivot_root` removes the
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
    host: &HostPaths,
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
        // were loaded from the system"). The egress gate is the netns
        // (no veth = no packets out), not the absence of CA certs.
        //
        // Only `/etc/ssl/certs`, read-only: the rest of `/etc/ssl` holds
        // `private/`, where hosts keep TLS keys. The certificates there link
        // into `/usr/share/ca-certificates`, already under the /usr bind.
        // A read-only remount that fails stops the agent from starting.
        let ca_certificates = Path::new("/etc/ssl/certs");
        if ca_certificates.is_dir() {
            mirror_bind_into_newroot(host, ca_certificates, true)?;
        } else {
            eprintln!(
                "havn-init: no {}; HTTPS will fail in the agent",
                ca_certificates.display()
            );
        }

        // 9. Mirror-bind workspace (rw) at its host path so the
        //    runtime's `--workspace <abs-path>` argument resolves
        //    identically inside the new rootfs.
        mirror_bind_into_newroot(host, workspace_dir, false)?;

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
            mirror_bind_into_newroot(host, parent, true)?;
        }

        // 9c. Mirror-bind the gateway's Unix socket file at its host
        //     path. The runtime opens it via `UnixStream::connect`
        //     immediately after startup; without this bind the
        //     connect() returns ENOENT post-pivot. Single-file bind —
        //     creates an empty placeholder at the target then binds.
        mirror_bind_file_into_newroot(host, gateway_socket)?;

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
            if !host.contains(&mount.path) {
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
            mirror_bind_into_newroot(host, &mount.path, read_only)?;
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
unsafe fn mirror_bind_into_newroot(
    host: &HostPaths,
    host_path: &Path,
    read_only: bool,
) -> io::Result<()> {
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
    let source = host.source(host_path)?;
    // SAFETY: src/dest are valid CStrings; null fstype/data with MS_BIND
    // is the canonical bind invocation.
    unsafe {
        check(libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ))?;
        restrict_bind(&target, read_only)?;
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
unsafe fn mirror_bind_file_into_newroot(host: &HostPaths, host_path: &Path) -> io::Result<()> {
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
    let source = host.source(host_path)?;
    // SAFETY: bind-mount file onto file is well-defined; both pointers valid.
    unsafe {
        check(libc::mount(
            source.as_ptr(),
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

/// Bind `src` onto `dest` read-only and `nosuid`. The 2-step is required
/// because the initial `MS_BIND` ignores those flags (kernel compatibility
/// quirk).
///
/// # Safety
///
/// As [`restrict_bind`].
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
        restrict_bind(dest, true)
    }
}

/// `struct mount_attr` of `mount_setattr(2)`; libc does not define it.
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const MOUNT_ATTR_RDONLY: u64 = 0x1;
const MOUNT_ATTR_NOSUID: u64 = 0x2;
/// `ST_RELATIME` and `ST_NOSYMFOLLOW` of statvfs(3); libc defines neither
/// for musl.
const ST_RELATIME: libc::c_ulong = 0x1000;
const ST_NOSYMFOLLOW: libc::c_ulong = 0x2000;

/// Make the bind mount at `dest` and every mount below it `nosuid`, and
/// read-only when `read_only`. `nosuid` keeps setuid bits and file
/// capabilities from taking effect in the agent: an agent process holds
/// no capabilities, so exec'ing a binary with file capabilities (`ping` on
/// many distributions) would otherwise fail with `EPERM`; without them it
/// falls back to what an unprivileged user can do.
///
/// Uses `mount_setattr` (Linux 5.12+), which only adds the two flags and
/// so keeps every other one, locked ones included. Where it is missing or
/// filtered, a bind remount of `dest` alone repeats the flags it has, since
/// in a user namespace the kernel refuses a remount that drops a locked
/// one.
///
/// # Safety
///
/// Only `mount_setattr`, `statvfs` and `mount`, so safe in a forked child.
unsafe fn restrict_bind(dest: &CStr, read_only: bool) -> io::Result<()> {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_NOSUID | if read_only { MOUNT_ATTR_RDONLY } else { 0 },
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: dest is a valid C string and attr a valid mount_attr of the
    // size passed.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            dest.as_ptr(),
            libc::AT_RECURSIVE,
            &raw const attr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    // ENOSYS before Linux 5.12; EPERM from a seccomp profile that predates
    // the call (container runtimes, systemd SystemCallFilter).
    if !matches!(err.raw_os_error(), Some(libc::ENOSYS | libc::EPERM)) {
        return Err(err);
    }
    // SAFETY: a zeroed statvfs is a valid out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: dest is a valid C string and st a valid out-pointer.
    unsafe { check(libc::statvfs(dest.as_ptr(), &raw mut st)) }?;
    let has = |flag: libc::c_ulong| st.f_flag & flag != 0;
    let mut flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_NOSUID;
    for (st_flag, ms_flag) in [
        (libc::ST_RDONLY, libc::MS_RDONLY),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (ST_RELATIME, libc::MS_RELATIME),
        (ST_NOSYMFOLLOW, libc::MS_NOSYMFOLLOW),
    ] {
        if has(st_flag) {
            flags |= ms_flag;
        }
    }
    // statvfs reports no flag for strictatime; without one the remount
    // would switch the mount to relatime.
    if !has(libc::ST_NOATIME) && !has(ST_RELATIME) {
        flags |= libc::MS_STRICTATIME;
    }
    if read_only {
        flags |= libc::MS_RDONLY;
    }
    // SAFETY: dest is a valid C string; a bind remount takes no source or data.
    unsafe {
        check(libc::mount(
            std::ptr::null(),
            dest.as_ptr(),
            std::ptr::null(),
            flags,
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

/// Securebits that stop uid 0 from meaning anything to capabilities, each
/// locked so no descendant can unset it: exec as uid 0 grants nothing
/// (`NOROOT`), uid changes leave capabilities alone (`NO_SETUID_FIXUP`),
/// and ambient capabilities cannot be raised. `KEEP_CAPS` stays settable:
/// with no capabilities to keep and `NO_SETUID_FIXUP` locked it changes
/// nothing, and tools such as `ping` exit when they cannot set it.
const LOCKED_NO_ROOT_SECUREBITS: libc::c_ulong = (1 << 0) // SECBIT_NOROOT
    | (1 << 1) // SECBIT_NOROOT_LOCKED
    | (1 << 2) // SECBIT_NO_SETUID_FIXUP
    | (1 << 3) // SECBIT_NO_SETUID_FIXUP_LOCKED
    | (1 << 6) // SECBIT_NO_CAP_AMBIENT_RAISE
    | (1 << 7); // SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED

/// `_LINUX_CAPABILITY_VERSION_3`: 64-bit sets, as two 32-bit words.
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapabilityWords {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Make the agent's own network namespace usable without capabilities:
/// bring up its loopback, so tools can serve and reach `localhost` with or
/// without egress, and let them ping and bind ports below 1024. All of it
/// belongs to that namespace alone: the host's settings are untouched, and
/// the agent reaches only what its routes allow. Needs the namespace's
/// `CAP_NET_ADMIN`, so it runs before [`drop_all_capabilities`], after
/// `/proc` is mounted.
///
/// # Safety
///
/// Uses only `socket`/`ioctl`/`open`/`write`/`close`, so it is safe in a
/// forked child of a multi-threaded process.
pub unsafe fn prepare_agent_network() -> io::Result<()> {
    // SAFETY: documented contract of `loopback_up`.
    let loopback = unsafe { loopback_up() };
    // Gid 0, the only gid mapped in the agent's user namespace (the kernel
    // refuses a range with unmapped gids), may open ICMP datagram sockets,
    // which is what `ping` uses.
    // SAFETY: constant proc paths and byte strings.
    let ping = unsafe { write_proc_file(c"/proc/sys/net/ipv4/ping_group_range", b"0 0") };
    // SAFETY: as above.
    let ports = unsafe { write_proc_file(c"/proc/sys/net/ipv4/ip_unprivileged_port_start", b"0") };
    loopback.and(ping).and(ports)
}

// `ioctl`'s request is a `c_ulong` with glibc and a `c_int` with musl.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::unnecessary_cast,
    reason = "the request numbers fit both types"
)]
const IOCTL_GET_FLAGS: libc::Ioctl = libc::SIOCGIFFLAGS as libc::Ioctl;
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::unnecessary_cast,
    reason = "the request numbers fit both types"
)]
const IOCTL_SET_FLAGS: libc::Ioctl = libc::SIOCSIFFLAGS as libc::Ioctl;

/// Set `IFF_UP` on `lo` in the calling process's network namespace.
///
/// # Safety
///
/// Only `socket`, `ioctl` and `close`.
unsafe fn loopback_up() -> io::Result<()> {
    // SAFETY: plain socket creation.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a zeroed ifreq is valid; the name fits with its NUL.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(b"lo") {
        *slot = libc::c_char::try_from(*byte).unwrap_or_default();
    }
    // SAFETY: fd is a socket and req a valid ifreq for both requests.
    let result = unsafe {
        check(libc::ioctl(fd, IOCTL_GET_FLAGS, &raw mut req)).and_then(|()| {
            #[allow(clippy::cast_possible_truncation, reason = "IFF_UP fits in a short")]
            {
                req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            }
            check(libc::ioctl(fd, IOCTL_SET_FLAGS, &raw const req))
        })
    };
    // SAFETY: closing the socket opened above.
    unsafe { libc::close(fd) };
    result
}

/// Give up every capability in the agent's user namespace for good: no
/// `CAP_DAC_OVERRIDE` on files it can see, no `CAP_SYS_PTRACE`, nothing
/// regained by exec'ing a root-owned or file-capability binary. A user
/// namespace the agent creates starts with its own full set, confined to
/// that namespace.
/// Called by `havn-init` last before it execs the runtime, after the
/// mounts that need the namespace's capabilities.
///
/// Locks the securebits first (needs `CAP_SETPCAP`), then empties the
/// bounding set (also needs it), the ambient set, and finally the
/// effective, permitted and inheritable sets, and checks the bounding set
/// is empty.
///
/// # Safety
///
/// Each step affects the calling thread only, so the process must be
/// single-threaded (havn-init's child after `fork` is).
pub unsafe fn drop_all_capabilities() -> io::Result<()> {
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_SECUREBITS, LOCKED_NO_ROOT_SECUREBITS, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for cap in 0..=63_u32 {
        // SAFETY: prctl with integer arguments only; the kernel reads the
        // capability as an unsigned long.
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, libc::c_ulong::from(cap), 0, 0, 0) } != 0 {
            let err = io::Error::last_os_error();
            // EINVAL: past the kernel's last capability.
            if err.raw_os_error() == Some(libc::EINVAL) && cap > 0 {
                break;
            }
            return Err(err);
        }
    }
    for cap in 0..=63_u32 {
        // SAFETY: prctl with integer arguments only.
        match unsafe { libc::prctl(libc::PR_CAPBSET_READ, libc::c_ulong::from(cap), 0, 0, 0) } {
            0 => {}
            1 => {
                return Err(io::Error::other(format!(
                    "capability {cap} is still in the bounding set"
                )));
            }
            _ => break,
        }
    }
    // SAFETY: prctl with integer arguments only.
    if unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let empty = [CapabilityWords {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: capset reads one header and two capability words, both live
    // for the call; pid 0 is the calling thread.
    if unsafe { libc::syscall(libc::SYS_capset, &raw mut header, empty.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    fn child_is_killed_when_the_thread_that_spawned_it_exits() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        let parent = std::process::id().cast_signed();
        let mut child = std::thread::spawn(move || {
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30");
            // SAFETY: the hook only makes async-signal-safe calls.
            unsafe {
                cmd.pre_exec(move || kill_with_parent(Some(parent)));
            }
            cmd.spawn().expect("spawn sleep")
        })
        .join()
        .expect("spawning thread");
        let status = child.wait().expect("wait");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
    }

    #[test]
    fn lifeline_reports_whether_another_write_end_is_open() {
        let lifeline = ParentLifeline::new().expect("pipe");
        // SAFETY: duplicating a valid fd; the copy stands in for the parent.
        let parent = unsafe { libc::dup(lifeline.write) };
        assert!(parent >= 0);
        assert!(lifeline.parent_alive());
        // SAFETY: closing the duplicate made above.
        unsafe { libc::close(parent) };

        let orphaned = ParentLifeline::new().expect("pipe");
        assert!(!orphaned.parent_alive(), "no parent holds the write end");
    }

    #[test]
    fn spawn_fails_when_the_parent_is_not_the_expected_one() {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new("true");
        // SAFETY: the hook only makes async-signal-safe calls.
        unsafe {
            cmd.pre_exec(|| kill_with_parent(Some(i32::MAX)));
        }
        let err = cmd.spawn().expect_err("parent check must fail");
        assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn proc_pid_paths_are_built_without_allocating() {
        let mut buf = [0u8; 48];
        assert_eq!(
            proc_pid_path(&mut buf, 4_194_304, b"uid_map").map(CStr::to_bytes),
            Some(&b"/proc/4194304/uid_map"[..])
        );
        assert_eq!(
            proc_pid_path(&mut buf, 7, b"setgroups").map(CStr::to_bytes),
            Some(&b"/proc/7/setgroups"[..])
        );
        assert!(proc_pid_path(&mut buf, -1, b"uid_map").is_none());
        assert!(proc_pid_path(&mut buf, 1, &[b'x'; 40]).is_none());
    }

    /// As root only: a child maps the namespace's root to an id of its
    /// own and, once it switches to it, holds that id on the host.
    #[test]
    fn a_root_child_runs_as_its_own_host_id() {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let id: u32 = 0x7100_0000 + 4_000_000;
        let uid_map = uid_map_line(id);
        let gid_map = gid_map_line(id);
        let mut cmd = std::process::Command::new("cat");
        cmd.arg("/proc/self/uid_map");
        // SAFETY: the hook makes only the calls each helper documents.
        unsafe {
            cmd.pre_exec(move || {
                clear_supplementary_groups()?;
                unshare_with_host_ids(uid_map.as_bytes(), gid_map.as_bytes())?;
                become_namespace_root()
            });
        }
        let out = cmd.output().expect("run");
        assert!(out.status.success(), "{out:?}");
        let map = String::from_utf8_lossy(&out.stdout);
        let fields: Vec<&str> = map.split_whitespace().collect();
        let host = id.to_string();
        assert_eq!(fields, ["0", host.as_str(), "1"], "{map}");
    }

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
