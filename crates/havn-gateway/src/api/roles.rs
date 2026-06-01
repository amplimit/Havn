//! `/teams/{id}/roles` — per-team role management (spec §6.4, §10.3).
//!
//! Roles are name + [`Policy`] JSON. Each freshly-created team gets
//! `admin` and `member` seeded automatically (see `api::teams::create`),
//! so admins can immediately set policies on either of those — or
//! create custom roles for narrower personas (e.g. "auditor",
//! "ops-readonly").
//!
//! All endpoints are admin-only. Policy bodies are validated by
//! deserialising into [`havn_core::Policy`] before write — a malformed
//! policy yields 400 with the serde error rather than landing a row
//! that future loads will reject.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_core::{Policy, RoleId, TeamId};
use havn_db::repo::roles::{self as repo, NewRole, Role};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::perm;

#[derive(Debug, Serialize)]
pub struct RoleView {
    pub id: String,
    pub team_id: Option<String>,
    pub name: String,
    pub policy: Policy,
    pub created_at: DateTime<Utc>,
}

impl From<Role> for RoleView {
    fn from(r: Role) -> Self {
        Self {
            id: r.id.to_string(),
            team_id: r.team_id.map(|t| t.to_string()),
            name: r.name,
            policy: r.policy,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    pub name: String,
    #[serde(default)]
    pub policy: Option<Policy>,
}

#[derive(Debug, Deserialize)]
pub struct PatchRoleRequest {
    pub policy: Policy,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(team): Path<String>,
) -> Result<Json<Vec<RoleView>>, ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;
    let rows = repo::list_for_team(&state.db, team).await?;
    Ok(Json(rows.into_iter().map(RoleView::from).collect()))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(team): Path<String>,
    Json(req): Json<CreateRoleRequest>,
) -> Result<(StatusCode, Json<RoleView>), ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must be non-empty".into()));
    }
    // Default to a *narrow* policy — admins explicitly broaden as needed.
    // Less surprising than copying the `admin` baseline.
    let policy = req.policy.unwrap_or_default();

    let role = repo::create(
        &state.db,
        NewRole {
            team_id: Some(team),
            name: req.name.trim(),
            policy: &policy,
        },
    )
    .await?;
    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "role.created",
        serde_json::json!({"name": role.name, "role_id": role.id.to_string()}),
    )
    .await;
    info!(team = %team, role_id = %role.id, name = %role.name, "role created");
    Ok((StatusCode::CREATED, Json(role.into())))
}

pub async fn patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((team, role_id)): Path<(String, String)>,
    Json(req): Json<PatchRoleRequest>,
) -> Result<Json<RoleView>, ApiError> {
    let team = parse_team(&team)?;
    let role_id = parse_role(&role_id)?;
    perm::require_admin(&state.db, user.id, team).await?;

    // Confirm role belongs to this team — repo::set_policy refuses
    // system roles already (team_id IS NOT NULL guard), but checking
    // here lets us return 400 with a clearer message.
    let existing = repo::find_by_id(&state.db, role_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if existing.team_id != Some(team) {
        return Err(ApiError::BadRequest(format!(
            "role {role_id} does not belong to team {team}"
        )));
    }

    let updated = repo::set_policy(&state.db, role_id, &req.policy).await?;
    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "role.policy_updated",
        serde_json::json!({"role_id": role_id.to_string(), "name": updated.name}),
    )
    .await;
    info!(team = %team, %role_id, "role policy updated");
    Ok(Json(updated.into()))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((team, role_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let team = parse_team(&team)?;
    let role_id = parse_role(&role_id)?;
    perm::require_admin(&state.db, user.id, team).await?;

    // Refuse to delete the team's own admin/member seeds — those are
    // load-bearing for the dashboard's role picker. Operators can
    // edit their policy via PATCH but not destroy them.
    let existing = repo::find_by_id(&state.db, role_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if existing.team_id != Some(team) {
        return Err(ApiError::BadRequest(format!(
            "role {role_id} does not belong to team {team}"
        )));
    }
    if matches!(existing.name.as_str(), "admin" | "member") {
        return Err(ApiError::Conflict(format!(
            "the team's seeded {:?} role can be edited but not deleted",
            existing.name
        )));
    }

    repo::delete(&state.db, role_id)
        .await
        .map_err(|e| match e {
            havn_db::DbError::Conflict(_) => ApiError::Conflict(
                "role still has members — re-assign them first via PATCH /members/{user_id}".into(),
            ),
            other => other.into(),
        })?;
    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "role.deleted",
        serde_json::json!({"role_id": role_id.to_string(), "name": existing.name}),
    )
    .await;
    info!(team = %team, %role_id, "role deleted");
    Ok(StatusCode::NO_CONTENT)
}

fn parse_team(s: &str) -> Result<TeamId, ApiError> {
    TeamId::from_str(s).map_err(|_| ApiError::BadRequest("invalid team id".into()))
}

fn parse_role(s: &str) -> Result<RoleId, ApiError> {
    RoleId::from_str(s).map_err(|_| ApiError::BadRequest("invalid role id".into()))
}
