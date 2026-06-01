//! Daily background pass that ages out expired memory rows (spec §9.4).
//!
//! From the user's seat: facts the agent learned about you 90 days ago
//! that you haven't reinforced shouldn't keep dominating the agent's
//! "recent events" view. The aging pass marks those rows `archived_at`
//! so they drop out of context build, but never DELETEs (the dashboard
//! still shows them as audit trail). Identity and preference rows have
//! no TTL and are immune.
//!
//! Cadence: every 24 hours (with an early first run 5 minutes after
//! startup so a freshly-reopened workspace gets housekept right away
//! rather than waiting a full day).

use std::time::Duration;

use sqlx::SqlitePool;
use tokio::time::{MissedTickBehavior, sleep};
use tracing::{debug, info, warn};

use havn_db::agent::memory;

/// Tick cadence for the aging pass.
pub const TICK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Initial delay so the first pass runs shortly after startup, not 24h
/// later. Keeps freshly-reopened workspaces tidy.
pub const FIRST_TICK_DELAY: Duration = Duration::from_secs(5 * 60);

/// Run forever. Spawn this as a tokio task in the runtime's startup
/// alongside the agent socket / writer / reader loops.
pub async fn run(pool: SqlitePool) {
    info!(
        first_run_in_s = FIRST_TICK_DELAY.as_secs(),
        cadence_s = TICK_INTERVAL.as_secs(),
        "memory aging pass scheduled"
    );
    sleep(FIRST_TICK_DELAY).await;
    tick_once(&pool).await;

    let mut interval = tokio::time::interval(TICK_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval.tick().await; // skip the immediate fire (already ticked above).
    loop {
        interval.tick().await;
        tick_once(&pool).await;
    }
}

async fn tick_once(pool: &SqlitePool) {
    match memory::age_expired(pool).await {
        Ok(0) => debug!("memory aging: nothing to archive"),
        Ok(n) => info!(archived = n, "memory aging: archived expired rows"),
        Err(e) => warn!(error = %e, "memory aging pass failed"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_db::agent::connect_in_memory;
    use havn_db::agent::memory::{Kind, NewEntry, Source};

    #[tokio::test]
    async fn tick_once_archives_expired() {
        let pool = connect_in_memory().await.expect("agent db");
        memory::remember(
            &pool,
            NewEntry {
                key: "e_old",
                value: "old",
                kind: Kind::Event,
                source: Source::AgentInferred,
                ttl_days: Some(1),
            },
        )
        .await
        .expect("remember");
        // Backdate so it's already past TTL.
        sqlx::query(
            "UPDATE memory \
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-2 days') \
             WHERE key = ?1",
        )
        .bind("e_old")
        .execute(&pool)
        .await
        .expect("backdate");

        tick_once(&pool).await;

        // memory::get filters archived_at IS NULL, so a successful
        // archive yields None here — verify by direct count instead.
        assert!(
            memory::get(&pool, "e_old").await.expect("get").is_none(),
            "row should no longer surface as active after aging"
        );
        let archived: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory WHERE archived_at IS NOT NULL AND key LIKE 'e_old%'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(archived, 1, "tick should have archived the expired event");
    }
}
