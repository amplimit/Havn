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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

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
}

#[derive(Debug)]
struct Entry {
    handle: AgentHandle,
    /// Set after the agent's runtime process connects and exchanges Hello.
    tx: Option<mpsc::Sender<GatewayToAgent>>,
    /// In-flight caps, kept per agent so reconnecting does not reset them.
    llm_slots: Arc<Semaphore>,
    query_slots: Arc<Semaphore>,
    /// Tells the writer of the connection that attached `tx` to give up;
    /// set and cleared together with `tx`.
    disconnect: Option<Arc<Notify>>,
}

/// Caps on `LlmRequest` / `AgentQuery` frames one agent may have handled at
/// once. Each is a gateway task outside the agent's cgroup; frames over the
/// cap get an immediate error reply.
pub const MAX_INFLIGHT_LLM_REQUESTS: usize = 16;
pub const MAX_INFLIGHT_AGENT_QUERIES: usize = 8;

#[derive(Debug, Clone, Copy)]
pub enum Slot {
    Llm,
    Query,
}

#[derive(Debug, Clone, Serialize)]
#[allow(
    dead_code,
    reason = "consumed by the agent-status dashboard endpoint in the next vertical"
)]
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

    /// Record the spawner-returned handle. Subsequent `attach` will wire the tx.
    pub async fn register(&self, handle: AgentHandle) {
        let id = handle.agent_id;
        let replaced = self.inner.lock().await.insert(
            id,
            Entry {
                handle,
                tx: None,
                llm_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_LLM_REQUESTS)),
                query_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_AGENT_QUERIES)),
                disconnect: None,
            },
        );
        // Nothing can reach a connection of the replaced entry any more.
        if let Some(signal) = replaced.and_then(|e| e.disconnect) {
            signal.notify_waiters();
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
        entry.disconnect = Some(Arc::new(Notify::new()));
        Ok(())
    }

    /// Drop the outbound channel without removing the registry entry.
    /// Called on socket disconnect — the agent process may still be alive.
    pub async fn detach(&self, agent_id: AgentId, tx: &mpsc::Sender<GatewayToAgent>) {
        // Only the connection that attached `tx` may clear it; a newer
        // connection may already have taken its place.
        if let Some(entry) = self.inner.lock().await.get_mut(&agent_id)
            && entry
                .tx
                .as_ref()
                .is_some_and(|current| current.same_channel(tx))
        {
            entry.tx = None;
            entry.disconnect = None;
        }
    }

    /// Remove the entry entirely; returns the spawner handle so callers can stop the process.
    pub async fn remove(&self, agent_id: AgentId) -> Option<AgentHandle> {
        self.inner.lock().await.remove(&agent_id).map(|e| e.handle)
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
        let tx = self.sender(agent_id).await?;
        tx.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Stalled(agent_id),
            mpsc::error::TrySendError::Closed(_) => SendError::ChannelClosed,
        })
    }

    #[allow(
        dead_code,
        reason = "consumed by the agent-status dashboard endpoint in the next vertical"
    )]
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
        AgentHandle {
            agent_id: AgentId::new(),
            pid: 12345,
        }
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
