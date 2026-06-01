//! Audit log repository (spec §5.1, §10.3).
//!
//! Append-only structured log of every consequential mutation in the
//! gateway. Spec §10.3 requires a "searchable, filterable" admin view;
//! the repo gives us the storage + a typed list API. The gateway-side
//! `audit::record` helper is fire-and-forget — a logging failure
//! must never block the underlying mutation.
//!
//! Read paths support filtering by team, user, agent, and time window.
//! Pagination is `(limit, before)`-style: pass the oldest `created_at`
//! you've seen as `before` and `limit` rows older than that come back.
//! Cheaper than offsets for an append-only timeline.

use chrono::{DateTime, Utc};
use havn_core::{AgentId, TeamId, UserId};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: String,
    pub team_id: Option<TeamId>,
    pub user_id: UserId,
    pub agent_id: Option<AgentId>,
    /// Stable verb string, e.g. `"agent.created"`, `"role.policy_updated"`.
    /// Conventionally `<resource>.<action>`. The gateway and CLI write
    /// these by hand; the dashboard groups by it.
    pub action: String,
    /// Free-form JSON blob carrying just-enough context to investigate
    /// the action without joining other tables. Keep it small — names
    /// not full bodies, ids not full rows.
    pub details: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewAuditEntry<'a> {
    pub team_id: Option<TeamId>,
    pub user_id: UserId,
    pub agent_id: Option<AgentId>,
    pub action: &'a str,
    pub details: serde_json::Value,
}

/// Insert one entry. Returns the persisted row (with its DB-assigned
/// `created_at`) so the caller can inline the row id into a response
/// if they want.
pub async fn record(pool: &SqlitePool, new: NewAuditEntry<'_>) -> Result<AuditEntry> {
    let id = Uuid::now_v7().to_string();
    let details_json = serde_json::to_string(&new.details).map_err(|e| DbError::InvalidValue {
        column: "audit_log.details",
        message: e.to_string(),
    })?;
    sqlx::query(
        "INSERT INTO audit_log (id, team_id, user_id, agent_id, action, details) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&id)
    .bind(new.team_id.map(|t| t.to_string()))
    .bind(new.user_id.to_string())
    .bind(new.agent_id.map(|a| a.to_string()))
    .bind(new.action)
    .bind(details_json)
    .execute(pool)
    .await?;
    fetch_one(pool, &id).await
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub team_id: Option<TeamId>,
    pub user_id: Option<UserId>,
    pub agent_id: Option<AgentId>,
    pub action_prefix: Option<String>,
    /// "Older than" anchor for pagination — pass the oldest `created_at`
    /// seen so far, get the next page back.
    pub before: Option<DateTime<Utc>>,
    /// Cap on rows returned. Bound your dashboard pagination off this.
    pub limit: u32,
}

/// Paginated descending-time list. Filters compose with AND.
/// `limit = 0` returns an empty list (caller error); the dashboard
/// passes 100 typically.
pub async fn list(pool: &SqlitePool, filter: ListFilter) -> Result<Vec<AuditEntry>> {
    if filter.limit == 0 {
        return Ok(Vec::new());
    }
    // Build WHERE / args dynamically. The number of conditions is tiny
    // and the columns are indexed (idx_audit_*); SQLite picks the right
    // index based on which filter binds non-NULL.
    let mut sql = String::from(
        "SELECT id, team_id, user_id, agent_id, action, details, created_at \
         FROM audit_log WHERE 1 = 1",
    );
    let mut binds: Vec<String> = Vec::new();
    if let Some(v) = filter.team_id {
        sql.push_str(" AND team_id = ?");
        binds.push(v.to_string());
    }
    if let Some(v) = filter.user_id {
        sql.push_str(" AND user_id = ?");
        binds.push(v.to_string());
    }
    if let Some(v) = filter.agent_id {
        sql.push_str(" AND agent_id = ?");
        binds.push(v.to_string());
    }
    if let Some(prefix) = &filter.action_prefix {
        sql.push_str(" AND action LIKE ?");
        binds.push(format!("{prefix}%"));
    }
    if let Some(before) = filter.before {
        // SQLite's `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')` produces
        // millisecond-precision timestamps. Match the same width when
        // we serialise a DateTime back for comparison so lexicographic
        // ordering across mixed-precision strings doesn't bite us.
        sql.push_str(" AND created_at < ?");
        binds.push(before.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    let mut q = sqlx::query_as::<_, EntryRow>(&sql);
    for v in &binds {
        q = q.bind(v);
    }
    q = q.bind(filter.limit);
    let rows: Vec<EntryRow> = q.fetch_all(pool).await?;
    rows.into_iter().map(AuditEntry::try_from).collect()
}

async fn fetch_one(pool: &SqlitePool, id: &str) -> Result<AuditEntry> {
    let row: EntryRow = sqlx::query_as::<_, EntryRow>(
        "SELECT id, team_id, user_id, agent_id, action, details, created_at \
         FROM audit_log WHERE id = ?1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    AuditEntry::try_from(row)
}

#[derive(Debug, sqlx::FromRow)]
struct EntryRow {
    id: String,
    team_id: Option<String>,
    user_id: String,
    agent_id: Option<String>,
    action: String,
    details: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<EntryRow> for AuditEntry {
    type Error = DbError;
    fn try_from(r: EntryRow) -> Result<Self> {
        let details = serde_json::from_str(&r.details).map_err(|e| DbError::InvalidValue {
            column: "audit_log.details",
            message: e.to_string(),
        })?;
        Ok(Self {
            id: r.id,
            team_id: r
                .team_id
                .as_deref()
                .map(|s| parse_db_uuid(s, "audit_log.team_id").map(TeamId::from_uuid))
                .transpose()?,
            user_id: UserId::from_uuid(parse_db_uuid(&r.user_id, "audit_log.user_id")?),
            agent_id: r
                .agent_id
                .as_deref()
                .map(|s| parse_db_uuid(s, "audit_log.agent_id").map(AgentId::from_uuid))
                .transpose()?,
            action: r.action,
            details,
            created_at: r.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::users::{NewUser, create as create_user};

    async fn user_id(pool: &SqlitePool) -> UserId {
        create_user(pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id
    }

    #[tokio::test]
    async fn record_then_list_shows_entry() {
        let pool = connect_in_memory().await.expect("db");
        let u = user_id(&pool).await;
        record(
            &pool,
            NewAuditEntry {
                team_id: None,
                user_id: u,
                agent_id: None,
                action: "agent.created",
                details: serde_json::json!({"name": "alpha"}),
            },
        )
        .await
        .expect("record");
        let rows = list(
            &pool,
            ListFilter {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "agent.created");
        assert_eq!(rows[0].details["name"], "alpha");
    }

    #[tokio::test]
    async fn filters_compose() {
        let pool = connect_in_memory().await.expect("db");
        let alice = user_id(&pool).await;
        let bob = user_id(&pool).await;
        for (who, action) in [
            (alice, "agent.created"),
            (alice, "agent.deleted"),
            (bob, "agent.created"),
        ] {
            record(
                &pool,
                NewAuditEntry {
                    team_id: None,
                    user_id: who,
                    agent_id: None,
                    action,
                    details: serde_json::json!({}),
                },
            )
            .await
            .expect("record");
        }
        let alice_only = list(
            &pool,
            ListFilter {
                user_id: Some(alice),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(alice_only.len(), 2);
        assert!(alice_only.iter().all(|r| r.user_id == alice));

        let creates = list(
            &pool,
            ListFilter {
                action_prefix: Some("agent.created".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(creates.len(), 2);
    }

    #[tokio::test]
    async fn pagination_with_before() {
        let pool = connect_in_memory().await.expect("db");
        let u = user_id(&pool).await;
        for i in 0..5 {
            record(
                &pool,
                NewAuditEntry {
                    team_id: None,
                    user_id: u,
                    agent_id: None,
                    action: "x.y",
                    details: serde_json::json!({"i": i}),
                },
            )
            .await
            .expect("record");
            // Different created_at on each row — SQLite resolves to ms.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let first = list(
            &pool,
            ListFilter {
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(first.len(), 2);
        let next = list(
            &pool,
            ListFilter {
                limit: 2,
                before: Some(first.last().unwrap().created_at),
                ..Default::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(next.len(), 2);
        // No overlap between pages.
        assert!(next.iter().all(|r| !first.iter().any(|f| f.id == r.id)));
    }
}
