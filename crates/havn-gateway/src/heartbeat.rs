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
//! §9.6 step 2). The tick names where the result goes (issue #226):
//! `heartbeat.deliver` is `"last"` (the default: the owner's conversation
//! they wrote in last), `"web"`, or a channel such as `"telegram"`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use havn_core::AgentId;
use havn_db::repo::agents::{self, Agent};
use havn_proto::{GatewayToAgent, HeartbeatTick};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::agent_registry::AgentRegistry;
use crate::config::{ChannelBindingConfig, ChannelEntry};

/// The `[channels]` of the gateway's config.
pub type Channels = std::collections::BTreeMap<String, ChannelEntry>;

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
    bindings: Arc<Vec<ChannelBindingConfig>>,
    channels: Arc<Channels>,
    state: Arc<Mutex<HashMap<AgentId, Instant>>>,
}

/// Where `heartbeat.deliver` sends results by default.
pub const DELIVER_LAST: &str = "last";

/// The `heartbeat.deliver` that keeps results in the web chat only.
pub const DELIVER_WEB: &str = "web";

/// The tick for `agent`: the owner's web chat, and the owner's
/// conversations with it that `heartbeat.deliver` lets the result go to:
/// their chats in the chat apps they linked, as the dashboard lists them,
/// that the agent may send to.
pub async fn tick_for(
    db: &SqlitePool,
    bindings: &[ChannelBindingConfig],
    channels: &Channels,
    agent: &Agent,
) -> HeartbeatTick {
    let web = crate::webchat::session_for(&agent.owner_id.to_string(), agent.id).to_string();
    let deliver = agent
        .config
        .get("heartbeat")
        .and_then(|h| h.get("deliver"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(DELIVER_LAST);
    if deliver == DELIVER_WEB {
        return HeartbeatTick {
            web,
            to: Vec::new(),
        };
    }
    let linked = match crate::api::conversation::linked_scopes(
        db,
        bindings,
        agent.owner_id,
        agent.id,
    )
    .await
    {
        Ok(linked) => linked,
        Err(e) => {
            warn!(agent_id = %agent.id, error = %e, "could not list the owner's linked accounts");
            Vec::new()
        }
    };
    let mut to = Vec::new();
    if deliver == DELIVER_LAST {
        to.push(web.clone());
    }
    for l in linked
        .into_iter()
        .filter(|l| deliver == DELIVER_LAST || l.channel == deliver)
    {
        let permitted = crate::channel_router::send_permitted(
            db,
            bindings,
            channels,
            agent.id,
            &l.channel,
            &l.account_id,
            &l.sender_id,
        )
        .await;
        match permitted {
            Ok(()) => to.push(l.scope),
            Err(refusal) => debug!(
                agent_id = %agent.id,
                channel = l.channel,
                reason = refusal.as_str(),
                "the owner's chat-app account cannot be sent to"
            ),
        }
    }
    HeartbeatTick { web, to }
}

impl HeartbeatScheduler {
    pub fn new(
        registry: AgentRegistry,
        db: SqlitePool,
        bindings: Arc<Vec<ChannelBindingConfig>>,
        channels: Arc<Channels>,
    ) -> Self {
        Self {
            registry,
            db,
            bindings,
            channels,
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

        let mut intervals = Vec::with_capacity(connected.len());
        for agent_id in &connected {
            // An agent that cannot be read now is tried on the next pass.
            match agents::find_by_id(&self.db, *agent_id).await {
                Ok(Some(agent)) => intervals.push((interval_of(Some(&agent)), agent)),
                Ok(None) => {}
                Err(e) => {
                    warn!(%agent_id, error = %e, "could not read the agent for its heartbeat")
                }
            }
        }

        // First observation: record `now` and skip — the first heartbeat
        // fires `interval` after the agent first connects, not immediately.
        let due: Vec<Agent> = {
            let mut state = self.state.lock().await;
            intervals
                .into_iter()
                .filter(|(interval, agent)| {
                    let last = *state.entry(agent.id).or_insert(now);
                    now.saturating_duration_since(last) >= *interval
                })
                .map(|(_, agent)| agent)
                .collect()
        };

        // One pass serves every agent, so it never waits on one agent's
        // queue: an agent whose queue is full misses this tick and is
        // retried on the next pass.
        for agent in due {
            let tick = tick_for(&self.db, &self.bindings, &self.channels, &agent).await;
            match self
                .registry
                .try_send(agent.id, GatewayToAgent::HeartbeatTick(tick))
                .await
            {
                Ok(()) => {
                    debug!(agent_id = %agent.id, "heartbeat tick sent");
                    self.state.lock().await.insert(agent.id, now);
                }
                Err(e) => {
                    warn!(agent_id = %agent.id, error = %e, "could not deliver HeartbeatTick");
                }
            }
        }

        // Forget state for agents that disconnected.
        self.state
            .lock()
            .await
            .retain(|id, _| connected.contains(id));
    }
}

fn interval_of(agent: Option<&Agent>) -> Duration {
    agent
        .and_then(|a| a.config.get("heartbeat"))
        .and_then(|h| h.get("interval_seconds"))
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_INTERVAL, Duration::from_secs)
        .max(MIN_INTERVAL)
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
        let scheduler = HeartbeatScheduler::new(
            registry.clone(),
            db.clone(),
            Arc::new(Vec::new()),
            Arc::new(Channels::new()),
        );
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

        registry.register(AgentHandle::new(agent.id, 1)).await;
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

        registry.register(AgentHandle::new(agent.id, 1)).await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<GatewayToAgent>(8);
        registry.attach(agent.id, tx).await.expect("attach");

        let t0 = Instant::now();
        scheduler.tick_once(t0).await;
        assert!(rx.try_recv().is_err());

        // Simulate 61 seconds later — should fire.
        let t1 = t0 + Duration::from_secs(61);
        scheduler.tick_once(t1).await;
        let frame = rx.recv().await.expect("tick");
        assert!(matches!(frame, GatewayToAgent::HeartbeatTick(_)));
    }

    #[tokio::test]
    async fn agent_with_a_full_queue_does_not_block_other_heartbeats() {
        let (scheduler, registry, db, owner) = fixture().await;
        let mut queues = Vec::new();
        for name in ["stalled", "healthy"] {
            let agent = havn_db::repo::agents::create(
                &db,
                NewAgent {
                    owner_id: owner,
                    team_id: None,
                    name,
                    config: serde_json::json!({"heartbeat": {"interval_seconds": 60}}),
                },
            )
            .await
            .expect("agent");
            registry.register(AgentHandle::new(agent.id, 1)).await;
            let (tx, rx) = tokio::sync::mpsc::channel::<GatewayToAgent>(1);
            registry.attach(agent.id, tx).await.expect("attach");
            queues.push((agent.id, rx));
        }
        let (stalled, _stalled_rx) = &queues[0];
        registry
            .send(*stalled, GatewayToAgent::Shutdown)
            .await
            .expect("fill the stalled agent's queue");

        let t0 = Instant::now();
        scheduler.tick_once(t0).await;
        tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.tick_once(t0 + Duration::from_secs(61)),
        )
        .await
        .expect("a full queue must not stall the pass");

        let (_, healthy_rx) = &mut queues[1];
        assert!(matches!(
            healthy_rx.try_recv(),
            Ok(GatewayToAgent::HeartbeatTick(_))
        ));
    }

    #[tokio::test]
    async fn config_below_floor_is_clamped() {
        let (_, _, db, owner) = fixture().await;
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
        assert_eq!(interval_of(Some(&agent)), MIN_INTERVAL);
    }

    #[tokio::test]
    async fn missing_config_uses_default() {
        let (_, _, db, owner) = fixture().await;
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
        assert_eq!(interval_of(Some(&agent)), DEFAULT_INTERVAL);
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

        registry.register(AgentHandle::new(agent.id, 1)).await;
        let (tx, _rx) = tokio::sync::mpsc::channel::<GatewayToAgent>(8);
        registry.attach(agent.id, tx.clone()).await.expect("attach");

        scheduler.tick_once(Instant::now()).await;
        assert_eq!(scheduler.state.lock().await.len(), 1);

        registry.detach(agent.id, &tx).await;
        scheduler.tick_once(Instant::now()).await;
        assert!(scheduler.state.lock().await.is_empty());
    }

    async fn agent_with(
        db: &SqlitePool,
        owner: UserId,
        name: &str,
        config: serde_json::Value,
    ) -> Agent {
        havn_db::repo::agents::create(
            db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name,
                config,
            },
        )
        .await
        .expect("agent")
    }

    fn bound(agent: &str, channel: &str, account: &str) -> ChannelBindingConfig {
        ChannelBindingConfig {
            agent_id: agent.into(),
            channel: channel.into(),
            account_id: account.into(),
        }
    }

    #[tokio::test]
    async fn a_heartbeat_goes_where_its_agent_is_told_to_send_it() {
        let (_, _, db, owner) = fixture().await;
        sqlx::query(
            "INSERT INTO channel_links (channel, sender_id, user_id) VALUES ('telegram', '42', ?1)",
        )
        .bind(owner.to_string())
        .execute(&db)
        .await
        .expect("link");
        let last = agent_with(&db, owner, "last", serde_json::json!({})).await;
        let web_only = agent_with(
            &db,
            owner,
            "web",
            serde_json::json!({"heartbeat": {"deliver": "web"}}),
        )
        .await;
        let telegram = agent_with(
            &db,
            owner,
            "telegram",
            serde_json::json!({"heartbeat": {"deliver": "telegram"}}),
        )
        .await;
        let lark = agent_with(
            &db,
            owner,
            "lark",
            serde_json::json!({"heartbeat": {"deliver": "lark"}}),
        )
        .await;
        let mut bindings = Vec::new();
        for agent in [&last, &web_only, &telegram, &lark] {
            let id = agent.id.to_string();
            bindings.push(bound(&id, "telegram", &format!("bot-{id}")));
            bindings.push(bound(&id, "lark", &format!("app-{id}")));
        }
        // An account bound first to another agent is that agent's.
        bindings.insert(0, bound(&web_only.id.to_string(), "telegram", "shared"));
        bindings.push(bound(&last.id.to_string(), "telegram", "shared"));
        // An account that may not send to the owner is left out.
        bindings.push(bound(&last.id.to_string(), "telegram", "closed"));
        let account =
            |id: &str, dm_policy: crate::config::DmPolicy| crate::config::ChannelAccount {
                id: id.into(),
                adapter_token_ref: "secret:channel:telegram:bot".into(),
                dm_policy,
                allow_from: Vec::new(),
                admins: Vec::new(),
                bot_username: None,
            };
        let mut channels = Channels::new();
        for channel in ["telegram", "lark"] {
            let mut accounts: Vec<_> = bindings
                .iter()
                .filter(|b| b.channel == channel && b.account_id != "closed")
                .map(|b| account(&b.account_id, crate::config::DmPolicy::Open))
                .collect();
            if channel == "telegram" {
                accounts.push(account("closed", crate::config::DmPolicy::Allowlist));
            }
            channels.insert(channel.to_string(), ChannelEntry { accounts });
        }

        let web =
            |agent: &Agent| crate::webchat::session_for(&owner.to_string(), agent.id).to_string();
        let dm = |agent: &Agent| format!("ch|telegram|bot-{}|42", agent.id);

        let tick = tick_for(&db, &bindings, &channels, &last).await;
        assert_eq!(tick.web, web(&last));
        assert_eq!(
            tick.to,
            vec![web(&last), dm(&last)],
            "the web chat and the linked account"
        );

        let tick = tick_for(&db, &bindings, &channels, &web_only).await;
        assert_eq!(
            tick,
            HeartbeatTick {
                web: web(&web_only),
                to: Vec::new()
            }
        );

        let tick = tick_for(&db, &bindings, &channels, &telegram).await;
        assert_eq!(tick.to, vec![dm(&telegram)]);

        let tick = tick_for(&db, &bindings, &channels, &lark).await;
        assert!(
            tick.to.is_empty(),
            "no account linked on that channel: the web chat"
        );
    }
}
