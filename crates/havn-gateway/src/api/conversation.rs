//! `GET /agents/:id/conversation` — recent webchat history (spec §5.2).
//!
//! Reads `agent.db` read-only and returns the last N turns for the
//! caller's stable webchat channel (UUIDv5 derived from
//! `(user_id, agent_id)` — same derivation as `webchat_ws::handle_session`).
//! Lets the dashboard restore the conversation when the user navigates
//! away and comes back.
//!
//! Spec §5.2 nuance: agent.db is single-writer (the runtime) but
//! the gateway can open it read-only; we use the same `connect_read_only`
//! pattern as `api::memory::list`. The endpoint works whether the
//! agent runtime is running or not — the conversations table is on
//! disk and immutable from the gateway side.

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use havn_core::AgentId;
use havn_db::agent::conversations::{self, Role};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;

#[derive(Debug, Deserialize)]
pub struct ConversationQuery {
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TurnView {
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ConversationResponse {
    pub agent_id: String,
    /// The stable webchat channel id this user has with this agent —
    /// same UUIDv5 the WS handler uses. Surfaced so the dashboard can
    /// distinguish messages it just sent (matching channel) from any
    /// other channel's history (heartbeat / cron) if those ever leak in.
    pub channel_id: String,
    pub turns: Vec<TurnView>,
    /// `true` when the agent has never been started — agent.db
    /// doesn't exist on disk yet.
    pub uninitialised: bool,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<ConversationQuery>,
) -> Result<Json<ConversationResponse>, ApiError> {
    let agent_id =
        AgentId::from_str(&id).map_err(|_| ApiError::BadRequest("invalid agent id".into()))?;

    let agent = havn_db::repo::agents::find_by_id(&state.db, agent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if agent.owner_id != user.id {
        return Err(ApiError::NotFound);
    }

    // Same derivation webchat_ws::handle_session uses — UUIDv5 over
    // (sender_id, agent_id). If these two ever drift the dashboard
    // shows an empty timeline despite the agent.db having rows; pull
    // into a shared helper if a third caller appears.
    let channel_id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("havn-webchat:{}:{}", user.id, agent_id).as_bytes(),
    )
    .to_string();

    let agent_db_path = state
        .workspace_root
        .join(agent_id.to_string())
        .join("workspace")
        .join("agent.db");
    if !tokio::fs::try_exists(&agent_db_path).await.unwrap_or(false) {
        return Ok(Json(ConversationResponse {
            agent_id: id,
            channel_id,
            turns: Vec::new(),
            uninitialised: true,
        }));
    }

    let pool = match havn_db::agent::connect_read_only(&agent_db_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                agent_id = %id,
                error = %e,
                "conversation list: opening agent.db RO failed"
            );
            return Err(ApiError::Internal("opening agent.db".into()));
        }
    };

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let turns = conversations::recent_with_channel(&pool, &channel_id, limit).await?;
    let views = turns
        .into_iter()
        .map(|t| TurnView {
            role: role_str(t.role).into(),
            content: t.content,
            created_at: t.created_at,
        })
        .collect();
    Ok(Json(ConversationResponse {
        agent_id: id,
        channel_id,
        turns: views,
        uninitialised: false,
    }))
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}
