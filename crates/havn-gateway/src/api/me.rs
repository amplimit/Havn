//! `GET /me` — info about the caller.
//!
//! Identity comes from the upstream reverse proxy (spec §1.7) — havn
//! does not authenticate. Single-user-mode loopback returns the
//! bootstrap user.

use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;

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
