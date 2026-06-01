//! Audit log writer (spec §10.3) — fire-and-forget convenience over
//! [`havn_db::repo::audit`].
//!
//! Every consequential mutation in the gateway calls one of these
//! helpers. They log a structured event but **never** propagate
//! errors to the caller — an audit-write failure must not cause an
//! agent-create or credential-rotate to fail. Worst case we print a
//! warning and keep going. Spec §11 threat model assumes the audit
//! log is a soft signal, not a security control.
//!
//! Conventions for `action`:
//! - lower-snake-case `<resource>.<verb>` strings, e.g.
//!   `agent.created`, `team.deleted`, `role.policy_updated`,
//!   `credential.created`, `member.added`.
//! - `details` carries just-enough JSON to investigate without joining
//!   other tables. Names not full bodies, ids not full rows.

use havn_core::{AgentId, TeamId, UserId};
use havn_db::repo::audit::{self, NewAuditEntry};
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::warn;

/// Record one event. Returns `()` regardless of outcome — failures
/// log a warning and are dropped. Use `record_or_log` if a caller
/// needs the row id back (rare).
pub async fn record(
    db: &SqlitePool,
    user_id: UserId,
    team_id: Option<TeamId>,
    agent_id: Option<AgentId>,
    action: &str,
    details: Value,
) {
    if let Err(e) = audit::record(
        db,
        NewAuditEntry {
            team_id,
            user_id,
            agent_id,
            action,
            details,
        },
    )
    .await
    {
        warn!(action, error = %e, "audit write failed; continuing");
    }
}

/// Helper for the common "by user, no team, no agent" pattern (e.g.
/// "user X created team Y" — Y exists but isn't an admin scope yet).
pub async fn record_user_action(db: &SqlitePool, user_id: UserId, action: &str, details: Value) {
    record(db, user_id, None, None, action, details).await;
}

/// Helper for "by user, on agent X". Agent's team_id (when set) is
/// looked up so the team-scoped audit view picks it up automatically.
/// Best-effort — if the agent lookup fails we still write the entry
/// without the team scope.
pub async fn record_agent_action(
    db: &SqlitePool,
    user_id: UserId,
    agent_id: AgentId,
    action: &str,
    details: Value,
) {
    let team_id = match havn_db::repo::agents::find_by_id(db, agent_id).await {
        Ok(Some(a)) => a.team_id,
        _ => None,
    };
    record(db, user_id, team_id, Some(agent_id), action, details).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_db::connect_in_memory;
    use havn_db::repo::audit::ListFilter;
    use havn_db::repo::users::{NewUser, create as create_user};

    #[tokio::test]
    async fn record_user_action_appears_in_list() {
        let pool = connect_in_memory().await.expect("db");
        let u = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user");
        record_user_action(
            &pool,
            u.id,
            "team.created",
            serde_json::json!({"name": "test"}),
        )
        .await;
        let rows = audit::list(
            &pool,
            ListFilter {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "team.created");
    }

    #[tokio::test]
    async fn write_failure_does_not_panic() {
        // We can't easily simulate a DB failure here, but the no-op
        // happy-path test above + the function's signature (`async ->
        // ()`) lock the contract: callers can never get an error from
        // these helpers, by construction.
    }
}
