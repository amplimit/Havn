//! Linux-namespace + Landlock + seccomp + cgroup spawner (spec §4.1).
//!
//! Current isolation layers:
//!
//! - **cgroup v2** — per-agent cgroup under
//!   `/sys/fs/cgroup/havn/agent-<id>` with the policy's memory / CPU /
//!   pids limits. `stop()` falls back to `cgroup.kill` for atomic
//!   subtree termination (fork-bomb safe). Every spawn starts from an
//!   empty cgroup: one left over from an earlier instance is killed and
//!   removed first, and [`NamespaceSpawner::reap_orphans`] clears those a
//!   previous gateway run left behind, including the ones gateways before
//!   #66 put under `/sys/fs/cgroup/havn.slice`.
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
//! - **Landlock** — rules computed here ([`crate::filesystem_rules`]) and
//!   applied by `havn-init` right before it execs the runtime, so every
//!   runtime thread and tool process inherits them.
//! - **seccomp** — installed by the runtime on all of its threads (TSYNC)
//!   before the LLM loop starts (`havn-runtime/src/seccomp.rs`).
//! - **Minimal rootfs** — `havn-init` `pivot_root`s into it before exec
//!   ([`crate::ns_setup::pivot_root_into_minimal_rootfs`]).
//!
//! Compared to [`crate::subprocess::SubprocessSpawner`], this spawner
//! requires root (it creates `havn` at the cgroup root and moves
//! agents into it). [`NamespaceSpawner::try_init`]
//! probes for that at startup and returns an error so the caller can fall
//! back to the unprivileged subprocess spawner with a clear log.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use havn_core::{AgentId, Policy};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::cgroup::{CgroupController, CgroupError, RETIRED_PARENT, SliceUnit};
use crate::ns_setup;
use crate::{
    AgentExit, AgentHandle, AgentSpawnConfig, AgentSpawner, ChildWatch, ResourceStats, Result,
    SpawnerError,
};

const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long `cgroup.kill` may take to empty a cgroup before giving up.
const CGROUP_DRAIN: Duration = Duration::from_secs(2);

const SYSTEMD_RUNTIME: &str = "/run/systemd";

const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct NamespaceSpawner {
    cgroup: CgroupController,
    state: Arc<Mutex<HashMap<AgentId, AgentState>>>,
    pool: crate::network::AgentSubnetPool,
    /// Set once the host-wide NAT and isolation rules are in place. Until
    /// then agents get no network interface at all, even if their policy
    /// allows egress, rather than an unisolated one.
    network_ready: bool,
    /// systemd's runtime directory, `/run/systemd`; tests point it
    /// elsewhere so they never ask the host's systemd.
    systemd_runtime: PathBuf,
}

#[derive(Debug)]
struct AgentState {
    child: ChildWatch,
    cgroup_path: PathBuf,
}

impl NamespaceSpawner {
    /// Probe the host for cgroup v2 and a writable agent cgroup parent. Returns an
    /// error if either is missing — the gateway should fall back to
    /// [`crate::subprocess::SubprocessSpawner`] with a warn log.
    pub fn try_init() -> Result<Self> {
        Self::try_init_with_pool(crate::network::AgentSubnetPool::default())
    }

    /// Variant of [`Self::try_init`] that takes an explicit per-agent network
    /// pool. Used by the gateway when `[network] agent_pool_cidr` is
    /// configured to something other than the default.
    pub fn try_init_with_pool(pool: crate::network::AgentSubnetPool) -> Result<Self> {
        let cgroup = CgroupController::system()
            .map_err(|e| SpawnerError::IsolationUnavailable(e.to_string()))?;
        info!(
            parent = %cgroup.parent().display(),
            pool_cidr = %pool.cidr(),
            "NamespaceSpawner cgroup v2 ready"
        );
        Ok(Self {
            cgroup,
            state: Arc::new(Mutex::new(HashMap::new())),
            pool,
            network_ready: false,
            systemd_runtime: PathBuf::from(SYSTEMD_RUNTIME),
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
            network_ready: false,
            systemd_runtime: PathBuf::from(SYSTEMD_RUNTIME),
        }
    }

    #[cfg(test)]
    fn with_systemd_runtime(mut self, dir: PathBuf) -> Self {
        self.systemd_runtime = dir;
        self
    }

    /// systemd's view of a `havn.slice` unit. Without systemd as init
    /// (no `<runtime>/system`) nothing can own `/havn.slice`, unless the
    /// cgroup root has `init.scope`: a container sharing the host's cgroup
    /// namespace, whose hierarchy the host's systemd owns.
    async fn slice_unit(&self) -> SliceUnit {
        if !self.systemd_runtime.join("system").exists() {
            if self.cgroup.root().join("init.scope").exists() {
                warn!("the cgroup hierarchy belongs to a systemd this gateway cannot ask");
                return SliceUnit::Unknown;
            }
            return SliceUnit::Inactive;
        }
        let systemctl = match crate::network::resolve_tool("systemctl") {
            Ok(path) => path,
            Err(e) => {
                warn!(error = %e, "cannot ask systemd about havn.slice");
                return SliceUnit::Unknown;
            }
        };
        let query = Command::new(systemctl)
            .args(["show", "--property=ActiveState", "--value", RETIRED_PARENT])
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(SYSTEMCTL_TIMEOUT, query).await {
            Ok(Ok(out)) if out.status.success() => {
                SliceUnit::from_active_state(&String::from_utf8_lossy(&out.stdout))
            }
            Ok(Ok(out)) => {
                warn!(status = %out.status, "systemctl show havn.slice failed");
                SliceUnit::Unknown
            }
            Ok(Err(e)) => {
                warn!(error = %e, "cannot run systemctl");
                SliceUnit::Unknown
            }
            Err(_) => {
                warn!("systemctl show havn.slice timed out");
                SliceUnit::Unknown
            }
        }
    }

    /// Record that [`crate::network::ensure_global_nat`] succeeded, so
    /// agents whose policy allows egress get their veth.
    pub fn set_network_ready(&mut self) {
        self.network_ready = true;
    }

    /// SIGKILL whatever is left in `cgroup`, wait for it to empty, and
    /// remove it.
    async fn clear_cgroup(&self, cgroup: &Path) -> Result<(), CgroupError> {
        if let Err(e) = self.cgroup.kill_all(cgroup) {
            warn!(cgroup = %cgroup.display(), error = %e, "cgroup.kill failed");
        }
        let deadline = tokio::time::Instant::now() + CGROUP_DRAIN;
        while self.cgroup.is_populated(cgroup) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.cgroup.cleanup(cgroup)
    }

    /// Stop one instance: SIGTERM through the reaper, `cgroup.kill` for
    /// whatever the reaper cannot see, then remove its cgroup and network.
    async fn terminate(&self, agent_id: AgentId, state: AgentState) {
        let AgentState { child, cgroup_path } = state;

        // 1. Graceful SIGTERM first. The child is the OUTER reaper (in the
        //    host PID ns); its signal handler forwards SIGTERM to the inner
        //    runtime (PID 1 in the new ns) — see
        //    `ns_setup::install_signal_forwarder`. The inner runs its
        //    graceful shutdown, exits, and the outer waitpid's then
        //    `_exit`s with the same status. For a child that has already
        //    exited this is a no-op and the wait below returns at once.
        child.terminate();

        // 2. Wait STOP_GRACE for the OUTER to exit on its own. A timeout
        //    means either the inner is stuck or the signal forwarder did
        //    not deliver — either way we fall through to cgroup.kill.
        let timed_out = match tokio::time::timeout(STOP_GRACE, child.exited()).await {
            Ok(exit) => {
                info!(%agent_id, %exit, "outer reaper exited");
                false
            }
            Err(_) => {
                warn!(%agent_id, "SIGTERM grace expired; will hammer cgroup subtree");
                true
            }
        };

        // 3. Backstop: ALWAYS cgroup.kill the whole subtree, then remove the
        //    cgroup. Even when the outer reaper exited cleanly above, the
        //    inner runtime (and any grandchild of its shell tool) may live
        //    on in the same cgroup, invisible to the reaper wait — for
        //    instance when the reaper itself was killed. Without this
        //    hammer they would survive `stop()`, leak resources, and keep
        //    the cgroup busy.
        if let Err(e) = self.clear_cgroup(&cgroup_path).await {
            warn!(
                cgroup = %cgroup_path.display(),
                error = %e,
                "cgroup cleanup failed; the next spawn or gateway start retries it"
            );
        }
        if timed_out {
            // The cgroup kill took the reaper with it; make sure it is
            // reaped even if the kill could not be written.
            child.kill();
            let _ = child.exited().await;
        }

        // 4. Best-effort veth teardown. The kernel reaps the pair when
        //    the agent's netns is destroyed (after the last process in
        //    it exits, which the cgroup.kill above ensures); this `ip
        //    link del` is the belt-and-braces path for the rare case
        //    where the host-side end lingers.
        crate::network::teardown_agent_network(agent_id, &self.pool);
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

        // 2. Start from an empty cgroup. A previous instance still tracked
        //    here is stopped; a cgroup left from anything else is cleared.
        //    Processes left in it could otherwise pass the gateway's peer
        //    check as this agent.
        let previous = self.state.lock().await.remove(&config.agent_id);
        if let Some(previous) = previous {
            warn!(agent_id = %config.agent_id, "stopping a previous instance before respawn");
            self.terminate(config.agent_id, previous).await;
        }
        let stale = self.cgroup.agent_path(&config.agent_id.to_string());
        if stale.exists() {
            self.clear_cgroup(&stale).await.map_err(|e| {
                SpawnerError::Spawn(format!(
                    "agent cgroup {} from an earlier run cannot be cleared: {e}",
                    stale.display()
                ))
            })?;
        }

        // 3. Create the cgroup with policy limits BEFORE spawning so we
        //    never have an unconstrained PID in flight.
        let cgroup_path = self
            .cgroup
            .create_agent(&config.agent_id.to_string(), &policy.resource_limits)
            .map_err(|e| map_cgroup_err(&e))?;

        // 4. Build the spawn command. Two-stage launch:
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
        cmd.env_clear()
            .envs(crate::agent_environment(&config.env_passthrough, |name| {
                std::env::var_os(name)
            }));
        // Operator opt-out for kernels without Landlock; the runtime
        // refuses to start without Landlock otherwise. Forwarded on its
        // own because HAVN_* variables are never passed through.
        if std::env::var_os("HAVN_ALLOW_NO_LANDLOCK").as_deref() == Some(std::ffi::OsStr::new("1"))
        {
            cmd.env("HAVN_ALLOW_NO_LANDLOCK", "1");
        }
        // Landlock rules havn-init applies right before exec'ing the runtime,
        // so every runtime thread and every tool process is confined.
        let (landlock_rw, landlock_ro) = crate::filesystem_rules::allowlist(
            &config.workspace_dir,
            &config.extra_mounts,
            &config.tmpfs_mounts,
            policy,
            &config.runtime_binary,
        );
        cmd.env(
            crate::filesystem_rules::RW_ENV,
            crate::filesystem_rules::encode(&landlock_rw),
        )
        .env(
            crate::filesystem_rules::RO_ENV,
            crate::filesystem_rules::encode(&landlock_ro),
        );
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
        // `configure_agent_network` (step 4 below) on the same bit.
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
        //   - writes `0` to the agent cgroup's `cgroup.procs` via
        //     [`crate::ns_setup::join_cgroup`] (raw libc I/O, path prepared
        //     before fork),
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
        // havn-init dies with the gateway (and takes the runtime along).
        let gateway_pid = std::process::id().cast_signed();
        // The child joins its cgroup itself, before exec, so havn-init and
        // everything it forks (the runtime, tools, subagents) inherit the
        // limits. Joining from the parent after spawn() races with
        // havn-init's fork and leaves the runtime outside the cgroup.
        let cgroup_procs = CString::new(cgroup_path.join("cgroup.procs").as_os_str().as_bytes())
            .map_err(|e| SpawnerError::Spawn(format!("cgroup path contains NUL: {e}")))?;
        // SAFETY: pre_exec runs in the child after fork, before exec; it
        // restricts itself to the contracts spelled out in `ns_setup`.
        #[allow(
            clippy::similar_names,
            reason = "uid/gid is the canonical spelling for these Linux primitives"
        )]
        unsafe {
            cmd.as_std_mut().pre_exec(move || {
                ns_setup::join_cgroup(&cgroup_procs)?;
                ns_setup::unshare_namespaces()?;
                ns_setup::write_user_namespace_maps(host_uid, host_gid)?;
                ns_setup::mark_mounts_private()?;
                // Last, after every credential change, which could clear it.
                ns_setup::kill_with_parent(Some(gateway_pid))?;
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
        if policy.network_policy.egress_allowed && !self.network_ready {
            warn!(
                agent_id = %config.agent_id,
                "agent policy allows egress but agent network isolation is not in place; \
                 starting the agent without network access"
            );
        }
        if policy.network_policy.egress_allowed && self.network_ready {
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

        let handle = AgentHandle::new(config.agent_id, pid);
        self.state.lock().await.insert(
            config.agent_id,
            AgentState {
                child: ChildWatch::start(child, &handle),
                cgroup_path,
            },
        );
        Ok(handle)
    }

    async fn stop(&self, handle: &AgentHandle) -> Result<()> {
        let state = {
            let mut guard = self.state.lock().await;
            if guard
                .get(&handle.agent_id)
                .is_none_or(|s| !s.child.belongs_to(handle))
            {
                return Err(SpawnerError::NotFound(handle.agent_id));
            }
            guard
                .remove(&handle.agent_id)
                .ok_or(SpawnerError::NotFound(handle.agent_id))?
        };
        self.terminate(handle.agent_id, state).await;
        Ok(())
    }

    async fn wait(&self, handle: &AgentHandle) -> Result<AgentExit> {
        let child = self
            .state
            .lock()
            .await
            .get(&handle.agent_id)
            .filter(|s| s.child.belongs_to(handle))
            .map(|s| s.child.clone())
            .ok_or(SpawnerError::NotFound(handle.agent_id))?;
        Ok(child.exited().await)
    }

    async fn reap_orphans(&self) -> Result<usize> {
        let tracked: Vec<PathBuf> = self
            .state
            .lock()
            .await
            .values()
            .map(|s| s.cgroup_path.clone())
            .collect();
        let mut reaped = 0;
        // Agents of a gateway before #66 belong to a gateway that is gone;
        // the ones still meant to run are restarted under the current parent.
        let retired = self.cgroup.retired_agent_cgroups();
        let current = self
            .cgroup
            .agent_cgroups()
            .into_iter()
            .filter(|c| !tracked.contains(c));
        for cgroup in retired.into_iter().chain(current) {
            let populated = self.cgroup.is_populated(&cgroup);
            match self.clear_cgroup(&cgroup).await {
                Ok(()) => {
                    info!(cgroup = %cgroup.display(), populated, "removed agent cgroup left by an earlier run");
                    reaped += 1;
                }
                Err(e) => warn!(
                    cgroup = %cgroup.display(),
                    error = %e,
                    "agent cgroup left by an earlier run cannot be removed"
                ),
            }
        }
        if self.cgroup.retired_parent().is_some() {
            let unit = self.slice_unit().await;
            self.cgroup.remove_retired_parent(unit);
        }
        Ok(reaped)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::cgroup::{ALL_CONTROLLERS, fake_root, retired_parent_with};
    use std::path::PathBuf;

    fn fake_cgroup_root() -> PathBuf {
        fake_root(ALL_CONTROLLERS, ALL_CONTROLLERS)
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
            env_passthrough: Vec::new(),
        };
        let policy = Policy::default();

        let err = spawner.spawn(&cfg, &policy).await.expect_err("should fail");
        assert!(matches!(err, SpawnerError::Spawn(_)), "{err:?}");

        // No `agent-*` directories should be left behind.
        let parent = root.join(havn_core::AGENT_CGROUP_PARENT);
        let mut entries = std::fs::read_dir(&parent).expect("readdir");
        let leftover_count = entries
            .by_ref()
            .filter(|e| e.as_ref().map(|x| x.path().is_dir()).unwrap_or(false))
            .count();
        assert_eq!(
            leftover_count, 0,
            "expected no leftover cgroup dirs after failed spawn"
        );
    }

    #[tokio::test]
    async fn spawn_refuses_a_cgroup_left_by_an_earlier_run_it_cannot_clear() {
        let root = fake_cgroup_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let spawner = NamespaceSpawner::with_controller(ctl.clone());
        let agent_id = AgentId::new();
        let stale = ctl
            .create_agent(&agent_id.to_string(), &Policy::default().resource_limits)
            .expect("stale cgroup");
        // A nested cgroup keeps the directory from being removed.
        std::fs::create_dir(stale.join("nested")).expect("nested");

        let cfg = AgentSpawnConfig {
            agent_id,
            workspace_dir: std::env::temp_dir().join(format!("havn-ws-{}", uuid::Uuid::now_v7())),
            runtime_binary: PathBuf::from("/bin/this-binary-does-not-exist"),
            init_binary: Some(PathBuf::from("/bin/this-binary-does-not-exist")),
            gateway_socket: std::env::temp_dir().join("havn-fake.sock"),
            is_subagent: false,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
            agent_dns: Vec::new(),
            env_passthrough: Vec::new(),
        };
        let err = spawner
            .spawn(&cfg, &Policy::default())
            .await
            .expect_err("must not join a cgroup that may hold old processes");
        assert!(
            matches!(&err, SpawnerError::Spawn(m) if m.contains("cannot be cleared")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn reap_orphans_clears_every_untracked_agent_cgroup() {
        let root = fake_cgroup_root();
        let ctl = CgroupController::with_root(root).expect("ctl");
        let limits = Policy::default().resource_limits;
        let a = ctl.create_agent("a", &limits).expect("a");
        let b = ctl.create_agent("b", &limits).expect("b");
        let spawner = NamespaceSpawner::with_controller(ctl);

        assert_eq!(spawner.reap_orphans().await.expect("reap"), 2);
        assert!(!a.exists() && !b.exists());
        assert_eq!(spawner.reap_orphans().await.expect("reap"), 0);
    }

    fn without_systemd() -> PathBuf {
        std::env::temp_dir().join(format!("havn-no-systemd-{}", uuid::Uuid::now_v7()))
    }

    #[tokio::test]
    async fn reap_orphans_clears_agents_an_older_gateway_left_in_havn_slice() {
        let root = fake_cgroup_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        let current = ctl
            .create_agent("now", &Policy::default().resource_limits)
            .expect("current");
        let ids = [AgentId::new().to_string(), AgentId::new().to_string()];
        let retired = retired_parent_with(&root, &[&ids[0], &ids[1]]);
        let spawner =
            NamespaceSpawner::with_controller(ctl.clone()).with_systemd_runtime(without_systemd());

        assert_eq!(spawner.reap_orphans().await.expect("reap"), 3);
        for agent in &retired {
            assert!(!agent.exists(), "{}", agent.display());
        }
        assert!(!current.exists());
        assert!(!root.join(RETIRED_PARENT).exists());
        assert!(ctl.parent().is_dir(), "the current parent stays");
        assert_eq!(spawner.reap_orphans().await.expect("reap"), 0);
    }

    #[tokio::test]
    async fn reap_orphans_keeps_havn_slice_holding_other_cgroups() {
        let root = fake_cgroup_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        let retired = retired_parent_with(&root, &[&AgentId::new().to_string()]);
        let other = root.join(RETIRED_PARENT).join("unrelated.service");
        std::fs::create_dir(&other).expect("other");
        let spawner =
            NamespaceSpawner::with_controller(ctl).with_systemd_runtime(without_systemd());

        assert_eq!(spawner.reap_orphans().await.expect("reap"), 1);
        assert!(!retired[0].exists());
        assert!(other.is_dir());
    }

    #[tokio::test]
    async fn reap_orphans_leaves_systemd_units_in_havn_slice_alone() {
        let root = fake_cgroup_root();
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        let units = retired_parent_with(&root, &["x.service", "backup"]);
        for unit in &units {
            std::fs::write(unit.join("cgroup.events"), "populated 1\n").expect("events");
        }
        let spawner =
            NamespaceSpawner::with_controller(ctl).with_systemd_runtime(without_systemd());

        assert_eq!(spawner.reap_orphans().await.expect("reap"), 0);
        for unit in &units {
            assert!(unit.is_dir(), "{}", unit.display());
            assert!(!unit.join("cgroup.kill").exists(), "{}", unit.display());
        }
        assert!(root.join(RETIRED_PARENT).is_dir());
    }

    #[tokio::test]
    async fn reap_orphans_keeps_havn_slice_when_the_host_systemd_owns_the_hierarchy() {
        let root = fake_cgroup_root();
        std::fs::create_dir(root.join("init.scope")).expect("init.scope");
        let ctl = CgroupController::with_root(root.clone()).expect("ctl");
        let retired = retired_parent_with(&root, &[&AgentId::new().to_string()]);
        let spawner =
            NamespaceSpawner::with_controller(ctl).with_systemd_runtime(without_systemd());

        assert_eq!(spawner.reap_orphans().await.expect("reap"), 1);
        assert!(!retired[0].exists());
        assert!(root.join(RETIRED_PARENT).is_dir());
    }
}
