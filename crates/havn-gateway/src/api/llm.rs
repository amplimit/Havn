//! Test endpoint for the LLM proxy.
//!
//! `POST /v1/llm/anthropic` — accepts an Anthropic Messages-shaped request,
//! resolves a credential, calls Anthropic, returns the response. Acts as the
//! end-to-end smoke test for the credential resolver + LLM proxy until the
//! agent runtime takes over via the Unix socket protocol.

use axum::Json;
use axum::extract::State;
use tracing::error;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;
use crate::llm_proxy::{self, AnthropicRequest, AnthropicResponse, ProxyError};

pub async fn anthropic(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<AnthropicRequest>,
) -> Result<Json<AnthropicResponse>, ApiError> {
    if req.model.is_empty() {
        return Err(ApiError::BadRequest("model must be non-empty".into()));
    }
    if req.messages.is_empty() {
        return Err(ApiError::BadRequest("messages must be non-empty".into()));
    }
    if req.max_tokens == 0 {
        return Err(ApiError::BadRequest("max_tokens must be > 0".into()));
    }

    let ring = state.keyring.as_ref().ok_or_else(|| {
        // No HAVN_AGE_KEY: boot guard only allowed this state when the
        // credentials table is empty. If we got here with the table
        // empty, NoCredential is the right error; if the table is
        // non-empty the boot guard would have refused start, so we
        // wouldn't be running.
        ApiError::NoCredential("anthropic".into())
    })?;

    let resp = llm_proxy::anthropic_complete(&state.db, &state.http, ring, user.id, &req)
        .await
        .map_err(map_proxy_error)?;
    Ok(Json(resp))
}

fn map_proxy_error(e: ProxyError) -> ApiError {
    error!(error = %e, "anthropic proxy failed");
    match e {
        ProxyError::NoCredential(p) => ApiError::NoCredential(p),
        ProxyError::Upstream { status, message } => {
            ApiError::Upstream(format!("status {status}: {message}"))
        }
        ProxyError::Transport(err) => ApiError::Upstream(format!("transport: {err}")),
        ProxyError::InvalidResponse(s) => ApiError::Upstream(format!("invalid response: {s}")),
        ProxyError::Db(err) => ApiError::Internal(err.to_string()),
        ProxyError::UnknownProvider(p) => ApiError::BadRequest(format!("unknown provider: {p}")),
    }
}
