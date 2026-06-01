//! `/credentials` REST endpoints (spec §8.3).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_core::CredentialId;
use havn_db::repo::credentials::{
    self as repo, Credential, CredentialScope, CredentialUpdate, NewCredential,
};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

#[derive(Debug, Deserialize)]
pub struct CreateCredentialRequest {
    pub provider: String,
    /// Optional v0.2 handle (spec §7.3). When present, the credential is
    /// addressable by `secret:<provider>:<name>` from config blocks
    /// (channel adapter tokens, OAuth2 SaaS rows). When absent, the row
    /// behaves as v0.1 — fallback-chain only, looked up by provider +
    /// priority.
    #[serde(default)]
    pub name: Option<String>,
    pub api_key: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub limits: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCredentialRequest {
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub limits: Option<serde_json::Value>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Public view of a credential — `api_key` is intentionally omitted.
/// `scope` and `scope_id` are surfaced so the dashboard can render
/// "personal" vs "team-shared" badges without a separate lookup.
#[derive(Debug, Serialize)]
pub struct CredentialView {
    pub id: String,
    pub scope: String,
    pub scope_id: String,
    pub provider: String,
    pub priority: i32,
    pub limits: serde_json::Value,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

impl From<Credential> for CredentialView {
    fn from(c: Credential) -> Self {
        Self {
            id: c.id.to_string(),
            scope: c.scope.as_str().into(),
            scope_id: c.scope_id,
            provider: c.provider,
            priority: c.priority,
            limits: c.limits,
            enabled: c.enabled,
            created_at: c.created_at,
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    let creds =
        repo::list_for_scope(&state.db, CredentialScope::User, &user.id.to_string()).await?;
    Ok(Json(creds.into_iter().map(CredentialView::from).collect()))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<CreateCredentialRequest>,
) -> Result<(StatusCode, Json<CredentialView>), ApiError> {
    if req.api_key.is_empty() {
        return Err(ApiError::BadRequest("api_key must be non-empty".into()));
    }
    if req.provider.is_empty() {
        return Err(ApiError::BadRequest("provider must be non-empty".into()));
    }

    let owner = user.id.to_string();
    // Encrypt the user-supplied plaintext API key before it crosses
    // the DB boundary (spec §13 Phase 3). KeyRing is `None` only on a
    // fresh install where the operator hasn't set HAVN_AGE_KEY AND
    // no credentials exist yet — refuse the write rather than letting
    // a plaintext row sneak in.
    let ring = state.keyring.as_ref().ok_or_else(|| {
        ApiError::BadRequest(
            "credential encryption is not configured: set HAVN_AGE_KEY and restart the gateway"
                .into(),
        )
    })?;
    let ciphertext = ring.encrypt(req.api_key.as_bytes()).map_err(|e| {
        tracing::error!(error = %e, "keyring encrypt failed during credential create");
        ApiError::Internal("credential encryption failed".into())
    })?;
    let cred = repo::create(
        &state.db,
        NewCredential {
            scope: CredentialScope::User,
            scope_id: &owner,
            provider: &req.provider,
            name: req.name.as_deref(),
            api_key_ciphertext: &ciphertext,
            priority: req.priority,
            limits: req.limits,
        },
    )
    .await?;
    info!(credential_id = %cred.id, provider = %cred.provider, "credential created");
    audit::record_user_action(
        &state.db,
        user.id,
        "credential.created",
        serde_json::json!({
            "credential_id": cred.id.to_string(),
            "provider": cred.provider,
        }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(cred.into())))
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<UpdateCredentialRequest>,
) -> Result<Json<CredentialView>, ApiError> {
    let id = parse_id(&id)?;
    let owner = ensure_owner(&state, &user, id).await?;
    let updated = repo::update(
        &state.db,
        id,
        CredentialUpdate {
            priority: req.priority,
            limits: req.limits,
            enabled: req.enabled,
        },
    )
    .await?;
    info!(credential_id = %id, owner = %owner, "credential updated");
    audit::record_user_action(
        &state.db,
        user.id,
        "credential.updated",
        serde_json::json!({"credential_id": id.to_string()}),
    )
    .await;
    Ok(Json(updated.into()))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<StatusOk, ApiError> {
    let id = parse_id(&id)?;
    let _ = ensure_owner(&state, &user, id).await?;
    repo::delete(&state.db, id).await?;
    info!(credential_id = %id, "credential deleted");
    audit::record_user_action(
        &state.db,
        user.id,
        "credential.deleted",
        serde_json::json!({"credential_id": id.to_string()}),
    )
    .await;
    Ok(StatusOk)
}

/// Tiny marker for "204 No Content" handler returns.
pub struct StatusOk;
impl axum::response::IntoResponse for StatusOk {
    fn into_response(self) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
}

fn parse_id(s: &str) -> Result<CredentialId, ApiError> {
    CredentialId::from_str(s).map_err(|_| ApiError::BadRequest("invalid credential id".into()))
}

/// Ensure the calling user owns the credential before mutating it.
///
/// This endpoint deliberately scopes to user-scoped credentials only —
/// team-shared credentials live at `/teams/{id}/credentials` (admin-
/// only mutations there). A team admin who tries to PATCH a team
/// credential through `/credentials/{id}` gets a 404 (we don't leak
/// existence across scopes), which is correct: management of team
/// keys belongs at the team-scoped surface.
async fn ensure_owner(
    state: &AppState,
    user: &AuthedUser,
    id: CredentialId,
) -> Result<String, ApiError> {
    let cred = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if cred.scope != CredentialScope::User || cred.scope_id != user.id.to_string() {
        return Err(ApiError::NotFound);
    }
    Ok(cred.scope_id)
}
