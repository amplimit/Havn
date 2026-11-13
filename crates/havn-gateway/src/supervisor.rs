//! Agent supervision: notices when an agent's process ends or goes silent,
//! restarts agents that are meant to be running, and reconciles the
//! database with reality when the gateway starts and stops.
//!
//! An agent is meant to be running while its DB status is `running`: it is
//! set when the agent starts and replaced by `stopped` on `POST /stop`.
//! Status `error` is written only here, when the supervisor gives up on an
//! agent that was meant to run, so it keeps that intent: the next gateway
//! start tries such agents again.
//!
//! - **Exit**: every spawned instance gets a task waiting on its process
//!   ([`watch`]). An exit nobody asked for (crash, `kill -9`, OOM kill)
//!   deregisters the instance at once, so `POST /start` spawns a fresh one
//!   instead of waiting for a handshake that will never come.
//! - **Silence**: the runtime sends `LivenessPing` every
//!   [`havn_proto::LIVENESS_PING_INTERVAL`] and the agent socket reader
//!   counts every frame it receives. An instance quiet for
//!   [`LIVENESS_MISSED_PINGS`] intervals (frozen, wedged) is stopped, as is
//!   one that takes minutes to connect or to send its first ping
//!   ([`check_liveness`]). It must be found quiet on two checks in a row,
//!   and a check that runs late (the gateway itself stalled) is skipped, so
//!   pings the gateway has not read yet do not get agents killed.
//! - **Restart**: either way the instance is cleaned up through the
//!   spawner and, if the agent is still meant to run, started again with
//!   exponential backoff ([`RestartPolicy`]). After too many failures in a
//!   row it is left in status `error` with a fresh budget for the next
//!   time it is started.
//! - **Gateway restart**: agent processes do not outlive the gateway (the
//!   spawner ties them to it with `PR_SET_PDEATHSIG`, and a clean shutdown
//!   stops them, [`shut_down`]). At startup [`reconcile`] kills whatever an
//!   earlier gateway left behind and starts every agent whose status is
//!   still `running` (or `error`), so status matches the processes again
//!   and cron jobs and heartbeats of those agents keep firing.
//!
//! Audit rows written here carry `"by": "supervisor"` in their details.
//! The audit log needs a real user as actor and there is no system user,
//! so the agent's owner stands in, which also files the row under the
//! owner's and the team's audit views.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use havn_core::{AgentId, UserId};
use havn_db::repo::agents::{self as repo, AgentStatus};
use havn_proto::GatewayToAgent;
use havn_spawner::{AgentExit, AgentHandle};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::AppState;
use crate::agent_registry::{Instance, LivenessLimits};
use crate::api::agents::{StartBy, start_agent, start_lock};
use crate::audit;

/// Missed liveness pings after which a connected agent counts as hung.
pub const LIVENESS_MISSED_PINGS: u32 = 4;

/// Agents [`reconcile`] starts at once, so a host with many agents is
/// not hit by all their start-ups together.
const RESTORE_CONCURRENCY: usize = 4;

/// From spawn to Hello. Covers opening agent.db, migrations and skill
/// indexing on a loaded host.
const CONNECT_LIMIT: Duration = Duration::from_secs(5 * 60);

/// From Hello to the first ping: the embedder (possibly downloading a
/// model) and MCP servers start in between.
const FIRST_PING_LIMIT: Duration = Duration::from_secs(10 * 60);

/// How long the socket reader may spend on one frame before the agent
/// counts as hung all the same. Deliveries do not block (`try_send`), so
/// only a wedged gateway-side handler gets near this.
const BUSY_LIMIT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    pub first_delay: Duration,
    pub max_delay: Duration,
    /// A run at least this long counts as healthy and clears the failures
    /// before it.
    pub stable_after: Duration,
    /// Failures in a row after which the supervisor gives up.
    pub max_failures: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            stable_after: Duration::from_secs(120),
            max_failures: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    After(Duration),
    GiveUp,
}

#[derive(Debug, Clone, Copy, Default)]
struct History {
    failures: u32,
}

impl RestartPolicy {
    /// Record a failed run of `ran_for` and decide what to do next.
    fn next(&self, history: &mut History, ran_for: Duration) -> Restart {
        if ran_for >= self.stable_after {
            history.failures = 0;
        }
        history.failures = history.failures.saturating_add(1);
        if history.failures > self.max_failures {
            return Restart::GiveUp;
        }
        let factor = 2_u32.saturating_pow(history.failures - 1);
        Restart::After(self.first_delay.saturating_mul(factor).min(self.max_delay))
    }
}

#[derive(Debug, Clone)]
pub struct Supervisor {
    policy: RestartPolicy,
    liveness_interval: Duration,
    limits: LivenessLimits,
    history: Arc<Mutex<HashMap<AgentId, History>>>,
    shutting_down: Arc<AtomicBool>,
}

impl Supervisor {
    pub fn new(policy: RestartPolicy, liveness_interval: Duration) -> Self {
        Self {
            policy,
            liveness_interval,
            limits: LivenessLimits {
                silence: liveness_interval.saturating_mul(LIVENESS_MISSED_PINGS),
                connect: CONNECT_LIMIT,
                first_ping: FIRST_PING_LIMIT,
                busy: BUSY_LIMIT,
            },
            history: Arc::new(Mutex::new(HashMap::new())),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Clear an agent's failure history (explicit start, stop, delete, or
    /// giving up).
    pub async fn forget(&self, id: AgentId) {
        self.history.lock().await.remove(&id);
    }

    async fn next_restart(&self, id: AgentId, ran_for: Duration) -> (Restart, u32) {
        let mut history = self.history.lock().await;
        let entry = history.entry(id).or_default();
        let decision = self.policy.next(entry, ran_for);
        (decision, entry.failures)
    }
}

#[derive(Debug, Clone, Copy)]
enum Ended {
    Exited(AgentExit),
    Silent(Duration),
}

/// Watch a freshly registered instance and recover the agent when its
/// process exits without being asked to.
pub(crate) fn watch(state: &AppState, handle: AgentHandle, generation: u64) {
    let state = state.clone();
    tokio::spawn(async move {
        let exit = match state.spawner.wait(&handle).await {
            Ok(exit) => exit,
            Err(e) => {
                debug!(agent_id = %handle.agent_id, error = %e, "no process to watch");
                return;
            }
        };
        if state.supervisor.shutting_down() {
            // Left during a drain. It stays registered so `shut_down`
            // releases what the spawner holds before the gateway exits.
            return;
        }
        // Gone from the registry: stopped on purpose, or already handled.
        let Some(instance) = state
            .registry
            .remove_generation(handle.agent_id, generation)
            .await
        else {
            return;
        };
        recover(&state, instance, Ended::Exited(exit)).await;
    });
}

/// Instances found quiet by the previous check.
pub(crate) type Suspects = HashSet<(AgentId, u64)>;

/// Stop every instance found quiet on this check and the one before, and
/// recover its agent. `now` is a parameter so tests can look ahead.
pub(crate) async fn check_liveness(state: &AppState, now: Instant, suspects: &mut Suspects) {
    let silent = state.registry.silent(now, &state.supervisor.limits).await;
    let previous = std::mem::take(suspects);
    for silent in silent {
        let key = (silent.agent_id, silent.generation);
        if !previous.contains(&key) {
            suspects.insert(key);
            continue;
        }
        let Some(instance) = state
            .registry
            .remove_generation(silent.agent_id, silent.generation)
            .await
        else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            recover(&state, instance, Ended::Silent(silent.silent_for)).await;
        });
    }
}

pub(crate) async fn run_liveness_checks(state: AppState) {
    let period = state.supervisor.liveness_interval;
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut suspects = Suspects::new();
    let mut last: Option<Instant> = None;
    loop {
        interval.tick().await;
        let now = Instant::now();
        let late = last.is_some_and(|last| now.saturating_duration_since(last) > period * 2);
        last = Some(now);
        if late {
            // The gateway itself was stalled (load, swap, SIGSTOP): pings
            // may sit unread in the sockets. Judge again next round.
            warn!("liveness check ran late; skipping this round");
            suspects.clear();
            continue;
        }
        check_liveness(&state, now, &mut suspects).await;
    }
}

/// An instance has ended (it is already out of the registry): log and
/// audit it, release what the spawner holds for it, and schedule a restart
/// if the agent is still meant to run.
async fn recover(state: &AppState, instance: Instance, ended: Ended) {
    let id = instance.handle.agent_id;
    let ran_for = instance.registered_at.elapsed();
    let ran_for_s = ran_for.as_secs();

    if state.supervisor.shutting_down() {
        // Ended as the gateway stops (the flag was set after `watch` took
        // it out of the registry): release it, but no audit or restart.
        debug!(agent_id = %id, "agent left while the gateway is stopping");
        if let Err(e) = state.spawner.stop(&instance.handle).await {
            debug!(agent_id = %id, error = %e, "spawner had nothing left to stop");
        }
        return;
    }
    match (ended, meant_to_run(state, id).await) {
        // Stopped or deleted meanwhile: its Shutdown frame can make it
        // exit before the stop deregisters it.
        (Ended::Exited(exit), None) => {
            debug!(agent_id = %id, %exit, "agent process exited after being stopped");
        }
        (Ended::Exited(exit), Some(agent)) => {
            warn!(agent_id = %id, %exit, ran_for_s, "agent process exited");
            let details =
                serde_json::json!({"by": "supervisor", "exit": exit, "ran_for_s": ran_for_s});
            audit::record_agent_action(&state.db, agent.owner_id, id, "agent.exited", details)
                .await;
        }
        (Ended::Silent(silent_for), agent) => {
            let silent_for_s = silent_for.as_secs();
            warn!(
                agent_id = %id,
                silent_for_s,
                "agent sent nothing in time; stopping it"
            );
            if let Some(agent) = agent {
                let details = serde_json::json!({
                    "by": "supervisor",
                    "silent_for_s": silent_for_s,
                    "ran_for_s": ran_for_s,
                });
                audit::record_agent_action(
                    &state.db,
                    agent.owner_id,
                    id,
                    "agent.unresponsive",
                    details,
                )
                .await;
            }
        }
    }

    // Under the start lock, so a concurrent start finds a clean slate.
    {
        let agent_lock = start_lock(state, id).await;
        let _guard = agent_lock.lock().await;
        if let Err(e) = state.spawner.stop(&instance.handle).await {
            debug!(agent_id = %id, error = %e, "spawner had nothing left to stop");
        }
    }

    if state.supervisor.shutting_down() {
        return;
    }
    if let Some(agent) = meant_to_run(state, id).await {
        restart(state.clone(), agent.owner_id, id, ran_for);
    }
}

/// The agent's row while its status is `running`.
async fn meant_to_run(state: &AppState, id: AgentId) -> Option<repo::Agent> {
    repo::find_by_id(&state.db, id)
        .await
        .ok()
        .flatten()
        .filter(|agent| agent.status == AgentStatus::Running)
}

/// Start the agent again after the policy's backoff, retrying as long as
/// no process could be spawned at all. Once one is spawned its own exit
/// watch takes over.
fn restart(state: AppState, owner: UserId, id: AgentId, ran_for: Duration) {
    tokio::spawn(async move {
        let mut ran_for = ran_for;
        loop {
            match state.supervisor.next_restart(id, ran_for).await {
                (Restart::GiveUp, failures) => {
                    give_up(&state, owner, id, failures).await;
                    return;
                }
                (Restart::After(delay), failures) => {
                    info!(
                        agent_id = %id,
                        failures,
                        delay_ms = delay.as_millis(),
                        "restarting agent"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
            if state.supervisor.shutting_down() {
                return;
            }
            match start_agent(&state, id, StartBy::Supervisor(owner)).await {
                Ok(()) => return,
                Err(e) if e.spawned => return,
                Err(e) => {
                    warn!(agent_id = %id, error = %e.error, "agent restart failed");
                    ran_for = Duration::ZERO;
                }
            }
        }
    });
}

async fn give_up(state: &AppState, owner: UserId, id: AgentId, failures: u32) {
    // Whoever starts it next (an operator, or a chat that auto-starts it)
    // gets a full budget again.
    state.supervisor.forget(id).await;
    let agent_lock = start_lock(state, id).await;
    let _guard = agent_lock.lock().await;
    let still_wanted = matches!(
        repo::find_by_id(&state.db, id).await,
        Ok(Some(agent)) if agent.status == AgentStatus::Running
    );
    if !still_wanted || state.registry.is_registered(id).await {
        return;
    }
    error!(
        agent_id = %id,
        failures,
        "agent keeps failing; giving up until it is started again"
    );
    if let Err(e) = repo::set_status(&state.db, id, AgentStatus::Error).await {
        warn!(agent_id = %id, error = %e, "could not mark agent as failed");
    }
    audit::record_agent_action(
        &state.db,
        owner,
        id,
        "agent.restart_abandoned",
        serde_json::json!({"by": "supervisor", "failures": failures}),
    )
    .await;
}

/// Gateway startup: kill agent processes an earlier gateway left behind,
/// then start every agent that is still meant to run: status `running`,
/// and `error`, which only a give-up writes. A failure at the last boot
/// (spawner unavailable, say) thus does not keep agents down for good.
/// Starts proceed in the background; the agent socket listener must
/// already be accepting.
pub(crate) async fn reconcile(state: &AppState) {
    match state.spawner.reap_orphans().await {
        Ok(0) => {}
        Ok(n) => info!(
            agents = n,
            "removed agent cgroups left by an earlier gateway run"
        ),
        Err(e) => warn!(error = %e, "could not look for agent processes from an earlier run"),
    }
    match repo::list_with_status(&state.db, AgentStatus::Error).await {
        Ok(failed) => {
            for agent in failed {
                info!(agent_id = %agent.id, name = %agent.name, "retrying agent given up on before");
                if let Err(e) = repo::set_status(&state.db, agent.id, AgentStatus::Running).await {
                    warn!(agent_id = %agent.id, error = %e, "could not retry agent");
                }
            }
        }
        Err(e) => warn!(error = %e, "cannot list failed agents; they stay down"),
    }
    let agents = match repo::list_with_status(&state.db, AgentStatus::Running).await {
        Ok(agents) => agents,
        Err(e) => {
            error!(error = %e, "cannot list agents to restore; none restarted");
            return;
        }
    };
    let slots = Arc::new(tokio::sync::Semaphore::new(RESTORE_CONCURRENCY));
    for agent in agents {
        info!(agent_id = %agent.id, name = %agent.name, "restoring agent that was running");
        let state = state.clone();
        let slots = Arc::clone(&slots);
        tokio::spawn(async move {
            let Ok(_slot) = slots.acquire().await else {
                return;
            };
            match start_agent(&state, agent.id, StartBy::Supervisor(agent.owner_id)).await {
                Ok(()) => {}
                Err(e) if e.spawned => {}
                Err(e) => {
                    warn!(agent_id = %agent.id, error = %e.error, "could not restore agent");
                    restart(state, agent.owner_id, agent.id, Duration::ZERO);
                }
            }
        });
    }
}

/// How long a stopping gateway waits for agents to finish their work.
pub const DRAIN_LIMIT: Duration = Duration::from_secs(30);

/// Shown in the open chats of agents that were still busy at the end of a
/// drain.
const CUT_OFF_NOTICE: &str = "The gateway restarted before the agent finished. Send your last message again once the chat is back.";

/// First half of a clean gateway shutdown: take no new work, ask every
/// connected agent to finish what it is doing and exit, and wait up to
/// `limit` for them. The agent socket and LLM proxy must keep running
/// meanwhile. Agents still busy at the end are left to [`shut_down`];
/// their open chats are told the reply was cut off.
pub(crate) async fn drain(state: &AppState, limit: Duration) {
    state.supervisor.shutting_down.store(true, Ordering::SeqCst);
    state.registry.start_draining();
    let connected: Vec<AgentId> = state
        .registry
        .list()
        .await
        .into_iter()
        .filter(|agent| agent.connected)
        .map(|agent| agent.agent_id)
        .collect();
    if connected.is_empty() {
        return;
    }
    info!(
        agents = connected.len(),
        limit_s = limit.as_secs(),
        "letting agents finish their work"
    );
    let deadline = tokio::time::Instant::now() + limit;
    for id in &connected {
        // A full queue means a stalled agent: `shut_down` stops it.
        if let Err(e) = state.registry.try_send(*id, GatewayToAgent::Drain).await {
            debug!(agent_id = %id, error = %e, "could not ask agent to drain");
        }
    }
    let mut busy = connected;
    loop {
        let mut still = Vec::with_capacity(busy.len());
        for id in busy {
            if state.registry.is_connected(id).await {
                still.push(id);
            }
        }
        busy = still;
        if busy.is_empty() || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
    if busy.is_empty() {
        info!("agents finished their work");
        return;
    }
    warn!(
        agents = busy.len(),
        "agents still busy after the drain limit; stopping them"
    );
    for id in busy {
        state.webchat_router.notice(id, CUT_OFF_NOTICE).await;
    }
}

/// How often [`drain`] checks whether the agents have left.
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// Clean gateway shutdown: stop every agent process. Their status stays
/// `running`, so the next gateway start brings them back. A start that is
/// in progress sees the flag after registering and stops its own instance.
pub(crate) async fn shut_down(state: &AppState) {
    state.supervisor.shutting_down.store(true, Ordering::SeqCst);
    let handles = state.registry.drain().await;
    if handles.is_empty() {
        return;
    }
    info!(agents = handles.len(), "stopping agents");
    futures_util::future::join_all(handles.iter().map(|handle| async move {
        if let Err(e) = state.spawner.stop(handle).await {
            warn!(agent_id = %handle.agent_id, error = %e, "stopping agent failed");
        }
    }))
    .await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::agent_registry::AgentRegistry;
    use async_trait::async_trait;
    use havn_core::Policy;
    use havn_db::repo::agents::NewAgent;
    use havn_proto::GatewayToAgent;
    use havn_spawner::{AgentSpawnConfig, AgentSpawner, ResourceStats, SpawnerError};
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use tokio::sync::{mpsc, watch as watch_channel};

    // Time is not paused in these tests: the in-memory database answers
    // from sqlx's worker thread, and with a paused clock tokio would jump
    // ahead (firing sqlx's acquire timeout) while it waits for it. The
    // restart delays are milliseconds instead and liveness checks take
    // `now` as a parameter.

    #[test]
    fn backoff_doubles_up_to_the_cap_then_gives_up() {
        let policy = RestartPolicy {
            first_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            stable_after: Duration::from_secs(60),
            max_failures: 4,
        };
        let mut history = History::default();
        let quick = Duration::from_secs(1);
        let delays: Vec<Restart> = (0..5).map(|_| policy.next(&mut history, quick)).collect();
        assert_eq!(
            delays,
            [
                Restart::After(Duration::from_secs(1)),
                Restart::After(Duration::from_secs(2)),
                Restart::After(Duration::from_secs(4)),
                Restart::After(Duration::from_secs(5)),
                Restart::GiveUp,
            ]
        );
        assert_eq!(policy.next(&mut history, quick), Restart::GiveUp);
    }

    #[test]
    fn a_stable_run_resets_the_failures() {
        let policy = RestartPolicy::default();
        let mut history = History::default();
        for _ in 0..policy.max_failures {
            assert!(matches!(
                policy.next(&mut history, Duration::ZERO),
                Restart::After(_)
            ));
        }
        assert_eq!(
            policy.next(&mut history, policy.stable_after),
            Restart::After(policy.first_delay),
            "a long run before this crash starts the count over"
        );
    }

    #[test]
    fn large_failure_counts_do_not_overflow() {
        let policy = RestartPolicy {
            max_failures: u32::MAX,
            ..RestartPolicy::default()
        };
        let mut history = History { failures: 200 };
        assert_eq!(
            policy.next(&mut history, Duration::ZERO),
            Restart::After(policy.max_delay)
        );
    }

    /// Spawns pretend processes. A process "connects" to the registry the
    /// way a runtime would, and lives until stopped or [`Self::crash`]ed.
    #[derive(Debug)]
    struct FakeSpawner {
        registry: AgentRegistry,
        connect: bool,
        exit_at_once: AtomicBool,
        fail_spawn: AtomicBool,
        spawn_delay_ms: AtomicU64,
        stop_delay_ms: AtomicU64,
        next_pid: AtomicU32,
        spawns: AtomicU32,
        orphans_reaped: AtomicU32,
        procs: std::sync::Mutex<HashMap<AgentId, Proc>>,
    }

    #[derive(Debug)]
    struct Proc {
        handle: AgentHandle,
        exit: watch_channel::Sender<Option<AgentExit>>,
        socket: Option<(mpsc::Sender<GatewayToAgent>, mpsc::Receiver<GatewayToAgent>)>,
    }

    impl FakeSpawner {
        fn new(registry: AgentRegistry, connect: bool, exit_at_once: bool) -> Self {
            Self {
                registry,
                connect,
                exit_at_once: AtomicBool::new(exit_at_once),
                fail_spawn: AtomicBool::new(false),
                spawn_delay_ms: AtomicU64::new(0),
                stop_delay_ms: AtomicU64::new(0),
                next_pid: AtomicU32::new(100),
                spawns: AtomicU32::new(0),
                orphans_reaped: AtomicU32::new(0),
                procs: std::sync::Mutex::new(HashMap::new()),
            }
        }

        fn spawns(&self) -> u32 {
            self.spawns.load(Ordering::SeqCst)
        }

        /// The process dies on its own (`kill -9`): the socket closes and
        /// the exit is reported.
        fn crash(&self, id: AgentId) {
            let mut procs = self.procs.lock().unwrap();
            let proc = procs.get_mut(&id).expect("running");
            proc.socket = None;
            proc.exit.send_replace(Some(AgentExit::Signal(9)));
        }

        /// The process exits by itself after its work (a drain): the
        /// socket closes and exit 0 is reported.
        fn finish(&self, id: AgentId) {
            let mut procs = self.procs.lock().unwrap();
            let proc = procs.get_mut(&id).expect("running");
            proc.socket = None;
            proc.exit.send_replace(Some(AgentExit::Code(0)));
        }

        /// Frames the gateway has sent the agent so far, oldest first.
        fn received(&self, id: AgentId) -> Vec<GatewayToAgent> {
            let mut procs = self.procs.lock().unwrap();
            let Some((_, rx)) = procs.get_mut(&id).and_then(|p| p.socket.as_mut()) else {
                return Vec::new();
            };
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        }

        /// The gateway side of the agent's current connection.
        fn connection(&self, id: AgentId) -> mpsc::Sender<GatewayToAgent> {
            let procs = self.procs.lock().unwrap();
            procs[&id].socket.as_ref().expect("connected").0.clone()
        }
    }

    fn delay(ms: &AtomicU64) -> Duration {
        Duration::from_millis(ms.load(Ordering::SeqCst))
    }

    #[async_trait]
    impl AgentSpawner for FakeSpawner {
        async fn spawn(
            &self,
            config: &AgentSpawnConfig,
            _policy: &Policy,
        ) -> havn_spawner::Result<AgentHandle> {
            tokio::time::sleep(delay(&self.spawn_delay_ms)).await;
            if self.fail_spawn.load(Ordering::SeqCst) {
                return Err(SpawnerError::Spawn("no runtime binary".into()));
            }
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let id = config.agent_id;
            let handle = AgentHandle::new(id, self.next_pid.fetch_add(1, Ordering::SeqCst));
            let (exit, _) = watch_channel::channel(None);
            let exit_at_once = self.exit_at_once.load(Ordering::SeqCst);
            if exit_at_once {
                exit.send_replace(Some(AgentExit::Code(1)));
            }
            let socket = if self.connect && !exit_at_once {
                let (tx, rx) = mpsc::channel(8);
                let registry = self.registry.clone();
                let hello = tx.clone();
                tokio::spawn(async move {
                    // Hello arrives once the gateway has registered us.
                    for _ in 0..200 {
                        if registry.attach(id, hello.clone()).await.is_ok() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                });
                Some((tx, rx))
            } else {
                None
            };
            self.procs.lock().unwrap().insert(
                id,
                Proc {
                    handle: handle.clone(),
                    exit,
                    socket,
                },
            );
            Ok(handle)
        }

        async fn stop(&self, handle: &AgentHandle) -> havn_spawner::Result<()> {
            tokio::time::sleep(delay(&self.stop_delay_ms)).await;
            let mut procs = self.procs.lock().unwrap();
            match procs.get(&handle.agent_id) {
                Some(p) if p.handle == *handle => {
                    let proc = procs.remove(&handle.agent_id).expect("present");
                    if proc.exit.borrow().is_none() {
                        proc.exit.send_replace(Some(AgentExit::Signal(15)));
                    }
                    Ok(())
                }
                _ => Err(SpawnerError::NotFound(handle.agent_id)),
            }
        }

        async fn wait(&self, handle: &AgentHandle) -> havn_spawner::Result<AgentExit> {
            let mut exit = self
                .procs
                .lock()
                .unwrap()
                .get(&handle.agent_id)
                .filter(|p| p.handle == *handle)
                .map(|p| p.exit.subscribe())
                .ok_or(SpawnerError::NotFound(handle.agent_id))?;
            let status = exit
                .wait_for(Option::is_some)
                .await
                .map(|s| s.unwrap_or(AgentExit::Unknown))
                .unwrap_or(AgentExit::Unknown);
            Ok(status)
        }

        async fn reap_orphans(&self) -> havn_spawner::Result<usize> {
            self.orphans_reaped.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }

        async fn stats(&self, _handle: &AgentHandle) -> havn_spawner::Result<ResourceStats> {
            Ok(ResourceStats::default())
        }
    }

    fn quick_policy() -> RestartPolicy {
        RestartPolicy {
            first_delay: Duration::from_millis(20),
            max_delay: Duration::from_millis(80),
            stable_after: Duration::from_secs(60),
            max_failures: 2,
        }
    }

    async fn fixture(
        policy: RestartPolicy,
        connect: bool,
        exit_at_once: bool,
    ) -> (AppState, Arc<FakeSpawner>, UserId) {
        let (mut state, user) = crate::auth::tests::state_with(false).await;
        let workspace = std::env::temp_dir().join(format!("havn-sup-{}", uuid::Uuid::now_v7()));
        state.workspace_root = workspace;
        let spawner = Arc::new(FakeSpawner::new(
            state.registry.clone(),
            connect,
            exit_at_once,
        ));
        state.spawner = spawner.clone();
        state.supervisor = Supervisor::new(policy, Duration::from_secs(15));
        (state, spawner, user)
    }

    async fn agent(state: &AppState, owner: UserId, name: &str) -> AgentId {
        repo::create(
            &state.db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name,
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent")
        .id
    }

    async fn running_agent(state: &AppState, owner: UserId) -> AgentId {
        let id = agent(state, owner, "a").await;
        crate::api::agents::ensure_running(state, owner, id)
            .await
            .expect("start");
        id
    }

    async fn status(state: &AppState, id: AgentId) -> AgentStatus {
        repo::find_by_id(&state.db, id)
            .await
            .expect("db")
            .expect("row")
            .status
    }

    async fn eventually<F, Fut>(what: &str, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..300 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    fn owner(user: UserId) -> crate::auth::AuthedUser {
        crate::auth::AuthedUser {
            id: user,
            display_name: "havn".into(),
        }
    }

    /// `POST /agents/{id}/stop` as the agent's owner.
    async fn stop(state: &AppState, user: UserId, id: AgentId) {
        let _ = crate::api::agents::stop(
            axum::extract::State(state.clone()),
            owner(user),
            axum::extract::Path(id.to_string()),
        )
        .await
        .expect("stop");
    }

    async fn audit_actions(state: &AppState) -> Vec<String> {
        havn_db::repo::audit::list(
            &state.db,
            havn_db::repo::audit::ListFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("audit")
        .into_iter()
        .map(|e| e.action)
        .collect()
    }

    fn count(actions: &[String], action: &str) -> usize {
        actions.iter().filter(|a| *a == action).count()
    }

    /// Mark the agent's current connection as pinging, as of now.
    async fn ping(state: &AppState, spawner: &FakeSpawner, id: AgentId) {
        let tx = spawner.connection(id);
        state.registry.frame_received(id, &tx, true).await;
        state.registry.frame_handled(id, &tx).await;
    }

    fn far_ahead(state: &AppState) -> Instant {
        Instant::now() + state.supervisor.limits.silence * 2
    }

    #[tokio::test]
    async fn a_crashed_agent_is_restarted() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;

        spawner.crash(id);

        eventually("restarted and connected", || async {
            spawner.spawns() == 2 && state.registry.is_connected(id).await
        })
        .await;
        assert_eq!(status(&state, id).await, AgentStatus::Running);
        let actions = audit_actions(&state).await;
        assert_eq!(count(&actions, "agent.exited"), 1, "{actions:?}");
        assert_eq!(count(&actions, "agent.restarted"), 1, "{actions:?}");
    }

    #[tokio::test]
    async fn start_after_a_crash_spawns_instead_of_waiting_for_the_dead_instance() {
        let policy = RestartPolicy {
            first_delay: Duration::from_secs(600),
            ..quick_policy()
        };
        let (state, spawner, user) = fixture(policy, true, false).await;
        let id = running_agent(&state, user).await;

        spawner.crash(id);
        eventually("dead instance deregistered", || async {
            !state.registry.is_registered(id).await
        })
        .await;

        tokio::time::timeout(
            Duration::from_secs(5),
            crate::api::agents::ensure_running(&state, user, id),
        )
        .await
        .expect("no handshake wait on a dead instance")
        .expect("start");
        assert_eq!(spawner.spawns(), 2);
        assert!(state.registry.is_connected(id).await);
    }

    #[tokio::test]
    async fn a_stopped_agent_stays_stopped() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;

        stop(&state, user, id).await;

        tokio::time::sleep(quick_policy().max_delay * 4).await;
        assert_eq!(spawner.spawns(), 1);
        assert!(!state.registry.is_registered(id).await);
        assert_eq!(status(&state, id).await, AgentStatus::Stopped);
        assert_eq!(count(&audit_actions(&state).await, "agent.exited"), 0);
    }

    #[tokio::test]
    async fn stop_cancels_a_pending_restart() {
        let policy = RestartPolicy {
            first_delay: Duration::from_millis(200),
            ..quick_policy()
        };
        let (state, spawner, user) = fixture(policy, true, false).await;
        let id = running_agent(&state, user).await;
        spawner.crash(id);
        eventually("crash noticed", || async {
            !state.registry.is_registered(id).await
        })
        .await;

        stop(&state, user, id).await;

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(spawner.spawns(), 1, "the pending restart must not run");
        assert_eq!(status(&state, id).await, AgentStatus::Stopped);
    }

    #[tokio::test]
    async fn stop_while_the_crash_is_being_cleaned_up_wins() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;
        // Recovery holds the start lock while the spawner cleans up.
        spawner.stop_delay_ms.store(200, Ordering::SeqCst);
        spawner.crash(id);
        eventually("crash noticed", || async {
            !state.registry.is_registered(id).await
        })
        .await;

        stop(&state, user, id).await;

        tokio::time::sleep(quick_policy().max_delay * 4).await;
        assert_eq!(spawner.spawns(), 1);
        assert_eq!(status(&state, id).await, AgentStatus::Stopped);
    }

    #[tokio::test]
    async fn delete_during_a_pending_restart_leaves_nothing_running() {
        let policy = RestartPolicy {
            first_delay: Duration::from_millis(200),
            ..quick_policy()
        };
        let (state, spawner, user) = fixture(policy, true, false).await;
        let id = running_agent(&state, user).await;
        spawner.crash(id);
        eventually("crash noticed", || async {
            !state.registry.is_registered(id).await
        })
        .await;

        crate::api::agents::delete(
            axum::extract::State(state.clone()),
            owner(user),
            axum::extract::Path(id.to_string()),
        )
        .await
        .expect("delete");

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(spawner.spawns(), 1);
        assert!(!state.registry.is_registered(id).await);
        assert!(repo::find_by_id(&state.db, id).await.expect("db").is_none());
    }

    #[tokio::test]
    async fn a_crash_loop_ends_in_error_status() {
        let (state, spawner, user) = fixture(quick_policy(), false, true).await;
        let id = agent(&state, user, "a").await;
        let first = crate::api::agents::ensure_running(&state, user, id).await;
        assert!(first.is_err(), "the first instance exits during startup");

        eventually("marked as failed", || async {
            status(&state, id).await == AgentStatus::Error
        })
        .await;
        // The first run plus max_failures restarts, then nothing more.
        assert_eq!(spawner.spawns(), 1 + quick_policy().max_failures);
        tokio::time::sleep(quick_policy().max_delay * 4).await;
        assert_eq!(spawner.spawns(), 1 + quick_policy().max_failures);
        let actions = audit_actions(&state).await;
        assert_eq!(count(&actions, "agent.restart_abandoned"), 1, "{actions:?}");
    }

    #[tokio::test]
    async fn an_auto_start_after_giving_up_gets_a_fresh_budget() {
        let (state, spawner, user) = fixture(quick_policy(), true, true).await;
        let id = agent(&state, user, "a").await;
        let _ = crate::api::agents::ensure_running(&state, user, id).await;
        eventually("marked as failed", || async {
            status(&state, id).await == AgentStatus::Error
        })
        .await;

        // The cause is fixed; a chat connect starts the agent again.
        spawner.exit_at_once.store(false, Ordering::SeqCst);
        crate::api::agents::ensure_running(&state, user, id)
            .await
            .expect("auto-start");
        let before = spawner.spawns();
        spawner.crash(id);

        eventually("restarted, not given up on", || async {
            spawner.spawns() == before + 1 && state.registry.is_connected(id).await
        })
        .await;
        assert_eq!(status(&state, id).await, AgentStatus::Running);
    }

    #[tokio::test]
    async fn restarts_retry_while_spawning_fails() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;
        spawner.fail_spawn.store(true, Ordering::SeqCst);
        spawner.crash(id);

        eventually("marked as failed", || async {
            status(&state, id).await == AgentStatus::Error
        })
        .await;
        assert_eq!(spawner.spawns(), 1);
    }

    #[tokio::test]
    async fn a_silent_agent_is_stopped_and_restarted_on_the_second_check() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;
        ping(&state, &spawner, id).await;
        let mut suspects = Suspects::new();

        check_liveness(&state, Instant::now(), &mut suspects).await;
        assert!(suspects.is_empty(), "a pinging agent is left alone");

        check_liveness(&state, far_ahead(&state), &mut suspects).await;
        assert!(
            state.registry.is_registered(id).await,
            "one check is not enough"
        );
        assert_eq!(suspects.len(), 1);

        check_liveness(&state, far_ahead(&state), &mut suspects).await;
        eventually("restarted and connected", || async {
            spawner.spawns() == 2 && state.registry.is_connected(id).await
        })
        .await;
        let actions = audit_actions(&state).await;
        assert_eq!(count(&actions, "agent.unresponsive"), 1, "{actions:?}");
    }

    #[tokio::test]
    async fn a_ping_between_checks_clears_the_suspicion() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;
        ping(&state, &spawner, id).await;
        let mut suspects = Suspects::new();

        check_liveness(&state, far_ahead(&state), &mut suspects).await;
        assert_eq!(suspects.len(), 1);
        // Buffered pings get read once the gateway catches up.
        ping(&state, &spawner, id).await;
        check_liveness(&state, Instant::now(), &mut suspects).await;
        assert!(suspects.is_empty());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(spawner.spawns(), 1);
        assert!(state.registry.is_connected(id).await);
    }

    #[tokio::test]
    async fn exit_and_silence_together_restart_once() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;
        ping(&state, &spawner, id).await;
        let mut suspects = Suspects::new();
        check_liveness(&state, far_ahead(&state), &mut suspects).await;

        spawner.crash(id);
        check_liveness(&state, far_ahead(&state), &mut suspects).await;

        eventually("restarted and connected", || async {
            spawner.spawns() == 2 && state.registry.is_connected(id).await
        })
        .await;
        tokio::time::sleep(quick_policy().max_delay * 4).await;
        assert_eq!(spawner.spawns(), 2);
        let actions = audit_actions(&state).await;
        assert_eq!(
            count(&actions, "agent.exited") + count(&actions, "agent.unresponsive"),
            1,
            "{actions:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_starts_only_agents_left_running() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let running = agent(&state, user, "running").await;
        let stopped = agent(&state, user, "stopped").await;
        let created = agent(&state, user, "created").await;
        repo::set_status(&state.db, running, AgentStatus::Running)
            .await
            .expect("status");
        repo::set_status(&state.db, stopped, AgentStatus::Stopped)
            .await
            .expect("status");

        reconcile(&state).await;

        assert_eq!(spawner.orphans_reaped.load(Ordering::SeqCst), 1);
        eventually("running agent restored", || async {
            state.registry.is_connected(running).await
        })
        .await;
        assert_eq!(spawner.spawns(), 1);
        assert!(!state.registry.is_registered(stopped).await);
        assert!(!state.registry.is_registered(created).await);
        assert_eq!(status(&state, stopped).await, AgentStatus::Stopped);
        assert_eq!(status(&state, created).await, AgentStatus::Created);
    }

    #[tokio::test]
    async fn reconcile_retries_agents_given_up_on_before() {
        let (state, _spawner, user) = fixture(quick_policy(), true, false).await;
        let id = agent(&state, user, "a").await;
        repo::set_status(&state.db, id, AgentStatus::Error)
            .await
            .expect("status");

        reconcile(&state).await;

        eventually("restored", || async {
            state.registry.is_connected(id).await
        })
        .await;
        assert_eq!(status(&state, id).await, AgentStatus::Running);
    }

    #[tokio::test]
    async fn reconcile_marks_an_agent_that_cannot_start_as_failed() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = agent(&state, user, "a").await;
        repo::set_status(&state.db, id, AgentStatus::Running)
            .await
            .expect("status");
        spawner.fail_spawn.store(true, Ordering::SeqCst);

        reconcile(&state).await;

        eventually("marked as failed", || async {
            status(&state, id).await == AgentStatus::Error
        })
        .await;
    }

    #[tokio::test]
    async fn shutdown_stops_agents_and_keeps_them_running_in_the_db() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;

        shut_down(&state).await;

        assert!(!state.registry.is_registered(id).await);
        assert!(spawner.procs.lock().unwrap().is_empty());
        tokio::time::sleep(quick_policy().max_delay * 4).await;
        assert_eq!(spawner.spawns(), 1, "nothing is restarted during shutdown");
        assert_eq!(status(&state, id).await, AgentStatus::Running);
    }

    #[tokio::test]
    async fn a_spawn_in_progress_at_shutdown_stops_its_own_instance() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = agent(&state, user, "a").await;
        spawner.spawn_delay_ms.store(200, Ordering::SeqCst);
        let start = {
            let state = state.clone();
            tokio::spawn(async move { crate::api::agents::ensure_running(&state, user, id).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        shut_down(&state).await;

        assert!(start.await.expect("join").is_err());
        assert_eq!(spawner.spawns(), 1);
        assert!(!state.registry.is_registered(id).await);
        assert!(spawner.procs.lock().unwrap().is_empty(), "instance stopped");
    }

    #[tokio::test]
    async fn drain_waits_for_agents_to_finish_their_work() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;
        eventually("connected", || async {
            state.registry.is_connected(id).await
        })
        .await;

        let drain = {
            let state = state.clone();
            tokio::spawn(async move { drain(&state, Duration::from_secs(30)).await })
        };
        let mut seen = Vec::new();
        eventually("asked to drain", || {
            seen.extend(spawner.received(id));
            let asked = seen.iter().any(|f| matches!(f, GatewayToAgent::Drain));
            async move { asked }
        })
        .await;
        assert!(
            matches!(
                state.registry.send(id, GatewayToAgent::HeartbeatTick).await,
                Err(crate::agent_registry::SendError::Draining)
            ),
            "no new work while draining"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!drain.is_finished(), "waits while the agent is busy");

        spawner.finish(id);
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain ends once the agent has left")
            .expect("join");
        assert!(
            state.registry.is_registered(id).await,
            "left for shut_down to release"
        );
        shut_down(&state).await;
        assert!(spawner.procs.lock().unwrap().is_empty(), "released");

        tokio::time::sleep(quick_policy().max_delay * 4).await;
        assert_eq!(spawner.spawns(), 1, "not restarted");
        assert_eq!(status(&state, id).await, AgentStatus::Running);
        let actions = audit_actions(&state).await;
        assert_eq!(count(&actions, "agent.exited"), 0, "{actions:?}");
    }

    #[tokio::test]
    async fn an_agent_busy_past_the_drain_limit_is_stopped_and_its_chat_told() {
        let (state, spawner, user) = fixture(quick_policy(), true, false).await;
        let id = running_agent(&state, user).await;
        eventually("connected", || async {
            state.registry.is_connected(id).await
        })
        .await;
        let (tab_tx, mut tab) = mpsc::channel(8);
        let _registration = state
            .webchat_router
            .register(uuid::Uuid::now_v7(), id, tab_tx)
            .await;

        drain(&state, Duration::from_millis(100)).await;

        match tab.try_recv() {
            Ok(crate::webchat::ServerMessage::Error { message }) => {
                assert!(message.contains("restarted"), "{message}");
            }
            other => panic!("expected a notice, got {other:?}"),
        }
        assert!(state.registry.is_connected(id).await, "left to shut_down");
        shut_down(&state).await;
        assert!(spawner.procs.lock().unwrap().is_empty(), "stopped");
    }
}
