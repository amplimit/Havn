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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use havn_core::AgentId;
use havn_proto::GatewayToAgent;
use havn_spawner::AgentHandle;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    inner: Arc<Mutex<HashMap<AgentId, Entry>>>,
    generations: Arc<AtomicU64>,
}

#[derive(Debug)]
struct Entry {
    handle: AgentHandle,
    generation: u64,
    registered_at: Instant,
    /// Set after the agent's runtime process connects and exchanges Hello.
    tx: Option<mpsc::Sender<GatewayToAgent>>,
    connected_at: Option<Instant>,
    last_ping: Option<Instant>,
}

impl Entry {
    /// How long the instance has given no sign of life, when that counts
    /// against it: since its last ping, or since registration while it has
    /// never connected. A connected runtime that has not pinged yet is
    /// still starting up (loading embeddings, starting MCP servers) and is
    /// not judged.
    fn silent_for(&self, now: Instant) -> Option<Duration> {
        match (self.last_ping, self.connected_at) {
            (Some(ping), _) => Some(now.saturating_duration_since(ping)),
            (None, None) => Some(now.saturating_duration_since(self.registered_at)),
            (None, Some(_)) => None,
        }
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
}

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
        self.inner.lock().await.insert(
            id,
            Entry {
                handle,
                generation,
                registered_at: Instant::now(),
                tx: None,
                connected_at: None,
                last_ping: None,
            },
        );
        generation
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
        Ok(())
    }

    /// Note a `LivenessPing` from the agent's current instance.
    pub async fn record_ping(&self, agent_id: AgentId) {
        self.record_ping_at(agent_id, Instant::now()).await;
    }

    async fn record_ping_at(&self, agent_id: AgentId, at: Instant) {
        if let Some(entry) = self.inner.lock().await.get_mut(&agent_id) {
            entry.last_ping = Some(at);
        }
    }

    /// Instances that have given no sign of life for longer than `limit`
    /// (see [`Entry::silent_for`]).
    pub async fn silent(&self, now: Instant, limit: Duration) -> Vec<Silent> {
        self.inner
            .lock()
            .await
            .iter()
            .filter_map(|(id, e)| {
                let silent_for = e.silent_for(now)?;
                (silent_for > limit).then_some(Silent {
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
            && entry
                .tx
                .as_ref()
                .is_some_and(|current| current.same_channel(tx))
        {
            entry.tx = None;
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

    /// Send a frame to a connected agent. Errors if not registered or not connected.
    pub async fn send(&self, agent_id: AgentId, frame: GatewayToAgent) -> Result<(), SendError> {
        let tx = {
            let map = self.inner.lock().await;
            let entry = map
                .get(&agent_id)
                .ok_or(SendError::NotRegistered(agent_id))?;
            entry.tx.clone().ok_or(SendError::NotConnected(agent_id))?
        };
        tx.send(frame).await.map_err(|_| SendError::ChannelClosed)
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

    #[tokio::test]
    async fn silence_counts_from_the_last_ping() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        let generation = reg.register(handle).await;
        let (tx, _rx) = mpsc::channel(8);
        reg.attach(id, tx).await.expect("attach");
        let limit = Duration::from_secs(60);
        let start = Instant::now();

        // Connected, never pinged: still starting up, not judged.
        assert!(reg.silent(start + limit * 10, limit).await.is_empty());

        reg.record_ping_at(id, start).await;
        assert!(reg.silent(start + limit, limit).await.is_empty());
        assert_eq!(
            reg.silent(start + limit * 2, limit).await,
            vec![Silent {
                agent_id: id,
                generation,
                silent_for: limit * 2,
            }]
        );

        reg.record_ping_at(id, start + limit * 2).await;
        assert!(reg.silent(start + limit * 2, limit).await.is_empty());
    }

    #[tokio::test]
    async fn an_instance_that_never_connects_counts_as_silent() {
        let reg = AgentRegistry::new();
        let handle = fake_handle();
        let id = handle.agent_id;
        reg.register(handle).await;
        let limit = Duration::from_secs(60);
        let silent = reg.silent(Instant::now() + limit * 2, limit).await;
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
