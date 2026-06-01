//! `/teams/{id}/audit-log` and `/me/audit-log` — audit log read API
//! (spec §10.3).
//!
//! Read-only. Pagination is `before=<rfc3339>&limit=100`-style: pass
//! the oldest `created_at` you've seen as `before`, get the next page.
//! Default limit 100, max 500.
//!
//! Authorisation:
//! - Team scope: caller must be a team admin AND the team's `policy.
//!   admin_visibility.can_view_audit_log` must be true (spec §6.2
//!   default `true` for admins). Members without admin role get 403.
//! - User scope (`/me/audit-log`): the calling user can see actions
//!   they themselves performed. Always allowed.
//!
//! Spec §10.3 calls for "JSON via API; CSV is `jq` user-side". We
//! ship JSON only — `jq -r ...` from the command line covers the rare
//! CSV need without a code path that grows stale.

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use havn_core::{AgentId, TeamId, UserId};
use havn_db::repo::audit::{self as repo, AuditEntry, ListFilter};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;
use crate::perm;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub user_id: Option<String>,
    pub agent_id: Option<String>,
    pub action_prefix: Option<String>,
    pub before: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct AuditEntryView {
    pub id: String,
    pub team_id: Option<String>,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub action: String,
    pub details: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl From<AuditEntry> for AuditEntryView {
    fn from(e: AuditEntry) -> Self {
        Self {
            id: e.id,
            team_id: e.team_id.map(|t| t.to_string()),
            user_id: e.user_id.to_string(),
            agent_id: e.agent_id.map(|a| a.to_string()),
            action: e.action,
            details: e.details,
            created_at: e.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditListResponse {
    pub entries: Vec<AuditEntryView>,
    /// Next-page anchor: pass this back as `?before=<value>` to walk
    /// older entries. `None` when the page is the last one.
    pub next_before: Option<DateTime<Utc>>,
}

/// `GET /teams/{id}/audit-log` — admin-only AND the admin's role
/// policy must grant `admin_visibility.can_view_audit_log`. Spec §6.2
/// default is `true` for the seeded admin role; admins who narrow
/// their own role policy can also lock themselves out of this view
/// — surfaced as 403 with a clear message.
pub async fn list_for_team(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(team): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<AuditListResponse>, ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;

    // Look up the admin's per-team role and consult the policy flag.
    // The membership join is cheap and we already know `is_admin`
    // from the require_admin check above; we just need the policy.
    let membership = havn_db::repo::team_memberships::find(&state.db, user.id, team)
        .await?
        .ok_or(ApiError::NotFound)?;
    let role = havn_db::repo::roles::find_by_id(&state.db, membership.role_id)
        .await?
        .ok_or(ApiError::Internal("admin role row missing".into()))?;
    if !role.policy.admin_visibility.can_view_audit_log {
        return Err(ApiError::Forbidden(
            "your role's policy disables admin_visibility.can_view_audit_log".into(),
        ));
    }

    let filter = build_filter(Some(team), &q)?;
    let entries = repo::list(&state.db, filter).await?;
    Ok(Json(into_response(entries)))
}

/// `GET /me/audit-log` — caller's own actions only.
pub async fn list_for_self(
    State(state): State<AppState>,
    user: AuthedUser,
    Query(mut q): Query<AuditQuery>,
) -> Result<Json<AuditListResponse>, ApiError> {
    // Force the user_id filter to `self` regardless of what the query
    // string says — defence against accidentally exposing another
    // user's actions through this endpoint.
    q.user_id = Some(user.id.to_string());
    let filter = build_filter(None, &q)?;
    let entries = repo::list(&state.db, filter).await?;
    Ok(Json(into_response(entries)))
}

fn build_filter(team_id: Option<TeamId>, q: &AuditQuery) -> Result<ListFilter, ApiError> {
    let user_id = q
        .user_id
        .as_deref()
        .map(UserId::from_str)
        .transpose()
        .map_err(|_| ApiError::BadRequest("invalid user_id filter".into()))?;
    let agent_id = q
        .agent_id
        .as_deref()
        .map(AgentId::from_str)
        .transpose()
        .map_err(|_| ApiError::BadRequest("invalid agent_id filter".into()))?;
    Ok(ListFilter {
        team_id,
        user_id,
        agent_id,
        action_prefix: q.action_prefix.clone(),
        before: q.before,
        limit: q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT),
    })
}

fn into_response(entries: Vec<AuditEntry>) -> AuditListResponse {
    let next_before = entries.last().map(|e| e.created_at);
    AuditListResponse {
        entries: entries.into_iter().map(AuditEntryView::from).collect(),
        next_before,
    }
}

fn parse_team(s: &str) -> Result<TeamId, ApiError> {
    TeamId::from_str(s).map_err(|_| ApiError::BadRequest("invalid team id".into()))
}
