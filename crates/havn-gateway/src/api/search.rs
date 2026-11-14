//! `GET /search`: which `web_search` backend the caller's agents get, so
//! the dashboard and `havn doctor` can say whether search works and how to
//! turn it on.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;

#[derive(Debug, Serialize)]
pub struct SearchStatus {
    /// `[search] provider`: `auto`, `off` or one backend.
    pub provider: String,
    /// The backend the caller's personal agents use first, or `None` when
    /// they have no `web_search` tool.
    pub backend: Option<&'static str>,
}

pub async fn status(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<SearchStatus>, ApiError> {
    let backend = state
        .search
        .backend_for(&state.db, user.id, None, &state.models.load())
        .await
        .map_err(|e| ApiError::Internal(format!("search credential lookup failed: {e}")))?;
    Ok(Json(SearchStatus {
        provider: state.search.provider().to_string(),
        backend: backend.map(crate::web_search::Backend::name),
    }))
}
