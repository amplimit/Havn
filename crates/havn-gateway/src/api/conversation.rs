//! The caller's dashboard chats with an agent (spec §5.2, issue #126).
//!
//! `GET /agents/:id/conversation` returns the last N turns of one chat of
//! the caller's webchat session (UUIDv5 of `(user_id, agent_id)`, as in
//! `webchat_ws::handle_session`), by default its active one, so the
//! dashboard can restore it. `GET /agents/:id/chats` lists the session's
//! chats; `PATCH` and `DELETE /agents/:id/chats/:chat` rename and delete
//! one.
//!
//! The runtime reads and writes its own agent.db (it is started if
//! needed); the gateway never opens the file, which the agent can write.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_db::agent::conversations::{BranchTurn, Chat, Role};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;
const MAX_CHATS: u32 = 200;

#[derive(Debug, Deserialize)]
pub struct ConversationQuery {
    pub limit: Option<u32>,
    /// The chat to read; the active one when absent.
    pub conversation: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TurnView {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    /// Its versions (issue #129): the turns in its place on other
    /// branches, itself included, oldest first.
    pub versions: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ConversationResponse {
    pub agent_id: String,
    /// The webchat session this user has with this agent — same UUIDv5
    /// the WS handler uses.
    pub channel_id: String,
    /// The chat the turns are of; `None` when the session has no active
    /// chat yet (or the one asked for is not its).
    pub conversation: Option<String>,
    pub turns: Vec<TurnView>,
    /// Messages of the chat the agent has not answered yet, the one it
    /// answers now included; absent from a runtime that does not say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<usize>,
    /// `true` when the agent has never been started — agent.db
    /// doesn't exist on disk yet.
    pub uninitialised: bool,
}

#[derive(Debug, Deserialize)]
struct ConversationRows {
    conversation: Option<String>,
    turns: Vec<BranchTurn>,
    #[serde(default)]
    waiting: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct Found {
    found: bool,
}

#[derive(Debug, Deserialize)]
pub struct BranchSwitch {
    pub turn: String,
}

#[derive(Debug, Deserialize)]
pub struct ChatPatch {
    pub title: String,
}

/// The webchat session of `user` with `agent`.
fn session(user: &AuthedUser, agent: havn_core::AgentId) -> String {
    crate::webchat::session_for(&user.id.to_string(), agent).to_string()
}

fn chat_param(conversation: Option<String>) -> Result<Option<String>, ApiError> {
    match conversation {
        Some(c) if c.is_empty() || c.len() > crate::webchat::MAX_CONVERSATION_ID => {
            Err(ApiError::BadRequest("conversation is not a chat id".into()))
        }
        other => Ok(other),
    }
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<ConversationQuery>,
) -> Result<Json<ConversationResponse>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;
    let channel_id = session(&user, agent_id);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let rows: ConversationRows = crate::api::agents::agent_data(
        &state,
        user.id,
        agent_id,
        havn_proto::DataQuery::Conversation {
            channel_id: channel_id.clone(),
            conversation: chat_param(q.conversation)?,
            limit,
        },
        true,
    )
    .await?;
    let views = rows
        .turns
        .into_iter()
        .map(|t| TurnView {
            id: t.id,
            role: role_str(t.role).into(),
            content: t.content,
            created_at: t.created_at,
            versions: t.versions,
        })
        .collect();
    Ok(Json(ConversationResponse {
        agent_id: agent_id.to_string(),
        channel_id,
        conversation: rows.conversation,
        turns: views,
        waiting: rows.waiting,
        uninitialised: false,
    }))
}

/// `GET /agents/:id/chats`: the caller's chats with the agent, most
/// recently used first.
pub async fn chats(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<Chat>>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let chats: Vec<Chat> = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::Chats {
            channel_id: session(&user, agent.id),
            limit: MAX_CHATS,
        },
        true,
    )
    .await?;
    Ok(Json(chats))
}

/// `PATCH /agents/:id/chats/:chat`: rename one of the caller's chats.
pub async fn chat_patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, chat)): Path<(String, String)>,
    Json(req): Json<ChatPatch>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let title = req.title.trim();
    if title.is_empty() {
        return Err(ApiError::BadRequest("title must be non-empty".into()));
    }
    let renamed: Found = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::RenameChat {
            channel_id: session(&user, agent.id),
            conversation: chat_param(Some(chat))?.unwrap_or_default(),
            title: title.to_string(),
        },
        true,
    )
    .await?;
    if !renamed.found {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /agents/:id/chats/:chat/branch`: continue one of the caller's
/// chats from the branch of `turn`, at its newest turn.
pub async fn chat_branch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, chat)): Path<(String, String)>,
    Json(req): Json<BranchSwitch>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let switched: Found = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::SwitchBranch {
            channel_id: session(&user, agent.id),
            conversation: chat_param(Some(chat))?.unwrap_or_default(),
            turn: chat_param(Some(req.turn))?.unwrap_or_default(),
        },
        true,
    )
    .await?;
    if !switched.found {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /agents/:id/chats/:chat`: delete one of the caller's chats and
/// its history.
pub async fn chat_delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, chat)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let conversation = chat_param(Some(chat))?.unwrap_or_default();
    let deleted: Found = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::DeleteChat {
            channel_id: session(&user, agent.id),
            conversation: conversation.clone(),
        },
        true,
    )
    .await?;
    if !deleted.found {
        return Err(ApiError::NotFound);
    }
    // Its tasks report to the conversation, no longer to the chat.
    havn_db::repo::cron::forget_chat(&state.db, agent.id, &conversation).await?;
    crate::audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.chat.deleted",
        serde_json::json!({ "conversation": conversation }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}
