//! Per-agent heartbeat scheduler — implements spec §9.6 dispatch side.
//!
//! Every [`TICK_RESOLUTION`], the scheduler walks the agent registry and
//! sends a `HeartbeatTick` frame to any connected agent whose configured
//! interval has elapsed. The interval is read from `Agent.config.heartbeat.
//! interval_seconds`; if absent, [`DEFAULT_INTERVAL`] is used (30 minutes
//! per spec). State is in-memory: an agent that disconnects has its tick
//! state forgotten, which is correct behaviour — a fresh runtime session
//! starts a fresh heartbeat cadence.
//!
//! The agent runtime side handles the tick — the scheduler is intentionally
//! ignorant of HEARTBEAT.md content and `HEARTBEAT_OK` suppression (spec
//! §9.6 step 2).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use havn_core::AgentId;
use havn_db::repo::agents;
use havn_proto::GatewayToAgent;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::agent_registry::AgentRegistry;

/// Default per-agent heartbeat interval if the agent's config doesn't override.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Cadence at which the scheduler checks whether any agents are due for a tick.
/// Lower than the minimum sane interval so jitter stays bounded.
pub const TICK_RESOLUTION: Duration = Duration::from_secs(30);

/// Floor for per-agent intervals. Anything shorter is clamped — bounds load on
/// the gateway and prevents config-driven self-DoS.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct HeartbeatScheduler {
    registry: AgentRegistry,
    db: SqlitePool,
    state: Arc<Mutex<HashMap<AgentId, Instant>>>,
}

impl HeartbeatScheduler {
    pub fn new(registry: AgentRegistry, db: SqlitePool) -> Self {
        Self {
            registry,
            db,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn run(self) {
        info!(
            tick_resolution_s = TICK_RESOLUTION.as_secs(),
            default_interval_s = DEFAULT_INTERVAL.as_secs(),
            "heartbeat scheduler started"
        );
        let mut interval = tokio::time::interval(TICK_RESOLUTION);
        // Skip the immediate first fire — `interval()` ticks at t=0 by default.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            self.tick_once(Instant::now()).await;
        }
    }

    /// One tick pass — public for testability.
    pub async fn tick_once(&self, now: Instant) {
        let connected: Vec<AgentId> = self
            .registry
            .list()
            .await
            .into_iter()
            .filter(|a| a.connected)
            .map(|a| a.agent_id)
            .collect();

        let mut state = self.state.lock().await;

        for agent_id in &connected {
            let interval = self.interval_for(*agent_id).await;
            // First observation: record `now` and skip — the first heartbeat
            // fires `interval` after the agent first connects, not immediately.
            let last = *state.entry(*agent_id).or_insert(now);
            if now.saturating_duration_since(last) < interval {
                continue;
            }
            match self
                .registry
                .send(*agent_id, GatewayToAgent::HeartbeatTick)
                .await
            {
                Ok(()) => {
                    debug!(%agent_id, "heartbeat tick sent");
                    state.insert(*agent_id, now);
                }
                Err(e) => {
                    warn!(%agent_id, error = %e, "could not deliver HeartbeatTick");
                }
            }
        }

        // Forget state for agents that disconnected.
        state.retain(|id, _| connected.contains(id));
    }

    async fn interval_for(&self, agent_id: AgentId) -> Duration {
        let raw = match agents::find_by_id(&self.db, agent_id).await {
            Ok(Some(agent)) => agent
                .config
                .get("heartbeat")
                .and_then(|h| h.get("interval_seconds"))
                .and_then(serde_json::Value::as_u64)
                .map_or(DEFAULT_INTERVAL, Duration::from_secs),
            _ => DEFAULT_INTERVAL,
        };
        raw.max(MIN_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::UserId;
    use havn_db::connect_in_memory;
    use havn_db::repo::agents::NewAgent;
    use havn_db::repo::users::{NewUser, create as create_user};
    use havn_spawner::AgentHandle;

    async fn fixture() -> (HeartbeatScheduler, AgentRegistry, SqlitePool, UserId) {
        let db = connect_in_memory().await.expect("db");
        let user = create_user(&db, NewUser { display_name: "u" })
            .await
            .expect("user");
        let registry = AgentRegistry::new();
        let scheduler = HeartbeatScheduler::new(registry.clone(), db.clone());
        (scheduler, registry, db, user.id)
    }

    #[tokio::test]
    async fn first_observation_does_not_fire() {
        let (scheduler, registry, db, owner) = fixture().await;
        let agent = havn_db::repo::agents::create(
            &db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");

        registry
            .register(AgentHandle {
                agent_id: agent.id,
                pid: 1,
            })
            .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<GatewayToAgent>(8);
        registry.attach(agent.id, tx).await.expect("attach");

        scheduler.tick_once(Instant::now()).await;
        assert!(rx.try_recv().is_err(), "first tick must not fire");
    }

    #[tokio::test]
    async fn fires_after_interval_elapsed() {
        let (scheduler, registry, db, owner) = fixture().await;
        // 60-second interval (the floor).
        let agent = havn_db::repo::agents::create(
            &db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({"heartbeat": {"interval_seconds": 60}}),
            },
        )
        .await
        .expect("agent");

        registry
            .register(AgentHandle {
                agent_id: agent.id,
                pid: 1,
            })
            .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<GatewayToAgent>(8);
        registry.attach(agent.id, tx).await.expect("attach");

        let t0 = Instant::now();
        scheduler.tick_once(t0).await;
        assert!(rx.try_recv().is_err());

        // Simulate 61 seconds later — should fire.
        let t1 = t0 + Duration::from_secs(61);
        scheduler.tick_once(t1).await;
        let frame = rx.recv().await.expect("tick");
        assert!(matches!(frame, GatewayToAgent::HeartbeatTick));
    }

    #[tokio::test]
    async fn config_below_floor_is_clamped() {
        let (scheduler, _, db, owner) = fixture().await;
        let agent = havn_db::repo::agents::create(
            &db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({"heartbeat": {"interval_seconds": 1}}),
            },
        )
        .await
        .expect("agent");
        let interval = scheduler.interval_for(agent.id).await;
        assert_eq!(interval, MIN_INTERVAL);
    }

    #[tokio::test]
    async fn missing_config_uses_default() {
        let (scheduler, _, db, owner) = fixture().await;
        let agent = havn_db::repo::agents::create(
            &db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        let interval = scheduler.interval_for(agent.id).await;
        assert_eq!(interval, DEFAULT_INTERVAL);
    }

    #[tokio::test]
    async fn disconnect_clears_state() {
        let (scheduler, registry, db, owner) = fixture().await;
        let agent = havn_db::repo::agents::create(
            &db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");

        registry
            .register(AgentHandle {
                agent_id: agent.id,
                pid: 1,
            })
            .await;
        let (tx, _rx) = tokio::sync::mpsc::channel::<GatewayToAgent>(8);
        registry.attach(agent.id, tx).await.expect("attach");

        scheduler.tick_once(Instant::now()).await;
        assert_eq!(scheduler.state.lock().await.len(), 1);

        registry.detach(agent.id).await;
        scheduler.tick_once(Instant::now()).await;
        assert!(scheduler.state.lock().await.is_empty());
    }
}
