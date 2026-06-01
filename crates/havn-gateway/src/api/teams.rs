//! `/teams` and `/teams/{id}` endpoints (spec §8.3, §10.3).
//!
//! Teams group users for shared agents, shared credentials, and a
//! single audit log. Creation is open to any authenticated user — the
//! creator is auto-promoted to the team's `admin` role so they can
//! invite others. Renames and deletion are admin-only.
//!
//! Sub-resources live in sibling modules:
//! - `/teams/{id}/members` → [`super::members`]
//! - `/teams/{id}/roles`   → [`super::roles`]
//! - `/teams/{id}/agents`  → [`Self::list_agents`] below
//! - `/teams/{id}/audit-log`   → [`super::audit`]
//! - `/teams/{id}/usage`       → [`Self::usage`] below
//! - `/teams/{id}/credentials` → [`super::team_credentials`]
//!
//! All admin-mutating endpoints write a `team.*` audit entry (best-effort).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use havn_core::{Policy, TeamId};
use havn_db::repo::credential_usage::TeamUserUsage;
use havn_db::repo::roles::{NewRole, create as create_role};
use havn_db::repo::team_memberships;
use havn_db::repo::teams::{self as repo, NewTeam};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::perm;

#[derive(Debug, Serialize)]
pub struct TeamView {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// Convenience flag — true when the calling user holds the team's
    /// `admin` role. Lets the dashboard render mgmt buttons without
    /// a second request per row.
    pub is_admin: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchTeamRequest {
    pub name: Option<String>,
}

/// `GET /teams` — teams the calling user belongs to. Single-user
/// operators see an empty list until they create one.
pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<Vec<TeamView>>, ApiError> {
    let memberships = team_memberships::list_for_user(&state.db, user.id).await?;
    let mut views = Vec::with_capacity(memberships.len());
    for m in memberships {
        let team = repo::find_by_id(&state.db, m.team_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        views.push(TeamView {
            id: team.id.to_string(),
            name: team.name,
            created_at: team.created_at,
            is_admin: m.role_name == "admin",
        });
    }
    Ok(Json(views))
}

/// `POST /teams` — create a team and auto-add the creator as admin.
/// Atomic enough: if the role create or membership add fails after
/// the team is inserted, we leave a half-created team behind. The CLI
/// `havn team delete` cleans up; in practice a failure here is rare
/// (tests exercise the full path) and the dashboard surfaces the
/// 5xx so the operator notices.
pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<CreateTeamRequest>,
) -> Result<(StatusCode, Json<TeamView>), ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must be non-empty".into()));
    }
    let team = repo::create(
        &state.db,
        NewTeam {
            name: req.name.trim(),
        },
    )
    .await?;
    info!(team_id = %team.id, name = %team.name, owner = %user.id, "team created");

    // Seed the team's own admin + member roles so admins can edit
    // policies without creating roles by hand.
    let admin_role = create_role(
        &state.db,
        NewRole {
            team_id: Some(team.id),
            name: "admin",
            policy: &admin_default_policy(),
        },
    )
    .await?;
    create_role(
        &state.db,
        NewRole {
            team_id: Some(team.id),
            name: "member",
            policy: &member_default_policy(),
        },
    )
    .await?;

    // Auto-add creator as admin.
    team_memberships::add(&state.db, user.id, team.id, admin_role.id).await?;

    audit::record(
        &state.db,
        user.id,
        Some(team.id),
        None,
        "team.created",
        serde_json::json!({"name": team.name}),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(TeamView {
            id: team.id.to_string(),
            name: team.name,
            created_at: team.created_at,
            is_admin: true,
        }),
    ))
}

/// `GET /teams/{id}` — full view (members must be in the team).
pub async fn get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<TeamView>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_member(&state.db, user.id, id).await?;
    let team = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let is_admin = perm::is_admin(&state.db, user.id, id).await?;
    Ok(Json(TeamView {
        id: team.id.to_string(),
        name: team.name,
        created_at: team.created_at,
        is_admin,
    }))
}

/// `PATCH /teams/{id}` — admin-only rename.
pub async fn patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<PatchTeamRequest>,
) -> Result<Json<TeamView>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_admin(&state.db, user.id, id).await?;
    if let Some(name) = req.name {
        if name.trim().is_empty() {
            return Err(ApiError::BadRequest("name must be non-empty".into()));
        }
        let renamed = repo::rename(&state.db, id, name.trim()).await?;
        audit::record(
            &state.db,
            user.id,
            Some(id),
            None,
            "team.renamed",
            serde_json::json!({"name": renamed.name}),
        )
        .await;
        let is_admin = perm::is_admin(&state.db, user.id, id).await?;
        return Ok(Json(TeamView {
            id: renamed.id.to_string(),
            name: renamed.name,
            created_at: renamed.created_at,
            is_admin,
        }));
    }
    // No-op patch — return current state.
    let team = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let is_admin = perm::is_admin(&state.db, user.id, id).await?;
    Ok(Json(TeamView {
        id: team.id.to_string(),
        name: team.name,
        created_at: team.created_at,
        is_admin,
    }))
}

/// `DELETE /teams/{id}` — admin-only. Cascades through roles and
/// memberships; agents owned by team members keep their team_id set
/// to NULL via the FK rule (not deleted — operators preserve work).
///
/// Audit shape: the deletion is recorded as a USER action (team_id
/// `None`), with the team's name + id snapshotted into details. The
/// team-scoped audit endpoint can't surface this entry (the team is
/// gone), but `/me/audit-log` shows it for the deleting admin with
/// full forensic context. Spec §11 treats audit as a soft signal,
/// so this trade-off is fine — the operator who needs to forensic
/// "who deleted team X" greps user audits, not team ones.
pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    perm::require_admin(&state.db, user.id, id).await?;

    // Snapshot the team name BEFORE the delete cascades.
    let snapshot_name = repo::find_by_id(&state.db, id)
        .await?
        .map_or_else(|| "(unknown)".into(), |t| t.name);

    repo::delete(&state.db, id).await?;
    audit::record(
        &state.db,
        user.id,
        None,
        None,
        "team.deleted",
        serde_json::json!({
            "team_id": id.to_string(),
            "team_name": snapshot_name,
        }),
    )
    .await;
    info!(team_id = %id, by = %user.id, "team deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---- /teams/{id}/agents ---------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TeamAgentView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub owner_id: String,
    pub owner_display_name: String,
    pub created_at: DateTime<Utc>,
}

/// `GET /teams/{id}/agents` — every agent rows with `team_id = id`.
/// Members can view; admins use this to bulk-stop runaway agents
/// (the bulk action is dashboard-side via per-agent stop calls;
/// the listing endpoint is members-only because seeing what's
/// running is non-sensitive).
pub async fn list_agents(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<TeamAgentView>>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_member(&state.db, user.id, id).await?;

    // Inline join — havn-db's agent repo doesn't have a "by team_id"
    // helper yet (every prior consumer keyed on owner). Hand-rolling
    // the SQL keeps the change local; promote to a repo function when
    // a third caller appears.
    use sqlx::Row as _;
    let rows = sqlx::query(
        "SELECT a.id, a.name, a.status, a.owner_id, u.display_name AS owner_display_name, \
                a.created_at \
         FROM agents a \
         JOIN users u ON u.id = a.owner_id \
         WHERE a.team_id = ?1 \
         ORDER BY a.created_at DESC",
    )
    .bind(id.to_string())
    .fetch_all(&state.db)
    .await
    .map_err(|e| ApiError::Internal(format!("query: {e}")))?;

    let views: Vec<TeamAgentView> = rows
        .into_iter()
        .filter_map(|r| {
            Some(TeamAgentView {
                id: r.try_get::<String, _>("id").ok()?,
                name: r.try_get::<String, _>("name").ok()?,
                status: r.try_get::<String, _>("status").ok()?,
                owner_id: r.try_get::<String, _>("owner_id").ok()?,
                owner_display_name: r.try_get::<String, _>("owner_display_name").ok()?,
                created_at: r.try_get::<DateTime<Utc>, _>("created_at").ok()?,
            })
        })
        .collect();
    Ok(Json(views))
}

// ---- /teams/{id}/usage ----------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// Lookback window in days. Defaults to 30. Caps at 365 to keep
    /// the rollup cheap.
    pub days: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TeamUsageEntry {
    pub user_id: String,
    pub display_name: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub call_count: i64,
}

#[derive(Debug, Serialize)]
pub struct TeamUsageResponse {
    pub team_id: String,
    pub since: DateTime<Utc>,
    pub entries: Vec<TeamUsageEntry>,
}

/// `GET /teams/{id}/usage?days=30` — token totals per member.
/// Spec §10.3 admin view. Members can read their own team's usage
/// even without admin role — visibility into shared spend is healthy.
pub async fn usage(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<TeamUsageResponse>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_member(&state.db, user.id, id).await?;

    let days = q.days.unwrap_or(30).min(365);
    let since = Utc::now() - ChronoDuration::days(days.into());
    let since_rfc = since.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let rows = havn_db::repo::credential_usage::list_team_usage(&state.db, id, &since_rfc).await?;
    Ok(Json(TeamUsageResponse {
        team_id: id.to_string(),
        since,
        entries: rows.into_iter().map(into_usage_entry).collect(),
    }))
}

fn into_usage_entry(u: TeamUserUsage) -> TeamUsageEntry {
    TeamUsageEntry {
        user_id: u.user_id.to_string(),
        display_name: u.display_name,
        tokens_in: u.tokens_in,
        tokens_out: u.tokens_out,
        call_count: u.call_count,
    }
}

// ---- helpers --------------------------------------------------------------

fn parse_id(s: &str) -> Result<TeamId, ApiError> {
    TeamId::from_str(s).map_err(|_| ApiError::BadRequest("invalid team id".into()))
}

/// Default policy for a freshly-created team's `admin` role. Mirrors
/// the system seed but lives independently in code so dashboard
/// admins can edit it without touching system roles.
fn admin_default_policy() -> Policy {
    let mut p = Policy {
        max_agents: 50,
        ..Policy::default()
    };
    p.permissions.can_use_shell = true;
    p.permissions.can_spawn_subagents = true;
    p.admin_visibility.can_view_audit_log = true;
    p
}

fn member_default_policy() -> Policy {
    let mut p = Policy {
        max_agents: 5,
        ..Policy::default()
    };
    p.permissions.can_use_shell = false;
    p.permissions.can_spawn_subagents = false;
    p.admin_visibility.can_view_audit_log = false;
    p
}
