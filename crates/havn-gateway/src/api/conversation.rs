//! `GET /agents/:id/conversation` — recent webchat history (spec §5.2).
//!
//! Asks the agent's runtime for the last N turns of the
//! caller's stable webchat channel (UUIDv5 derived from
//! `(user_id, agent_id)` — same derivation as `webchat_ws::handle_session`).
//! Lets the dashboard restore the conversation when the user navigates
//! away and comes back.
//!
//! The runtime reads its own agent.db (it is started if needed); the
//! gateway never opens the file, which the agent can write.

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use havn_db::agent::conversations::{self, Role};
use serde::{Deserialize, Serialize};
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
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;

    // Same derivation webchat_ws::handle_session uses — UUIDv5 over
    // (sender_id, agent_id). If these two ever drift the dashboard
    // shows an empty timeline despite the agent.db having rows; pull
    // into a shared helper if a third caller appears.
    let channel_id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("havn-webchat:{}:{}", user.id, agent_id).as_bytes(),
    )
    .to_string();

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let turns: Vec<conversations::TurnWithChannel> = crate::api::agents::agent_data(
        &state,
        user.id,
        agent_id,
        havn_proto::DataQuery::Conversation {
            channel_id: channel_id.clone(),
            limit,
        },
    )
    .await?;
    let views = turns
        .into_iter()
        .map(|t| TurnView {
            role: role_str(t.role).into(),
            content: t.content,
            created_at: t.created_at,
        })
        .collect();
    Ok(Json(ConversationResponse {
        agent_id: agent_id.to_string(),
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
