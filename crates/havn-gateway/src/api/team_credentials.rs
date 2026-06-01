//! `/teams/{id}/credentials` — team-scoped LLM provider credentials
//! (spec §7, §10.3).
//!
//! Mirrors `api::credentials` for `scope = team`. Admin-only mutations.
//! Members can list (so they know which providers their team-shared
//! keys cover) but the API key bytes are never surfaced — the
//! `CredentialView` shape omits `api_key` by construction.
//!
//! Per-user caps live in `limits.per_user.{max_tokens_per_day,
//! max_requests_per_minute}`. The credential resolver consults these
//! at LLM-proxy time (see `crate::credential_resolver::per_user_ceilings_remaining`).
//!
//! Spec §1.5 (`Phase 2 still to land`): "team-level credentials with
//! per-user token / RPM caps (the schema supports it; consume + UI)".
//! This module is the consume-side; the dashboard is the UI.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use havn_core::{CredentialId, TeamId};
use havn_db::repo::credentials::{
    self as repo, Credential, CredentialScope, CredentialUpdate, NewCredential,
};
use serde::Deserialize;
use std::str::FromStr as _;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::api::credentials::CredentialView;
use crate::audit;
use crate::auth::AuthedUser;
use crate::perm;

#[derive(Debug, Deserialize)]
pub struct CreateTeamCredentialRequest {
    pub provider: String,
    /// Optional v0.2 handle (spec §7.3). See `CreateCredentialRequest::name`.
    #[serde(default)]
    pub name: Option<String>,
    pub api_key: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub limits: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTeamCredentialRequest {
    pub priority: Option<i32>,
    pub limits: Option<serde_json::Value>,
    pub enabled: Option<bool>,
}

/// Members can list so they understand what shared keys exist.
pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(team): Path<String>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let team = parse_team(&team)?;
    perm::require_member(&state.db, user.id, team).await?;
    let creds = repo::list_for_scope(&state.db, CredentialScope::Team, &team.to_string()).await?;
    Ok(Json(creds.into_iter().map(CredentialView::from).collect()))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(team): Path<String>,
    Json(req): Json<CreateTeamCredentialRequest>,
) -> Result<(StatusCode, Json<CredentialView>), ApiError> {
    let team = parse_team(&team)?;
    perm::require_admin(&state.db, user.id, team).await?;
    if req.api_key.is_empty() {
        return Err(ApiError::BadRequest("api_key must be non-empty".into()));
    }
    if req.provider.is_empty() {
        return Err(ApiError::BadRequest("provider must be non-empty".into()));
    }

    // Encrypt before crossing the DB boundary — same boot guard as
    // user-scoped /credentials. See `api::credentials::create`.
    let ring = state.keyring.as_ref().ok_or_else(|| {
        ApiError::BadRequest(
            "credential encryption is not configured: set HAVN_AGE_KEY and restart the gateway"
                .into(),
        )
    })?;
    let ciphertext = ring.encrypt(req.api_key.as_bytes()).map_err(|e| {
        tracing::error!(error = %e, "keyring encrypt failed during team credential create");
        ApiError::Internal("credential encryption failed".into())
    })?;
    let cred = repo::create(
        &state.db,
        NewCredential {
            scope: CredentialScope::Team,
            scope_id: &team.to_string(),
            provider: &req.provider,
            name: req.name.as_deref(),
            api_key_ciphertext: &ciphertext,
            priority: req.priority,
            limits: req.limits.clone(),
        },
    )
    .await?;
    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "team_credential.created",
        serde_json::json!({
            "credential_id": cred.id.to_string(),
            "provider": cred.provider,
            "limits": req.limits,
        }),
    )
    .await;
    info!(team = %team, credential_id = %cred.id, provider = %cred.provider, "team credential created");
    Ok((StatusCode::CREATED, Json(cred.into())))
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((team, cred_id)): Path<(String, String)>,
    Json(req): Json<UpdateTeamCredentialRequest>,
) -> Result<Json<CredentialView>, ApiError> {
    let team = parse_team(&team)?;
    let cred_id = parse_credential(&cred_id)?;
    perm::require_admin(&state.db, user.id, team).await?;
    ensure_team_owned(&state, team, cred_id).await?;

    let updated = repo::update(
        &state.db,
        cred_id,
        CredentialUpdate {
            priority: req.priority,
            limits: req.limits.clone(),
            enabled: req.enabled,
        },
    )
    .await?;
    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "team_credential.updated",
        serde_json::json!({
            "credential_id": cred_id.to_string(),
            "patch": {
                "priority": req.priority,
                "limits": req.limits,
                "enabled": req.enabled,
            }
        }),
    )
    .await;
    Ok(Json(updated.into()))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((team, cred_id)): Path<(String, String)>,
) -> Result<axum::http::StatusCode, ApiError> {
    let team = parse_team(&team)?;
    let cred_id = parse_credential(&cred_id)?;
    perm::require_admin(&state.db, user.id, team).await?;
    ensure_team_owned(&state, team, cred_id).await?;
    repo::delete(&state.db, cred_id).await?;
    audit::record(
        &state.db,
        user.id,
        Some(team),
        None,
        "team_credential.deleted",
        serde_json::json!({"credential_id": cred_id.to_string()}),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn ensure_team_owned(
    state: &AppState,
    team: TeamId,
    cred_id: CredentialId,
) -> Result<Credential, ApiError> {
    let cred = repo::find_by_id(&state.db, cred_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if cred.scope != CredentialScope::Team || cred.scope_id != team.to_string() {
        // Cross-tenant probe — return NotFound instead of Forbidden so
        // we don't leak whether the id exists in another team.
        return Err(ApiError::NotFound);
    }
    Ok(cred)
}

fn parse_team(s: &str) -> Result<TeamId, ApiError> {
    TeamId::from_str(s).map_err(|_| ApiError::BadRequest("invalid team id".into()))
}

fn parse_credential(s: &str) -> Result<CredentialId, ApiError> {
    CredentialId::from_str(s).map_err(|_| ApiError::BadRequest("invalid credential id".into()))
}
