//! Linux-namespace + Landlock + seccomp + cgroup spawner (spec §4.1).
//!
//! Current isolation layers:
//!
//! - **cgroup v2** — per-agent cgroup under
//!   `/sys/fs/cgroup/havn.slice/agent-<id>` with the policy's memory / CPU /
//!   pids limits. `stop()` falls back to `cgroup.kill` for atomic
//!   subtree termination (fork-bomb safe).
//! - **User + mount + network + PID namespaces** — `unshare(CLONE_NEWUSER
//!   | CLONE_NEWNS | CLONE_NEWNET | CLONE_NEWPID)` in `pre_exec`, host UID
//!   mapped to agent's in-namespace UID 0, mount propagation marked
//!   `MS_PRIVATE` so the agent's mount changes can't escape, network
//!   limited to the new namespace's loopback. After unshare, `pre_exec`
//!   forks: the parent stays in the host PID ns and `_exit`s with the
//!   child's status; the child is PID 1 in the new PID ns, mounts a fresh
//!   `proc`, and exec's the runtime. Combined with the user ns this means
//!   the agent's `ps -ef` / `ls /proc` shows nothing from the host or
//!   other agents (spec §4.1).
//! - **Landlock + seccomp** — applied inside the runtime itself
//!   (`havn-runtime/src/{sandbox,seccomp}.rs`) right before the LLM loop
//!   starts, so the syscall + filesystem surfaces are restricted in
//!   addition to the namespace boundary.
//!
//! Still deferred (spec §4.1): `pivot_root` to a minimal rootfs. Landlock
//! covers the same threat model in practice; `pivot_root` is defence in
//! depth on top of it.
//!
//! Compared to [`crate::subprocess::SubprocessSpawner`], this spawner
//! requires either root or systemd cgroup delegation
//! (`Delegate=yes` in the gateway's unit file). [`NamespaceSpawner::try_init`]
//! probes for that at startup and returns an error so the caller can fall
//! back to the unprivileged subprocess spawner with a clear log.

use std::collections::HashMap;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use havn_core::{AgentId, Policy};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::cgroup::{CgroupController, CgroupError};
use crate::ns_setup;
use crate::{AgentHandle, AgentSpawnConfig, AgentSpawner, ResourceStats, Result, SpawnerError};

const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct NamespaceSpawner {
    cgroup: CgroupController,
    state: Arc<Mutex<HashMap<AgentId, AgentState>>>,
    pool: crate::network::AgentSubnetPool,
}

#[derive(Debug)]
struct AgentState {
    child: Child,
    cgroup_path: PathBuf,
}

impl NamespaceSpawner {
    /// Probe the host for cgroup v2 + havn.slice writability. Returns an
    /// error if either is missing — the gateway should fall back to
    /// [`crate::subprocess::SubprocessSpawner`] with a warn log.
    pub fn try_init() -> Result<Self> {
        Self::try_init_with_pool(crate::network::AgentSubnetPool::default())
    }

    /// Variant of [`try_init`] that takes an explicit per-agent network
    /// pool. Used by the gateway when `[network] agent_pool_cidr` is
    /// configured to something other than the default.
    pub fn try_init_with_pool(pool: crate::network::AgentSubnetPool) -> Result<Self> {
        let cgroup = CgroupController::system()
            .map_err(|e| SpawnerError::IsolationUnavailable(e.to_string()))?;
        info!(
            slice = %cgroup.slice().display(),
            pool_cidr = %pool.cidr(),
            "NamespaceSpawner cgroup v2 ready"
        );
        Ok(Self {
            cgroup,
            state: Arc::new(Mutex::new(HashMap::new())),
            pool,
        })
    }

    /// Inject a controller (typically tempdir-rooted) for tests. Skipped from
    /// docs because production paths must go through `try_init`.
    #[doc(hidden)]
    pub fn with_controller(cgroup: CgroupController) -> Self {
        Self {
            cgroup,
            state: Arc::new(Mutex::new(HashMap::new())),
            pool: crate::network::AgentSubnetPool::default(),
        }
    }
}

#[async_trait]
impl AgentSpawner for NamespaceSpawner {
    #[allow(
        clippy::similar_names,
        clippy::too_many_lines,
        reason = "host_uid / host_gid is the canonical spelling for these Linux primitives"
    )]
    async fn spawn(&self, config: &AgentSpawnConfig, policy: &Policy) -> Result<AgentHandle> {
        // 1. Workspace must exist before exec — the runtime opens agent.db
        //    in it on startup.
        tokio::fs::create_dir_all(&config.workspace_dir).await?;

        // 2. Create the cgroup with policy limits BEFORE spawning so we
        //    never have an unconstrained PID in flight.
        let cgroup_path = self
            .cgroup
            .create_agent(&config.agent_id.to_string(), &policy.resource_limits)
            .map_err(|e| map_cgroup_err(&e))?;

        // 3. Build the spawn command. Two-stage launch:
        //    a. exec `havn-init <runtime> <runtime-args>`. The pre_exec
        //       hook below runs in this process; it puts everything from
        //       here on into fresh user/mount/net/pid namespaces.
        //    b. `havn-init` then forks: one side becomes PID 1 in the new
        //       PID namespace, mounts a fresh `/proc`, and exec's the
        //       runtime; the other side waits + `_exit`s with the
        //       runtime's status. We can't do that fork in `pre_exec`
        //       directly because std's `Command::spawn` uses a cloexec
        //       pipe to detect exec success and would block until our
        //       runtime exited.
        //
        //    NamespaceSpawner requires `init_binary` to be set; the
        //    gateway's startup wires it from the same directory as the
        //    runtime binary. Fail loudly if it's missing.
        let init_binary = config.init_binary.as_ref().ok_or_else(|| {
            SpawnerError::Spawn(
                "NamespaceSpawner requires init_binary to be configured (havn-init); \
                 install.sh ships it next to havn-runtime"
                    .into(),
            )
        })?;
        let mut cmd = Command::new(init_binary);
        cmd.arg(&config.runtime_binary)
            .arg("--agent-id")
            .arg(config.agent_id.to_string())
            .arg("--gateway-socket")
            .arg(&config.gateway_socket)
            .arg("--workspace")
            .arg(&config.workspace_dir);
        if config.is_subagent {
            cmd.arg("--is-subagent");
        }
        // Operator-declared extra bind-mounts (spec §4.1 v0.7). Encoded
        // as `path:mode,path:mode,...` so havn-init can parse without
        // serde_json (it has no logging stack — diagnostics go to
        // stderr). Empty list → empty string → havn-init no-ops.
        if !config.extra_mounts.is_empty() {
            cmd.env(
                "HAVN_EXTRA_MOUNTS",
                crate::encode_extra_mounts(&config.extra_mounts),
            );
        }
        // Operator-declared in-namespace tmpfs mounts (spec §16.2).
        // havn-init mounts them inside the new rootfs after pivot.
        if !config.tmpfs_mounts.is_empty() {
            cmd.env(
                "HAVN_TMPFS_MOUNTS",
                crate::encode_tmpfs_mounts(&config.tmpfs_mounts),
            );
        }
        // Operator-declared seccomp allowances (spec §16.2). The
        // runtime — not havn-init — is the consumer (seccomp is applied
        // post-exec by the runtime, see havn-runtime/src/seccomp.rs).
        // We forward via env anyway so the data path stays uniform with
        // the other primitives; the runtime reads it on startup. The
        // spawner does not validate names — that's the runtime's job
        // against `libc::SYS_*`.
        if !config.seccomp_allow_extra.is_empty() {
            cmd.env(
                "HAVN_SECCOMP_ALLOW_EXTRA",
                crate::encode_seccomp_allow_extra(&config.seccomp_allow_extra),
            );
        }
        // Network egress flag (spec §6.3 v0.7). Single source of truth
        // is `policy.network_policy.egress_allowed`. havn-init gates
        // /etc/resolv.conf + /etc/ssl writes on it; this spawner gates
        // `configure_agent_network` (step 5 below) on the same bit.
        // Default off — spec §1.5 says "agent processes never see API
        // keys" and the original posture is "no direct outbound,
        // gateway proxies LLM calls"; per-role opt-in restores that
        // narrative.
        if policy.network_policy.egress_allowed {
            cmd.env("HAVN_NETWORK_OUTBOUND", "1");
            // DNS server list operator declared in `[network] agent_dns`.
            // Comma-separated, parsed by havn-init. Empty list → havn-init
            // skips the resolv.conf write entirely (DNS will fail), but
            // GatewayConfig defaults to public resolvers so this should
            // never be empty in practice.
            if !config.agent_dns.is_empty() {
                cmd.env("HAVN_AGENT_DNS", config.agent_dns.join(","));
            }
        }
        cmd.kill_on_drop(true);

        // SAFETY: the closure runs between fork and exec in the child. It
        //   - calls `libc::unshare` (async-signal-safe),
        //   - writes to `/proc/self/{setgroups,uid_map,gid_map}` via
        //     [`crate::ns_setup::write_user_namespace_maps`] (raw libc I/O,
        //     no allocator paths),
        //   - calls `libc::mount` (async-signal-safe).
        // It does perform a `format!` for the map lines; in practice the
        // gateway's allocator does not hold cross-thread locks across fork,
        // so this is sound for our deployment. If we ever hit deadlocks, swap
        // for stack-buffer formatting and the code in `ns_setup` is the only
        // file to update.
        //
        // Note: PID namespace was unshared above (spec §4.1), but the kernel
        // semantic is "next fork enters the new PID ns" — the calling
        // process stays in the old one. We can't fork-and-block here
        // because std's `Command::spawn` uses a cloexec pipe to detect
        // exec success; if we never exec from this process, the pipe
        // never closes and `spawn()` blocks until our child exits.
        // Instead `cmd` below is set to exec `havn-init`, which forks
        // inside its own main: one side becomes PID 1 in the new ns and
        // exec's the runtime, the other side waits + `_exit`s.
        // SAFETY: `getuid` / `getgid` are async-signal-safe and infallible.
        let host_uid = unsafe { libc::getuid() };
        let host_gid = unsafe { libc::getgid() };
        // SAFETY: pre_exec runs in the child after fork, before exec; it
        // restricts itself to the contracts spelled out in `ns_setup`.
        #[allow(
            clippy::similar_names,
            reason = "uid/gid is the canonical spelling for these Linux primitives"
        )]
        unsafe {
            cmd.as_std_mut().pre_exec(move || {
                ns_setup::unshare_namespaces()?;
                ns_setup::write_user_namespace_maps(host_uid, host_gid)?;
                ns_setup::mark_mounts_private()?;
                Ok(())
            });
        }

        let child = cmd.spawn().map_err(|e| {
            // Clean up the cgroup we just created — it's empty so rmdir is fine.
            let _ = self.cgroup.cleanup(&cgroup_path);
            SpawnerError::Spawn(format!("exec {}: {e}", config.runtime_binary.display()))
        })?;
        let pid = child
            .id()
            .ok_or_else(|| SpawnerError::Spawn("child has no pid".into()))?;

        // 4. Place the child in the cgroup. There's a tiny window between
        //    spawn and this write where the child runs unconstrained, but
        //    it's strictly bounded by the kernel scheduler — sub-millisecond.
        if let Err(e) = self.cgroup.add_pid(&cgroup_path, pid) {
            warn!(error = %e, "failed to add agent PID to cgroup; killing child to avoid runaway");
            let _ = self.cgroup.kill_all(&cgroup_path);
            let _ = self.cgroup.cleanup(&cgroup_path);
            return Err(map_cgroup_err(&e));
        }

        // 5. Per-agent veth + NAT setup (spec §4.1, §6.3 v0.7).
        //
        //    **Gated on `policy.network_policy.egress_allowed`** (default
        //    false). The pre_exec hook above already unshared
        //    CLONE_NEWNET, so an agent without egress sits in an empty
        //    netns with only `lo` (down) — credential-proxy paths over
        //    the Unix gateway socket still work, but `web_fetch`, MCP
        //    servers that reach the internet, and capability packs like
        //    havn-pack-browser stay offline. This preserves the spec
        //    §1.5 / §11 security narrative: agents do not directly
        //    speak to the internet by default; the operator opts in
        //    per-role.
        //
        //    **Non-blocking on purpose.** Even when egress is allowed,
        //    we don't await the 4–5 ip / nsenter shellouts (~50–100ms)
        //    before returning the AgentHandle, because the runtime —
        //    which has already exec'd in parallel — would otherwise
        //    race the gateway's `registry.register(handle)` call
        //    (called by the lifecycle endpoint after spawn returns).
        //    The runtime's first action is Hello over the Unix domain
        //    socket, which doesn't need netns; its first **outbound
        //    HTTP** doesn't happen until a user message triggers a
        //    tool call. **This ordering invariant is documented** in
        //    spec §16 — startup-phase code MUST NOT assume the agent
        //    netns is fully configured. If a future startup hook
        //    needs the network ready, add an explicit
        //    `await network_ready()` await point in the runtime; do
        //    not block here.
        //
        //    Failure is non-fatal: tools needing outbound surface
        //    their own clear errors at tool-use time.
        if policy.network_policy.egress_allowed {
            let net_pid = i32::try_from(pid).unwrap_or(0);
            let net_agent_id = config.agent_id;
            let net_pool = self.pool;
            tokio::task::spawn_blocking(move || {
                match crate::network::configure_agent_network(net_pid, net_agent_id, &net_pool) {
                    Ok(net) => {
                        info!(
                            agent_id = %net_agent_id,
                            host_iface = %net.host_iface,
                            ns_ip = %net.ns_ip,
                            "agent veth configured (egress_allowed=true)"
                        );
                    }
                    Err(e) => {
                        warn!(
                            agent_id = %net_agent_id,
                            error = %e,
                            "veth/NAT setup failed; agent will have no outbound network"
                        );
                    }
                }
            });
        } else {
            info!(
                agent_id = %config.agent_id,
                "agent ns left without outbound network (policy.network_policy.egress_allowed=false)"
            );
        }

        info!(
            agent_id = %config.agent_id,
            pid,
            cgroup = %cgroup_path.display(),
            "agent started in cgroup"
        );

        self.state
            .lock()
            .await
            .insert(config.agent_id, AgentState { child, cgroup_path });

        Ok(AgentHandle {
            agent_id: config.agent_id,
            pid,
        })
    }

    async fn stop(&self, handle: &AgentHandle) -> Result<()> {
        let AgentState {
            mut child,
            cgroup_path,
        } = {
            let mut guard = self.state.lock().await;
            guard
                .remove(&handle.agent_id)
                .ok_or(SpawnerError::NotFound(handle.agent_id))?
        };

        // 1. Try graceful SIGTERM first. libc::kill is the same call the
        //    SubprocessSpawner uses; we need a separate file because tokio's
        //    Child only exposes start_kill (= SIGKILL).
        //
        //    `handle.pid` is the OUTER reaper (in the host PID ns); its
        //    signal handler forwards SIGTERM to the inner runtime (PID 1
        //    in the new ns) — see `ns_setup::install_signal_forwarder`.
        //    The inner runs its graceful shutdown, exits, and the outer
        //    waitpid's then `_exit`s with the same status.
        send_sigterm(handle.pid);

        // 2. Wait STOP_GRACE for the OUTER to exit on its own. A clean exit
        //    here means the inner already shut down gracefully and the
        //    outer reaper finished. A timeout means either the inner is
        //    stuck or the signal forwarder did not deliver — either way
        //    we fall through to cgroup.kill.
        let timed_out = match tokio::time::timeout(STOP_GRACE, child.wait()).await {
            Ok(Ok(status)) => {
                info!(agent_id = %handle.agent_id, ?status, "outer reaper exited after SIGTERM");
                false
            }
            Ok(Err(e)) => {
                warn!(agent_id = %handle.agent_id, error = %e, "wait failed; proceeding to cgroup.kill");
                false
            }
            Err(_) => {
                warn!(
                    agent_id = %handle.agent_id,
                    "SIGTERM grace expired; will hammer cgroup subtree"
                );
                true
            }
        };

        // 3. Backstop: ALWAYS cgroup.kill the whole subtree. Even when the
        //    outer reaper exited cleanly above, the inner runtime (and any
        //    grandchild of its shell tool) lives in the same cgroup but is
        //    invisible to `child.wait()` — the gateway only ever held a
        //    handle on the outer. Without this hammer the inner would
        //    survive `stop()`, leak resources, and the subsequent
        //    `cleanup()` would fail with EBUSY because the cgroup still
        //    has live tasks.
        if let Err(e) = self.cgroup.kill_all(&cgroup_path) {
            warn!(error = %e, "cgroup.kill backstop failed");
        }
        if timed_out {
            // Drain the wait future so we don't leave a zombie on our side.
            let _ = child.wait().await;
        }

        // 4. cgroup is now empty — rmdir.
        if let Err(e) = self.cgroup.cleanup(&cgroup_path) {
            warn!(
                cgroup = %cgroup_path.display(),
                error = %e,
                "cgroup cleanup failed; will retry on next gateway restart"
            );
        }

        // 5. Best-effort veth teardown. The kernel reaps the pair when
        //    the agent's netns is destroyed (after the last process in
        //    it exits, which the cgroup.kill above ensures); this `ip
        //    link del` is the belt-and-braces path for the rare case
        //    where the host-side end lingers.
        crate::network::teardown_agent_network(handle.agent_id, &self.pool);
        Ok(())
    }

    async fn stats(&self, handle: &AgentHandle) -> Result<ResourceStats> {
        let cgroup_path = {
            let guard = self.state.lock().await;
            guard
                .get(&handle.agent_id)
                .map(|s| s.cgroup_path.clone())
                .ok_or(SpawnerError::NotFound(handle.agent_id))?
        };
        let snapshot = self
            .cgroup
            .stats(&cgroup_path)
            .map_err(|e| map_cgroup_err(&e))?;
        Ok(ResourceStats {
            memory_bytes: snapshot.memory_bytes,
            cpu_microseconds: snapshot.cpu_microseconds,
            pids: snapshot.pids,
        })
    }
}

fn map_cgroup_err(e: &CgroupError) -> SpawnerError {
    SpawnerError::IsolationUnavailable(e.to_string())
}

#[cfg(unix)]
fn send_sigterm(pid: u32) {
    // SAFETY: same invariants as in the SubprocessSpawner — well-known
    // signal number, ESRCH is benign (child already exited).
    unsafe {
        let _ = libc::kill(pid.cast_signed(), libc::SIGTERM);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::fs::File;
    use std::path::PathBuf;

    fn fake_cgroup_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("havn-ns-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&p).expect("mkdir");
        File::create(p.join("cgroup.controllers")).expect("touch cgroup.controllers");
        p
    }

    #[tokio::test]
    async fn try_init_fails_on_non_v2_root() {
        // Validates the production fallback path: if cgroup v2 isn't there
        // we must surface a clear error so the gateway can fall back.
        let result = CgroupController::with_root(PathBuf::from("/this/does/not/exist"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn spawn_invalid_runtime_cleans_up_cgroup() {
        // Even when the runtime binary is missing, we must not leak an
        // empty cgroup directory.
        let root = fake_cgroup_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        let spawner = NamespaceSpawner::with_controller(ctl);

        let cfg = AgentSpawnConfig {
            agent_id: AgentId::new(),
            workspace_dir: std::env::temp_dir().join(format!("havn-ws-{}", uuid::Uuid::now_v7())),
            runtime_binary: PathBuf::from("/bin/this-binary-does-not-exist"),
            init_binary: Some(PathBuf::from("/bin/this-binary-does-not-exist")),
            gateway_socket: std::env::temp_dir().join("havn-fake.sock"),
            is_subagent: false,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
            agent_dns: Vec::new(),
        };
        let policy = Policy::default();

        let err = spawner.spawn(&cfg, &policy).await.expect_err("should fail");
        assert!(matches!(err, SpawnerError::Spawn(_)), "{err:?}");

        // No `agent-*` directories should be left behind.
        let slice = root.join("havn.slice");
        let mut entries = std::fs::read_dir(&slice).expect("readdir");
        let leftover_count = entries
            .by_ref()
            .filter(|e| e.as_ref().map(|x| x.path().is_dir()).unwrap_or(false))
            .count();
        assert_eq!(
            leftover_count, 0,
            "expected no leftover cgroup dirs after failed spawn"
        );
    }
}
