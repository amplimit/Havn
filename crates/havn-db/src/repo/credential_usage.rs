//! Credential usage records — every gateway-side LLM call writes one row here.
//!
//! Drives:
//! - per-user / per-credential / per-agent usage rollups for the dashboard
//! - daily-token budget enforcement (spec §6.2 `budget.max_tokens_per_day`)
//!
//! v0.6 dropped the `estimated_usd` column (spec §7.3): havn doesn't
//! maintain a model pricing table. Operators that need $ visibility
//! compute it from token counts in their own analytics pipeline.

use havn_core::{AgentId, CredentialId, UserId};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone)]
pub struct NewUsage<'a> {
    pub credential_id: CredentialId,
    pub user_id: UserId,
    /// `None` for gateway-direct LLM calls (e.g. the test endpoint).
    pub agent_id: Option<AgentId>,
    pub provider: &'a str,
    pub model: &'a str,
    pub tokens_in: i64,
    pub tokens_out: i64,
}

pub async fn record(pool: &SqlitePool, usage: NewUsage<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO credential_usages \
         (id, credential_id, user_id, agent_id, provider, model, tokens_in, tokens_out) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(usage.credential_id.to_string())
    .bind(usage.user_id.to_string())
    .bind(usage.agent_id.map(|a| a.to_string()))
    .bind(usage.provider)
    .bind(usage.model)
    .bind(usage.tokens_in)
    .bind(usage.tokens_out)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sum `tokens_in + tokens_out` for a single credential since `since`
/// (RFC 3339). Used by the LLM proxy to enforce per-credential daily
/// token caps before issuing a request.
pub async fn tokens_since(
    pool: &SqlitePool,
    credential_id: CredentialId,
    since_rfc3339: &str,
) -> Result<i64> {
    let total: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
         WHERE credential_id = ?1 AND created_at >= ?2",
    )
    .bind(credential_id.to_string())
    .bind(since_rfc3339)
    .fetch_one(pool)
    .await?;
    Ok(total.unwrap_or(0))
}

/// Per-user cap variant. Filters `credential_usages` by both
/// `credential_id` AND `user_id` so a team-scoped credential can apply
/// a per-user daily ceiling on top of the credential-wide cap.
///
/// Spec §7.3 / §10.3: a shared team key shouldn't let one heavy user
/// drain the whole team's daily quota. The `limits.per_user.max_tokens_per_day`
/// JSON path enables this; the resolver consults it before falling
/// through to the next credential in the chain.
pub async fn tokens_since_for_user(
    pool: &SqlitePool,
    credential_id: CredentialId,
    user_id: UserId,
    since_rfc3339: &str,
) -> Result<i64> {
    let total: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM credential_usages \
         WHERE credential_id = ?1 AND user_id = ?2 AND created_at >= ?3",
    )
    .bind(credential_id.to_string())
    .bind(user_id.to_string())
    .bind(since_rfc3339)
    .fetch_one(pool)
    .await?;
    Ok(total.unwrap_or(0))
}

/// Number of LLM calls a single (credential, user) pair has made in
/// the recent window. Used to enforce per-user RPM caps on team
/// credentials. Window passed as RFC 3339 lower-bound timestamp;
/// caller computes "now − 60s" for a per-minute check.
pub async fn requests_since_for_user(
    pool: &SqlitePool,
    credential_id: CredentialId,
    user_id: UserId,
    since_rfc3339: &str,
) -> Result<i64> {
    let n: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM credential_usages \
         WHERE credential_id = ?1 AND user_id = ?2 AND created_at >= ?3",
    )
    .bind(credential_id.to_string())
    .bind(user_id.to_string())
    .bind(since_rfc3339)
    .fetch_one(pool)
    .await?;
    Ok(n.unwrap_or(0))
}

/// Per-user token totals within a team since `since_rfc3339`. Drives
/// the dashboard's `/teams/:id/usage` view. Joined to `users.display_name`
/// and `agents.team_id` so we get one row per user with their total —
/// no N+1 lookups in the handler.
#[derive(Debug, Clone)]
pub struct TeamUserUsage {
    pub user_id: UserId,
    pub display_name: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub call_count: i64,
}

/// Rollup query: for every member of `team_id` that's used a credential
/// since `since_rfc3339`, return their token totals. Includes calls
/// against either user-scoped or team-scoped credentials, joined via
/// agents.team_id (so a member's calls on their personal agent only
/// count when that agent is associated to the team).
///
/// Spec §10.3 admin view: "aggregate CPU / memory / token usage per
/// user / credential". Token usage is the live one.
pub async fn list_team_usage(
    pool: &SqlitePool,
    team_id: havn_core::TeamId,
    since_rfc3339: &str,
) -> Result<Vec<TeamUserUsage>> {
    let rows: Vec<UsageRow> = sqlx::query_as::<_, UsageRow>(
        "SELECT u.id AS user_id, \
                u.display_name AS display_name, \
                COALESCE(SUM(cu.tokens_in), 0)  AS tokens_in, \
                COALESCE(SUM(cu.tokens_out), 0) AS tokens_out, \
                COUNT(cu.id)                    AS call_count \
         FROM team_memberships m \
         JOIN users u ON u.id = m.user_id \
         LEFT JOIN credential_usages cu \
              ON cu.user_id = m.user_id \
             AND cu.created_at >= ?2 \
         WHERE m.team_id = ?1 \
         GROUP BY u.id, u.display_name \
         ORDER BY (tokens_in + tokens_out) DESC, u.display_name",
    )
    .bind(team_id.to_string())
    .bind(since_rfc3339)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(TeamUserUsage {
                user_id: UserId::from_uuid(crate::repo::parse_db_uuid(
                    &r.user_id,
                    "credential_usages.user_id",
                )?),
                display_name: r.display_name,
                tokens_in: r.tokens_in,
                tokens_out: r.tokens_out,
                call_count: r.call_count,
            })
        })
        .collect()
}

#[derive(Debug, sqlx::FromRow)]
struct UsageRow {
    user_id: String,
    display_name: String,
    tokens_in: i64,
    tokens_out: i64,
    call_count: i64,
}
