//! `/teams/{id}/members` — team membership management (spec §8.3, §10.3).
//!
//! All endpoints are admin-only — adding/removing members and changing
//! their roles is sensitive. Listing is also admin-only because the
//! roster includes display names that aren't otherwise discoverable.
//!
//! Spec §10.3 calls out: "no invite link / email flow — that's the
//! upstream auth proxy's job". An admin types `havn user add ...` to
//! provision the account, then opens this endpoint with the resulting
//! id. The dashboard surfaces both steps.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_core::{RoleId, TeamId, UserId};
use havn_db::repo::team_memberships::{self, MembershipDetail};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::perm;

#[derive(Debug, Serialize)]
pub struct MemberView {
    pub user_id: String,
    pub display_name: String,
    pub role_id: String,
    pub role_name: String,
    pub joined_at: DateTime<Utc>,
}

impl From<MembershipDetail> for MemberView {
    fn from(m: MembershipDetail) -> Self {
        Self {
            user_id: m.user_id.to_string(),
            display_name: m.user_display_name,
            role_id: m.role_id.to_string(),
            role_name: m.role_name,
            joined_at: m.joined_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    /// X-User-ID of the user to add. Must already exist in the users
    /// table — the operator runs `havn user add` first.
    pub user_id: String,
    /// Role id within this team. The dashboard's role picker enumerates
    /// `/teams/{id}/roles` and binds one of those ids here.
    pub role_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchMemberRequest {
    /// New role id within this team.
    pub role_id: String,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(team): Path<String>,
) -> Result<Json<Vec<MemberView>>, ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;
    let members = team_memberships::list_for_team(&state.db, team).await?;
    Ok(Json(members.into_iter().map(MemberView::from).collect()))
}

pub async fn add(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(team): Path<String>,
    Json(req): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<MemberView>), ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;

    let new_user = parse_user(&req.user_id)?;
    let role_id = parse_role(&req.role_id)?;

    // Validate the user exists and the role belongs to this team.
    let target_user = havn_db::repo::users::find_by_id(&state.db, new_user)
        .await?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "no user with id {new_user}; provision via `havn user add` first"
            ))
        })?;
    let role = havn_db::repo::roles::find_by_id(&state.db, role_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest(format!("no role with id {role_id}")))?;
    if role.team_id != Some(team) {
        return Err(ApiError::BadRequest(format!(
            "role {role_id} does not belong to team {team}"
        )));
    }

    let membership = team_memberships::add(&state.db, new_user, team, role_id)
        .await
        .map_err(|e| match e {
            havn_db::DbError::Conflict(_) => ApiError::Conflict(
                "user is already a member of this team — patch their role instead".into(),
            ),
            other => other.into(),
        })?;

    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "member.added",
        serde_json::json!({
            "user_id": new_user.to_string(),
            "display_name": target_user.display_name,
            "role": role.name,
        }),
    )
    .await;
    info!(team = %team, %new_user, role = %role.name, "member added");

    let view = MemberView {
        user_id: new_user.to_string(),
        display_name: target_user.display_name,
        role_id: role_id.to_string(),
        role_name: role.name,
        joined_at: membership.joined_at,
    };
    Ok((StatusCode::CREATED, Json(view)))
}

pub async fn patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((team, member)): Path<(String, String)>,
    Json(req): Json<PatchMemberRequest>,
) -> Result<Json<MemberView>, ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;
    let member = parse_user(&member)?;
    let new_role_id = parse_role(&req.role_id)?;

    let role = havn_db::repo::roles::find_by_id(&state.db, new_role_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest(format!("no role with id {new_role_id}")))?;
    if role.team_id != Some(team) {
        return Err(ApiError::BadRequest(format!(
            "role {new_role_id} does not belong to team {team}"
        )));
    }

    // Self-demote guard: prevent the *only* admin from demoting themselves.
    // Otherwise a one-admin team can be left with no admin at all and the
    // team-management endpoints become unreachable. Listing all admins is
    // cheap (small table) and beats trying to encode this in SQL.
    if user.id == member && role.name != "admin" {
        let detail_rows = team_memberships::list_for_team(&state.db, team).await?;
        let admin_count = detail_rows
            .iter()
            .filter(|m| m.role_name == "admin")
            .count();
        if admin_count <= 1 {
            return Err(ApiError::Conflict(
                "you are the only admin of this team — promote someone else before demoting yourself".into(),
            ));
        }
    }

    team_memberships::set_role(&state.db, member, team, new_role_id).await?;

    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "member.role_changed",
        serde_json::json!({
            "user_id": member.to_string(),
            "role": role.name,
        }),
    )
    .await;
    info!(team = %team, %member, role = %role.name, "member role changed");

    let updated = team_memberships::find(&state.db, member, team)
        .await?
        .ok_or(ApiError::NotFound)?;
    let target_user = havn_db::repo::users::find_by_id(&state.db, member)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(MemberView {
        user_id: member.to_string(),
        display_name: target_user.display_name,
        role_id: updated.role_id.to_string(),
        role_name: role.name,
        joined_at: updated.joined_at,
    }))
}

pub async fn remove(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((team, member)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;
    let member = parse_user(&member)?;

    // Same self-removal guard as `patch` — never let the last admin
    // remove themselves and lock the team out.
    if user.id == member {
        let detail_rows = team_memberships::list_for_team(&state.db, team).await?;
        let admin_count = detail_rows
            .iter()
            .filter(|m| m.role_name == "admin")
            .count();
        let is_admin = detail_rows
            .iter()
            .any(|m| m.user_id == member && m.role_name == "admin");
        if is_admin && admin_count <= 1 {
            return Err(ApiError::Conflict(
                "you are the only admin of this team — promote someone else before removing yourself".into(),
            ));
        }
    }

    team_memberships::remove(&state.db, member, team).await?;
    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "member.removed",
        serde_json::json!({"user_id": member.to_string()}),
    )
    .await;
    info!(team = %team, %member, "member removed");
    Ok(StatusCode::NO_CONTENT)
}

fn parse_team(s: &str) -> Result<TeamId, ApiError> {
    TeamId::from_str(s).map_err(|_| ApiError::BadRequest("invalid team id".into()))
}

fn parse_user(s: &str) -> Result<UserId, ApiError> {
    UserId::from_str(s).map_err(|_| ApiError::BadRequest("invalid user id".into()))
}

fn parse_role(s: &str) -> Result<RoleId, ApiError> {
    RoleId::from_str(s).map_err(|_| ApiError::BadRequest("invalid role id".into()))
}
