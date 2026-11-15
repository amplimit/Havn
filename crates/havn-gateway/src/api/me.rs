//! `GET /me` — info about the caller.
//!
//! Identity comes from the upstream reverse proxy (spec §1.7) — havn
//! does not authenticate. Single-user-mode loopback returns the
//! bootstrap user.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;

#[derive(Debug, Serialize)]
pub struct MeView {
    pub id: String,
    pub display_name: String,
    /// Token to attach to WebSocket connections as `?token=<...>`.
    /// Equals the user id — the WebChat WS handler validates origin +
    /// token-equals-user-id, so leaking it doesn't grant cross-tenant
    /// access (the upstream proxy still has to allow the connection).
    pub ws_token: String,
    /// Reserved for symmetry with future SystemTimeProtocol-style
    /// fields. Currently always now() because the AuthedUser resolver
    /// doesn't lift created_at out of the row — `created_at` viewing
    /// can come back when the dashboard needs it.
    #[serde(skip)]
    _phantom: Option<DateTime<Utc>>,
}

pub async fn get(user: AuthedUser) -> Result<Json<MeView>, ApiError> {
    let id = user.id.to_string();
    Ok(Json(MeView {
        ws_token: id.clone(),
        id,
        display_name: user.display_name,
        _phantom: None,
    }))
}

#[derive(Debug, Serialize)]
pub struct TimezoneView {
    /// The zone the caller's agents tell the time in.
    pub timezone: String,
    /// The caller set it; otherwise it is the gateway host's.
    pub chosen: bool,
    pub host: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimezonePut {
    /// IANA name, or `null` for the gateway host's.
    pub timezone: Option<String>,
}

async fn timezone_view(state: &AppState, user: &AuthedUser) -> Result<TimezoneView, ApiError> {
    let chosen = havn_db::repo::users::timezone(&state.db, user.id).await?;
    Ok(TimezoneView {
        timezone: crate::timezone::for_user(&state.db, user.id).await,
        chosen: chosen.is_some(),
        host: crate::timezone::host(),
    })
}

pub async fn timezone_get(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<TimezoneView>, ApiError> {
    Ok(Json(timezone_view(&state, &user).await?))
}

/// Set the caller's time zone; their running agents get it at once.
pub async fn timezone_put(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<TimezonePut>,
) -> Result<Json<TimezoneView>, ApiError> {
    crate::timezone::set(&state.db, &state.registry, user.id, req.timezone.as_deref())
        .await
        .map_err(ApiError::BadRequest)?;
    crate::audit::record_user_action(
        &state.db,
        user.id,
        "user.timezone.updated",
        serde_json::json!({ "timezone": req.timezone }),
    )
    .await;
    Ok(Json(timezone_view(&state, &user).await?))
}
