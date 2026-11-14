//! In-memory registry of running agents.
//!
//! Two-stage lifecycle:
//! 1. [`AgentRegistry::register`] — called by the lifecycle endpoint after the
//!    spawner returns a process handle. The agent is "registered but not yet
//!    connected" — its Unix socket connection has not arrived.
//! 2. [`AgentRegistry::attach`] — called by the agent socket handler after a
//!    successful Hello/Welcome handshake. Wires the outbound channel.
//!
//! Detach + remove run in reverse on disconnect / stop.
//!
//! Each registration gets a generation number. Whoever notices that an
//! instance ended (its exit, or its liveness pings stopping) removes it by
//! generation, so a stale observer can never remove a newer instance and
//! the instance an operator stopped is not mistaken for a crash.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use havn_core::AgentId;
use havn_proto::GatewayToAgent;
use havn_spawner::AgentHandle;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::warn;

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    inner: Arc<Mutex<HashMap<AgentId, Entry>>>,
    generations: Arc<AtomicU64>,
    /// Woken on every registration, for connections that arrive first.
    registered: Arc<Notify>,
    /// Set while the gateway stops: agents get no new work.
    draining: Arc<AtomicBool>,
}

#[derive(Debug)]
struct Entry {
    handle: AgentHandle,
    generation: u64,
    registered_at: Instant,
    /// Set after the agent's runtime process connects and exchanges Hello.
    tx: Option<mpsc::Sender<GatewayToAgent>>,
    /// In-flight caps, kept per agent so reconnecting does not reset them.
    llm_slots: Arc<Semaphore>,
    query_slots: Arc<Semaphore>,
    tool_slots: Arc<Semaphore>,
    /// Tells the writer of the connection that attached `tx` to give up;
    /// set and cleared together with `tx`.
    disconnect: Option<Arc<Notify>>,
    connected_at: Option<Instant>,
    /// When the connection's reader last received or finished handling a
    /// frame, and whether a `LivenessPing` has been among them.
    last_frame: Option<Instant>,
    pinging: bool,
    /// Since when the reader has been handling a frame. Gateway-side work
    /// (a slow delivery) keeps it from reading pings, which counts against
    /// the agent only past [`LivenessLimits::busy`].
    busy_since: Option<Instant>,
}

/// How long an instance may stay quiet before it counts as hung.
#[derive(Debug, Clone, Copy)]
pub struct LivenessLimits {
    /// Between frames, once the runtime has started pinging.
    pub silence: Duration,
    /// From spawn to Hello: opening agent.db, migrations, skill indexing.
    pub connect: Duration,
    /// From Hello to the first ping: embedder and MCP server start-up.
    pub first_ping: Duration,
    /// Handling one frame; past this the connection counts as hung too.
    pub busy: Duration,
}

impl Entry {
    /// How long the instance has been quiet, if that exceeds the limit
    /// for the phase it is in.
    fn overdue(&self, now: Instant, limits: &LivenessLimits) -> Option<Duration> {
        let (since, limit) = match (self.busy_since, self.pinging, self.last_frame) {
            (Some(busy), _, _) => (busy, limits.busy),
            (None, true, Some(frame)) => (frame, limits.silence),
            (None, _, _) => match self.connected_at {
                Some(connected) => (connected, limits.first_ping),
                None => (self.registered_at, limits.connect),
            },
        };
        let quiet = now.saturating_duration_since(since);
        (quiet > limit).then_some(quiet)
    }

    fn is_connection(&self, tx: &mpsc::Sender<GatewayToAgent>) -> bool {
        self.tx
            .as_ref()
            .is_some_and(|current| current.same_channel(tx))
    }
}

/// An instance that was removed from the registry, with what is needed to
/// clean it up and judge how long it ran.
#[derive(Debug, Clone)]
pub struct Instance {
    pub handle: AgentHandle,
    pub registered_at: Instant,
}

/// An instance that has been silent for longer than allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silent {
    pub agent_id: AgentId,
    pub generation: u64,
    pub silent_for: Duration,
}

/// Caps on `LlmRequest` / `AgentQuery` / `GatewayToolCall` frames one agent
/// may have handled at once. Each is a gateway task outside the agent's
/// cgroup; frames over the cap get an immediate error reply.
pub const MAX_INFLIGHT_LLM_REQUESTS: usize = 16;
pub const MAX_INFLIGHT_AGENT_QUERIES: usize = 8;
pub const MAX_INFLIGHT_TOOL_CALLS: usize = 4;

#[derive(Debug, Clone, Copy)]
pub enum Slot {
    Llm,
    Query,
    Tool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredAgent {
    pub agent_id: AgentId,
    pub pid: u32,
    pub connected: bool,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AttachError {
    #[error("agent {0} is not registered (was the lifecycle endpoint called?)")]
    NotRegistered(AgentId),
    #[error("agent {0} already has a live connection")]
    AlreadyConnected(AgentId),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SendError {
    #[error("agent {0} is registered but not connected")]
    NotConnected(AgentId),
    #[error("agent {0} not registered")]
    NotRegistered(AgentId),
    #[error("outbound channel closed")]
    ChannelClosed,
    #[error("agent {0} is not draining its outbound queue")]
    Stalled(AgentId),
    #[error("the gateway is restarting; send it again once it is back")]
    Draining,
}

/// Longest a sender waits for room in an agent's outbound queue. An agent
/// whose queue stays full that long is not keeping up, so it is
/// disconnected: anything it is waiting for then fails at once instead of
/// being silently dropped.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

impl AgentRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the spawner-returned handle. Subsequent `attach` will wire the
    /// tx. Returns the generation that identifies this instance.
    pub async fn register(&self, handle: AgentHandle) -> u64 {
        let id = handle.agent_id;
        let generation = self.generations.fetch_add(1, Ordering::Relaxed) + 1;
        let replaced = self.inner.lock().await.insert(
            id,
            Entry {
                handle,
                generation,
                registered_at: Instant::now(),
                tx: None,
                connected_at: None,
                last_frame: None,
                pinging: false,
                busy_since: None,
                llm_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_LLM_REQUESTS)),
                query_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_AGENT_QUERIES)),
                tool_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_TOOL_CALLS)),
                disconnect: None,
            },
        );
        // Nothing can reach a connection of the replaced entry any more.
        if let Some(signal) = replaced.and_then(|e| e.disconnect) {
            signal.notify_waiters();
        }
        self.registered.notify_waiters();
        generation
    }

    /// Wait up to `limit` for `agent_id` to be registered. The spawner
    /// returns the handle only after the runtime has started, so under load
    /// its Hello can arrive before the registration it belongs to.
    pub async fn wait_registered(&self, agent_id: AgentId, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let registered = self.registered.notified();
            tokio::pin!(registered);
            registered.as_mut().enable();
            if self.is_registered(agent_id).await {
                return true;
            }
            if tokio::time::timeout_at(deadline, registered).await.is_err() {
                return self.is_registered(agent_id).await;
            }
        }
    }

    /// Take one in-flight slot for `agent_id`, or `None` when its cap is
    /// reached or it is not registered.
    pub async fn try_reserve(&self, agent_id: AgentId, slot: Slot) -> Option<OwnedSemaphorePermit> {
        let slots = {
            let map = self.inner.lock().await;
            let entry = map.get(&agent_id)?;
            match slot {
                Slot::Llm => Arc::clone(&entry.llm_slots),
                Slot::Query => Arc::clone(&entry.query_slots),
                Slot::Tool => Arc::clone(&entry.tool_slots),
            }
        };
        slots.try_acquire_owned().ok()
    }

    /// The signal that closes the connection that attached `tx`. A waiter
    /// only sees signals sent after it called `notified()`.
    pub async fn disconnect_signal(
        &self,
        agent_id: AgentId,
        tx: &mpsc::Sender<GatewayToAgent>,
    ) -> Option<Arc<Notify>> {
        let map = self.inner.lock().await;
        let entry = map.get(&agent_id)?;
        if !entry.tx.as_ref().is_some_and(|t| t.same_channel(tx)) {
            return None;
        }
        entry.disconnect.clone()
    }

    /// Close the agent's live connection, or only the one that attached
    /// `tx` when given; checked and signalled under one lock.
    pub async fn disconnect(&self, agent_id: AgentId, tx: Option<&mpsc::Sender<GatewayToAgent>>) {
        let map = self.inner.lock().await;
        let Some(entry) = map.get(&agent_id) else {
            return;
        };
        let current = match (tx, entry.tx.as_ref()) {
            (Some(tx), Some(live)) => live.same_channel(tx),
            (None, live) => live.is_some(),
            (Some(_), None) => false,
        };
        if current && let Some(signal) = &entry.disconnect {
            warn!(%agent_id, "agent is not keeping up with its outbound queue; disconnecting");
            signal.notify_waiters();
        }
    }

    /// Wire the outbound channel after a successful Hello/Welcome.
    pub async fn attach(
        &self,
        agent_id: AgentId,
        tx: mpsc::Sender<GatewayToAgent>,
    ) -> Result<(), AttachError> {
        let mut map = self.inner.lock().await;
        let entry = map
            .get_mut(&agent_id)
            .ok_or(AttachError::NotRegistered(agent_id))?;
        // A second connection must not take over a live one; the runtime
        // only reconnects after its previous connection is gone.
        if entry.tx.as_ref().is_some_and(|live| !live.is_closed()) {
            return Err(AttachError::AlreadyConnected(agent_id));
        }
        entry.tx = Some(tx);
        entry.connected_at = Some(Instant::now());
        entry.disconnect = Some(Arc::new(Notify::new()));
        Ok(())
    }

    /// The reader of connection `tx` received a frame and starts handling
    /// it. Any frame is a sign of life; a ping also arms the silence check.
    pub async fn frame_received(
        &self,
        agent_id: AgentId,
        tx: &mpsc::Sender<GatewayToAgent>,
        ping: bool,
    ) {
        self.frame_received_at(agent_id, tx, ping, Instant::now())
            .await;
    }

    async fn frame_received_at(
        &self,
        agent_id: AgentId,
        tx: &mpsc::Sender<GatewayToAgent>,
        ping: bool,
        at: Instant,
    ) {
        if let Some(entry) = self.inner.lock().await.get_mut(&agent_id)
            && entry.is_connection(tx)
        {
            entry.last_frame = Some(at);
            entry.pinging |= ping;
            entry.busy_since = Some(at);
        }
    }

    /// The reader of connection `tx` is done with its frame and reads again.
    pub async fn frame_handled(&self, agent_id: AgentId, tx: &mpsc::Sender<GatewayToAgent>) {
        if let Some(entry) = self.inner.lock().await.get_mut(&agent_id)
            && entry.is_connection(tx)
        {
            entry.last_frame = Some(Instant::now());
            entry.busy_since = None;
        }
    }

    /// Instances quiet for longer than `limits` allow (see [`Entry::overdue`]).
    pub async fn silent(&self, now: Instant, limits: &LivenessLimits) -> Vec<Silent> {
        self.inner
            .lock()
            .await
            .iter()
            .filter_map(|(id, e)| {
                e.overdue(now, limits).map(|silent_for| Silent {
                    agent_id: *id,
                    generation: e.generation,
                    silent_for,
                })
            })
            .collect()
    }

    /// Drop the outbound channel without removing the registry entry.
    /// Called on socket disconnect — the agent process may still be alive.
    pub async fn detach(&self, agent_id: AgentId, tx: &mpsc::Sender<GatewayToAgent>) {
        // Only the connection that attached `tx` may clear it; a newer
        // connection may already have taken its place.
        if let Some(entry) = self.inner.lock().await.get_mut(&agent_id)
            && entry.is_connection(tx)
        {
            entry.tx = None;
            entry.busy_since = None;
            entry.disconnect = None;
        }
    }

    /// Remove the entry entirely; returns the spawner handle so callers can stop the process.
    pub async fn remove(&self, agent_id: AgentId) -> Option<AgentHandle> {
        self.inner.lock().await.remove(&agent_id).map(|e| e.handle)
    }

    /// Remove the entry only if it is still instance `generation`.
    pub async fn remove_generation(&self, agent_id: AgentId, generation: u64) -> Option<Instance> {
        let mut map = self.inner.lock().await;
        if map.get(&agent_id)?.generation != generation {
            return None;
        }
        map.remove(&agent_id).map(|e| Instance {
            handle: e.handle,
            registered_at: e.registered_at,
        })
    }

    /// Remove every entry (gateway shutdown).
    pub async fn drain(&self) -> Vec<AgentHandle> {
        self.inner
            .lock()
            .await
            .drain()
            .map(|(_, e)| e.handle)
            .collect()
    }

    /// Stop handing agents new work: from now on [`Self::send`] and
    /// [`Self::try_send`] refuse frames that start it. Frames serving work
    /// already in flight still go through.
    pub fn start_draining(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    fn refuses(&self, frame: &GatewayToAgent) -> bool {
        self.draining.load(Ordering::SeqCst)
            && matches!(
                frame,
                GatewayToAgent::InboundMessage(_)
                    | GatewayToAgent::HeartbeatTick
                    | GatewayToAgent::CronTick(_)
            )
    }

    async fn sender(&self, agent_id: AgentId) -> Result<mpsc::Sender<GatewayToAgent>, SendError> {
        let map = self.inner.lock().await;
        let entry = map
            .get(&agent_id)
            .ok_or(SendError::NotRegistered(agent_id))?;
        entry.tx.clone().ok_or(SendError::NotConnected(agent_id))
    }

    /// Send a frame to a connected agent, waiting at most [`SEND_TIMEOUT`]
    /// for queue space; on timeout the agent is disconnected. Errors if not
    /// registered, not connected or stalled.
    pub async fn send(&self, agent_id: AgentId, frame: GatewayToAgent) -> Result<(), SendError> {
        if self.refuses(&frame) {
            return Err(SendError::Draining);
        }
        let tx = self.sender(agent_id).await?;
        match tx.send_timeout(frame, SEND_TIMEOUT).await {
            Ok(()) => Ok(()),
            Err(mpsc::error::SendTimeoutError::Closed(_)) => Err(SendError::ChannelClosed),
            Err(mpsc::error::SendTimeoutError::Timeout(_)) => {
                // Only the connection that stalled; a newer one may be live.
                self.disconnect(agent_id, Some(&tx)).await;
                Err(SendError::Stalled(agent_id))
            }
        }
    }

    /// Like [`Self::send`], but fails at once when the queue is full. For
    /// callers that serve many agents in one pass.
    pub async fn try_send(
        &self,
        agent_id: AgentId,
        frame: GatewayToAgent,
    ) -> Result<(), SendError> {
        if self.refuses(&frame) {
            return Err(SendError::Draining);
        }
        let tx = self.sender(agent_id).await?;
        tx.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Stalled(agent_id),
            mpsc::error::TrySendError::Closed(_) => SendError::ChannelClosed,
        })
    }

    pub async fn list(&self) -> Vec<RegisteredAgent> {
        self.inner
            .lock()
            .await
            .iter()
            .map(|(id, e)| RegisteredAgent {
                agent_id: *id,
                pid: e.handle.pid,
                connected: e.tx.as_ref().is_some_and(|tx| !tx.is_closed()),
            })
            .collect()
    }

    pub async fn is_connected(&self, agent_id: AgentId) -> bool {
        self.inner
            .lock()
            .await
            .get(&agent_id)
            .is_some_and(|e| e.tx.as_ref().is_some_and(|tx| !tx.is_closed()))
    }

    /// True if the agent has been spawned (handle present in the
    /// registry) regardless of whether it's finished its Hello/Welcome
    /// handshake. Used by `ensure_running` to distinguish "needs spawn"
    /// from "spawned, awaiting handshake".
    pub async fn is_registered(&self, agent_id: AgentId) -> bool {
        self.inner.lock().await.contains_key(&agent_id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn fake_handle() -> AgentHandle {
        AgentHandle::new(AgentId::new(), 12345)
    }

    #[tokio::test]
    async fn register_then_attach_then_send() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;

        reg.register(handle).await;

        let (tx, mut rx) = mpsc::channel::<GatewayToAgent>(8);
        reg.attach(id, tx).await.expect("attach");

        reg.send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect("send");
        let frame = rx.recv().await.expect("frame");
        assert!(matches!(frame, GatewayToAgent::HeartbeatTick));

        let listed = reg.list().await;
        assert_eq!(listed.len(), 1);
        assert!(listed[0].connected);
    }

    #[tokio::test]
    async fn attach_refuses_to_replace_a_live_connection() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;

        let (first, mut first_rx) = mpsc::channel::<GatewayToAgent>(8);
        reg.attach(id, first).await.expect("first attach");
        let (second, _second_rx) = mpsc::channel::<GatewayToAgent>(8);
        let err = reg
            .attach(id, second)
            .await
            .expect_err("second connection must be refused");
        assert!(matches!(err, AttachError::AlreadyConnected(_)), "{err:?}");

        reg.send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect("send");
        assert!(
            matches!(first_rx.recv().await, Some(GatewayToAgent::HeartbeatTick)),
            "frames keep going to the original connection"
        );
    }

    #[tokio::test]
    async fn attach_after_the_previous_connection_closed_succeeds() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;

        let (first, first_rx) = mpsc::channel::<GatewayToAgent>(8);
        reg.attach(id, first).await.expect("first attach");
        drop(first_rx);
        let (second, mut second_rx) = mpsc::channel::<GatewayToAgent>(8);
        reg.attach(id, second).await.expect("reconnect");
        reg.send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect("send");
        assert!(matches!(
            second_rx.recv().await,
            Some(GatewayToAgent::HeartbeatTick)
        ));
    }

    #[tokio::test]
    async fn attach_without_register_fails() {
        let reg = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel::<GatewayToAgent>(8);
        let err = reg
            .attach(AgentId::new(), tx)
            .await
            .expect_err("should fail");
        assert!(matches!(err, AttachError::NotRegistered(_)));
    }

    #[tokio::test]
    async fn send_without_connection_fails() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;

        let err = reg
            .send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect_err("not connected");
        assert!(matches!(err, SendError::NotConnected(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn send_to_a_full_queue_gives_up_and_disconnects() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;
        let (tx, _rx) = mpsc::channel::<GatewayToAgent>(1);
        reg.attach(id, tx.clone()).await.expect("attach");
        reg.send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect("first frame fits");
        let signal = reg.disconnect_signal(id, &tx).await.expect("connected");
        let disconnected = signal.notified();

        let started = tokio::time::Instant::now();
        let err = reg
            .send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect_err("a full queue must not block forever");
        assert!(matches!(err, SendError::Stalled(_)), "{err:?}");
        assert!(started.elapsed() >= SEND_TIMEOUT);

        let err = reg
            .try_send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect_err("try_send fails at once");
        assert!(matches!(err, SendError::Stalled(_)), "{err:?}");
        tokio::time::timeout(Duration::from_secs(1), disconnected)
            .await
            .expect("the stalled connection is told to close");
    }

    #[tokio::test]
    async fn disconnect_reaches_only_the_connection_it_names() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;
        let (old, old_rx) = mpsc::channel::<GatewayToAgent>(1);
        reg.attach(id, old.clone()).await.expect("attach");
        let old_signal = reg.disconnect_signal(id, &old).await.expect("old");
        drop(old_rx);
        let (new, _new_rx) = mpsc::channel::<GatewayToAgent>(1);
        reg.attach(id, new.clone()).await.expect("reconnect");
        let new_signal = reg.disconnect_signal(id, &new).await.expect("new");
        assert!(reg.disconnect_signal(id, &old).await.is_none());
        let new_disconnected = new_signal.notified();
        tokio::pin!(new_disconnected);

        reg.disconnect(id, Some(&old)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut new_disconnected)
                .await
                .is_err(),
            "a stall on the old connection leaves the new one alone"
        );

        reg.disconnect(id, None).await;
        tokio::time::timeout(Duration::from_secs(1), new_disconnected)
            .await
            .expect("the live connection is closed");
        drop(old_signal);
    }

    #[tokio::test]
    async fn inflight_slots_survive_a_reconnect() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;
        let (first, first_rx) = mpsc::channel::<GatewayToAgent>(1);
        reg.attach(id, first).await.expect("attach");
        let mut held = Vec::new();
        for _ in 0..MAX_INFLIGHT_LLM_REQUESTS {
            held.push(reg.try_reserve(id, Slot::Llm).await.expect("slot"));
        }
        assert!(reg.try_reserve(id, Slot::Llm).await.is_none());

        drop(first_rx);
        let (second, _second_rx) = mpsc::channel::<GatewayToAgent>(1);
        reg.attach(id, second).await.expect("reconnect");
        assert!(
            reg.try_reserve(id, Slot::Llm).await.is_none(),
            "reconnecting does not hand out more slots"
        );
        assert!(reg.try_reserve(id, Slot::Query).await.is_some());
        drop(held);
        assert!(reg.try_reserve(id, Slot::Llm).await.is_some());
    }

    #[tokio::test]
    async fn detach_keeps_handle() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;

        let (tx, _rx) = mpsc::channel(8);
        reg.attach(id, tx.clone()).await.expect("attach");
        reg.detach(id, &tx).await;

        assert!(!reg.is_connected(id).await);
        // Handle still present.
        assert!(reg.remove(id).await.is_some());
    }

    #[tokio::test]
    async fn wait_registered_returns_once_the_agent_is_registered() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        let waiter = {
            let reg = reg.clone();
            tokio::spawn(async move { reg.wait_registered(id, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        reg.register(handle).await;
        let found = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("woken by the registration")
            .expect("join");
        assert!(found);

        let missing = AgentId::new();
        assert!(
            !reg.wait_registered(missing, Duration::from_millis(50))
                .await
        );
    }

    #[tokio::test]
    async fn remove_generation_only_removes_that_instance() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        let first = reg.register(handle.clone()).await;
        let second = reg.register(handle).await;
        assert_ne!(first, second);

        assert!(
            reg.remove_generation(id, first).await.is_none(),
            "an observer of the old instance must not remove the new one"
        );
        assert!(reg.is_registered(id).await);
        let removed = reg.remove_generation(id, second).await.expect("current");
        assert_eq!(removed.handle.agent_id, id);
        assert!(!reg.is_registered(id).await);
        assert!(reg.remove_generation(id, second).await.is_none());
    }

    const LIMITS: LivenessLimits = LivenessLimits {
        silence: Duration::from_secs(60),
        connect: Duration::from_secs(300),
        first_ping: Duration::from_secs(600),
        busy: Duration::from_secs(600),
    };

    #[tokio::test]
    async fn silence_counts_from_the_last_frame_once_pinging() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        let generation = reg.register(handle).await;
        let (tx, _rx) = mpsc::channel(8);
        reg.attach(id, tx.clone()).await.expect("attach");
        let start = Instant::now();

        // Connected, never pinged: still starting up, judged only by the
        // generous first-ping limit.
        assert!(
            reg.silent(start + LIMITS.silence * 5, &LIMITS)
                .await
                .is_empty()
        );
        assert_eq!(
            reg.silent(start + LIMITS.first_ping * 2, &LIMITS)
                .await
                .len(),
            1
        );

        reg.frame_received_at(id, &tx, true, start).await;
        reg.frame_handled(id, &tx).await;
        let handled = Instant::now();
        assert!(
            reg.silent(handled + LIMITS.silence / 2, &LIMITS)
                .await
                .is_empty()
        );
        let silent = reg.silent(handled + LIMITS.silence * 2, &LIMITS).await;
        assert_eq!(silent.len(), 1);
        assert_eq!(silent[0].generation, generation);
        assert!(silent[0].silent_for >= LIMITS.silence * 2);

        // Any frame counts as a sign of life, a ping or not.
        let later = handled + LIMITS.silence * 2;
        reg.frame_received_at(id, &tx, false, later).await;
        assert!(reg.silent(later, &LIMITS).await.is_empty());
        reg.frame_handled(id, &tx).await;
        let handled = Instant::now();
        assert!(
            reg.silent(handled + LIMITS.silence / 2, &LIMITS)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_agent_is_not_judged_while_its_reader_is_busy() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;
        let (tx, _rx) = mpsc::channel(8);
        reg.attach(id, tx.clone()).await.expect("attach");
        let start = Instant::now();

        // A frame whose delivery stalls on the gateway side: excused for a
        // while, but not forever.
        reg.frame_received_at(id, &tx, true, start).await;
        assert!(
            reg.silent(start + LIMITS.silence * 5, &LIMITS)
                .await
                .is_empty()
        );
        assert_eq!(
            reg.silent(start + LIMITS.busy * 2, &LIMITS).await.len(),
            1,
            "a reader stuck on one frame for too long counts as hung"
        );
        reg.frame_handled(id, &tx).await;
        assert_eq!(
            reg.silent(Instant::now() + LIMITS.silence * 2, &LIMITS)
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn frames_from_an_old_connection_do_not_count() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;
        let (old, old_rx) = mpsc::channel(8);
        reg.attach(id, old.clone()).await.expect("attach");
        drop(old_rx);
        let (new, _new_rx) = mpsc::channel(8);
        reg.attach(id, new).await.expect("reconnect");

        reg.frame_received_at(id, &old, true, Instant::now()).await;
        assert!(
            reg.silent(Instant::now() + LIMITS.silence * 2, &LIMITS)
                .await
                .is_empty(),
            "a ping from the old connection must not arm the new one"
        );
    }

    #[tokio::test]
    async fn an_instance_that_never_connects_counts_as_silent() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;
        let now = Instant::now();
        assert!(
            reg.silent(now + LIMITS.silence * 2, &LIMITS)
                .await
                .is_empty()
        );
        let silent = reg.silent(now + LIMITS.connect * 2, &LIMITS).await;
        assert_eq!(silent.len(), 1);
        assert_eq!(silent[0].agent_id, id);
    }

    #[tokio::test]
    async fn stale_detach_leaves_a_newer_connection_alone() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;

        let (old, old_rx) = mpsc::channel::<GatewayToAgent>(8);
        reg.attach(id, old.clone()).await.expect("first attach");
        drop(old_rx);
        let (new, mut new_rx) = mpsc::channel::<GatewayToAgent>(8);
        reg.attach(id, new).await.expect("reconnect");

        reg.detach(id, &old).await;

        assert!(
            reg.is_connected(id).await,
            "the new connection must survive"
        );
        reg.send(id, GatewayToAgent::HeartbeatTick)
            .await
            .expect("send");
        assert!(matches!(
            new_rx.recv().await,
            Some(GatewayToAgent::HeartbeatTick)
        ));
    }
}
