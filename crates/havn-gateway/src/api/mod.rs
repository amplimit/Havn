//! HTTP API surface for the gateway.
//!
//! Each submodule owns the routes for one entity. [`ApiError`] is the
//! gateway's uniform handler-return type and turns into a JSON error body
//! with an appropriate status code.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use havn_db::DbError;
use serde_json::json;

pub mod agents;
pub mod audit;
pub mod bootstrap;
pub mod channel;
pub mod conversation;
pub mod credentials;
pub mod cron;
pub mod embedding;
pub mod llm;
pub mod mcp;
pub mod me;
pub mod members;
pub mod memory;
pub mod roles;
pub mod skills;
pub mod team_credentials;
pub mod teams;
pub mod webchat_ws;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    #[error("not found")]
    NotFound,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    /// Caller is not authenticated, OR an authentication token they
    /// presented didn't match. Distinct from `Forbidden` which means
    /// "authenticated but not allowed". Used by channel adapter WS
    /// upgrade when X-Adapter-Token is missing or wrong.
    #[error("unauthorized")]
    Unauthorized,

    /// Authenticated but the policy doesn't allow this action — quota
    /// exhausted, capability disabled, etc. (spec §6.3 enforcement).
    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("upstream provider error: {0}")]
    Upstream(String),

    #[error("no usable credential for provider {0:?}")]
    NoCredential(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => Self::NotFound,
            DbError::Conflict(c) => Self::Conflict(c.to_string()),
            DbError::InvalidValue { column, message } => {
                Self::BadRequest(format!("invalid {column}: {message}"))
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            Self::Conflict(_) => (StatusCode::CONFLICT, self.to_string()),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, self.to_string()),
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            Self::NoCredential(_) => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
