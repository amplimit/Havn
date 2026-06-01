//! Curator scheduler — periodic, idle-aware trigger for [`crate::curator::run_pass`].
//!
//! Cadence (spec §9.5): every 7 days, conditional on the agent being
//! idle ≥ 2 hours. Concretely, the scheduler:
//!
//! 1. Wakes every [`TICK_INTERVAL`] (24 h) — short enough to catch the
//!    7-day deadline within a day, long enough that the scheduler
//!    itself is invisible to operators.
//! 2. Reads the last-run timestamp from `workspace/.curator/state.json`.
//!    If `< 7d` ago, sleep.
//! 3. Reads the last-inbound-message timestamp from the same state file.
//!    If `< 2h` ago, sleep — the user is probably mid-conversation;
//!    don't disrupt prefix-cache hot paths.
//! 4. Otherwise, fires [`crate::curator::run_pass`] and writes a fresh
//!    state file.
//!
//! `record_inbound` is called from the agent's reader loop so the
//! scheduler always has an up-to-date "last message at" anchor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::time::{MissedTickBehavior, sleep};
use tracing::{debug, info, warn};

use crate::curator::{self, Bridge};

pub const TICK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub const FIRST_TICK_DELAY: Duration = Duration::from_secs(10 * 60);
pub const MIN_DAYS_BETWEEN_PASSES: i64 = 7;
pub const MIN_IDLE_HOURS: i64 = 2;

/// Live mirror of "when did this agent last receive an inbound message".
/// The reader loop bumps this; the scheduler reads it on each tick.
/// Wrapped in `Arc<RwLock>` so the reader path stays cheap (one write
/// per inbound message; scheduler reads once per 24 h).
pub type LastInbound = Arc<RwLock<DateTime<Utc>>>;

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    last_ran_at: Option<DateTime<Utc>>,
}

pub async fn run(bridge: Bridge, last_inbound: LastInbound) {
    info!(
        first_run_in_s = FIRST_TICK_DELAY.as_secs(),
        cadence_s = TICK_INTERVAL.as_secs(),
        min_days_between = MIN_DAYS_BETWEEN_PASSES,
        min_idle_h = MIN_IDLE_HOURS,
        "curator scheduler started"
    );
    sleep(FIRST_TICK_DELAY).await;
    tick(&bridge, &last_inbound).await;

    let mut interval = tokio::time::interval(TICK_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        interval.tick().await;
        tick(&bridge, &last_inbound).await;
    }
}

async fn tick(bridge: &Bridge, last_inbound: &LastInbound) {
    let workspace = &*bridge.workspace_dir;
    let state = read_state(workspace).await;

    let now = Utc::now();
    if let Some(last_ran) = state.last_ran_at {
        let days_since = (now - last_ran).num_days();
        if days_since < MIN_DAYS_BETWEEN_PASSES {
            debug!(days_since, "curator: too soon since last pass — skipping");
            return;
        }
    }
    let last_msg = *last_inbound.read().await;
    let hours_idle = (now - last_msg).num_hours();
    if hours_idle < MIN_IDLE_HOURS {
        debug!(hours_idle, "curator: agent not idle long enough — skipping");
        return;
    }

    info!("curator: idle threshold met, running pass");
    match curator::run_pass(bridge).await {
        Ok(report) => {
            let updated = PersistedState {
                last_ran_at: Some(report.ran_at),
            };
            if let Err(e) = write_state(workspace, &updated).await {
                warn!(error = %e, "writing curator state.json failed (next pass will run too eagerly)");
            }
        }
        Err(e) => {
            warn!(error = %e, "curator pass failed");
        }
    }
}

async fn read_state(workspace: &Path) -> PersistedState {
    let path = workspace.join(".curator").join("state.json");
    match tokio::fs::read_to_string(&path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            warn!(error = %e, "curator state.json malformed; treating as fresh");
            PersistedState::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
        Err(e) => {
            warn!(error = %e, "reading curator state.json failed; treating as fresh");
            PersistedState::default()
        }
    }
}

async fn write_state(workspace: &Path, state: &PersistedState) -> std::io::Result<()> {
    let dir = workspace.join(".curator");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join("state.json");
    let body =
        serde_json::to_string_pretty(state).map_err(|e| std::io::Error::other(e.to_string()))?;
    tokio::fs::write(&path, body).await
}

/// Constructor for the scheduler's shared state. Initialised to "now"
/// so the first tick's idle check has a reasonable anchor (the agent
/// just started — treat that as activity).
pub fn new_last_inbound() -> LastInbound {
    Arc::new(RwLock::new(Utc::now()))
}

/// Bump `last_inbound` to now. Call from the reader loop on every
/// `InboundMessage` frame.
pub async fn record_inbound(handle: &LastInbound) {
    *handle.write().await = Utc::now();
}

#[allow(dead_code, reason = "exported for tests / future refactors")]
pub fn workspace_path(workspace: &Path) -> PathBuf {
    workspace.join(".curator").join("state.json")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn read_state_returns_default_when_missing() {
        let dir = std::env::temp_dir().join(format!("havn-cur-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let s = read_state(&dir).await;
        assert!(s.last_ran_at.is_none());
    }

    #[tokio::test]
    async fn round_trip_persisted_state() {
        let dir = std::env::temp_dir().join(format!("havn-cur-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let s = PersistedState {
            last_ran_at: Some(Utc::now()),
        };
        write_state(&dir, &s).await.expect("write");
        let read = read_state(&dir).await;
        assert!(read.last_ran_at.is_some());
    }

    #[tokio::test]
    async fn record_inbound_advances_clock() {
        let handle = new_last_inbound();
        let before = *handle.read().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        record_inbound(&handle).await;
        let after = *handle.read().await;
        assert!(after > before);
    }
}
