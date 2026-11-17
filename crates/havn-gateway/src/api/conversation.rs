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
    /// For a reply: what its turn did on the way, as its folded line says
    /// it (issue #178); the whole of it from `GET .../turns/{turn}/trace`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceSummary>,
}

/// What a reply's folded line says of its turn.
#[derive(Debug, Serialize, Deserialize)]
pub struct TraceSummary {
    pub ms: u64,
    pub thought: bool,
    pub steps: u32,
}

/// One part of what a reply's turn did, as the dashboard shows it. The
/// agent writes its database, so what it stored is read into these and
/// nothing else.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceItem {
    Thinking {
        text: String,
    },
    Step {
        tool: String,
        input: String,
        result: String,
        #[serde(default)]
        error: bool,
    },
}

/// What a reply's turn did on the way.
#[derive(Debug, Serialize)]
pub struct TraceView {
    pub ms: u64,
    pub items: Vec<TraceItem>,
    pub left_out: u64,
}

/// Characters of a trace part passed on: the runtime keeps them shorter.
const MAX_TRACE_PART: usize = 10_000;
/// Parts of a trace passed on at most.
const MAX_TRACE_ITEMS: usize = 100;

/// A stored trace as the dashboard may see it: parts of the known kinds,
/// cut to size; `None` when it is not a trace.
fn trace_view(raw: &str) -> Option<TraceView> {
    let trace: serde_json::Value = serde_json::from_str(raw).ok()?;
    let cut = |s: String| -> String { s.chars().take(MAX_TRACE_PART).collect() };
    let items = trace
        .get("items")?
        .as_array()?
        .iter()
        .take(MAX_TRACE_ITEMS)
        .filter_map(|item| serde_json::from_value::<TraceItem>(item.clone()).ok())
        .map(|item| match item {
            TraceItem::Thinking { text } => TraceItem::Thinking { text: cut(text) },
            TraceItem::Step {
                tool,
                input,
                result,
                error,
            } => TraceItem::Step {
                tool: cut(tool),
                input: cut(input),
                result: cut(result),
                error,
            },
        })
        .collect();
    Some(TraceView {
        ms: trace
            .get("ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        items,
        left_out: trace
            .get("left_out")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    })
}

#[derive(Debug, Deserialize)]
struct TraceRow {
    trace: Option<String>,
}

/// `GET /agents/:id/chats/:chat/turns/:turn/trace`: what one of the
/// caller's replies did on the way.
pub async fn turn_trace(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, chat, turn)): Path<(String, String, String)>,
) -> Result<Json<TraceView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let row: TraceRow = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::TurnTrace {
            channel_id: session(&user, agent.id),
            conversation: chat_param(Some(chat))?.unwrap_or_default(),
            turn: chat_param(Some(turn))?.unwrap_or_default(),
        },
        true,
    )
    .await?;
    row.trace
        .as_deref()
        .and_then(trace_view)
        .map(Json)
        .ok_or(ApiError::NotFound)
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
    /// The chat's own model and thinking choice, when it has them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
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
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
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
    /// A new title.
    #[serde(default)]
    pub title: Option<String>,
    /// The chat's own model; `null` for the agent's (issue #190).
    #[serde(default, deserialize_with = "present")]
    pub model: Option<Option<String>>,
    /// The chat's own thinking choice; `null` for its model's default.
    #[serde(default, deserialize_with = "present")]
    pub thinking: Option<Option<String>>,
}

/// A field that is present, `null` included.
fn present<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
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
            trace: t.trace.and_then(|raw| serde_json::from_str(&raw).ok()),
        })
        .collect();
    Ok(Json(ConversationResponse {
        agent_id: agent_id.to_string(),
        channel_id,
        conversation: rows.conversation,
        turns: views,
        waiting: rows.waiting,
        model: rows.model,
        thinking: rows.thinking,
        uninitialised: false,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

/// Longest search the dashboard may send, in characters.
const MAX_SEARCH_CHARS: usize = 200;
/// Chats and turns a search returns at most, each.
const SEARCH_RESULTS: u32 = 30;

/// `GET /agents/:id/search?q=`: the caller's chats with the agent whose
/// titles or turns have every word of `q` (issue #175).
pub async fn search(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let query = q.q.trim();
    if query.is_empty() || query.chars().count() > MAX_SEARCH_CHARS {
        return Err(ApiError::BadRequest(format!(
            "q must be 1 to {MAX_SEARCH_CHARS} characters"
        )));
    }
    let found: serde_json::Value = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::SearchChats {
            channel_id: session(&user, agent.id),
            query: query.to_string(),
            limit: SEARCH_RESULTS,
        },
        true,
    )
    .await?;
    Ok(Json(found))
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
    let conversation = chat_param(Some(chat))?.unwrap_or_default();
    if req.model.is_some() || req.thinking.is_some() {
        // Only a model the agent may use; a thinking choice that reads as
        // one (the model decides whether it takes it).
        if let Some(Some(model)) = &req.model {
            let usable = crate::commands::usable_models(&state, agent.id)
                .await
                .unwrap_or_default();
            if !usable.iter().any(|m| &m.id == model) {
                return Err(ApiError::BadRequest(format!(
                    "{model:?} is not a model this agent may use"
                )));
            }
        }
        if let Some(Some(thinking)) = &req.thinking
            && !crate::commands::is_thinking_choice(thinking)
        {
            return Err(ApiError::BadRequest(format!(
                "{thinking:?} is not a thinking choice"
            )));
        }
        let _: Found = crate::api::agents::agent_data(
            &state,
            user.id,
            agent.id,
            havn_proto::DataQuery::ChatSettings {
                channel_id: session(&user, agent.id),
                conversation: conversation.clone(),
                model: req.model.clone(),
                thinking: req.thinking.clone(),
            },
            true,
        )
        .await?;
    }
    let Some(title) = req.title.as_deref().map(str::trim) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    if title.is_empty() {
        return Err(ApiError::BadRequest("title must be non-empty".into()));
    }
    let renamed: Found = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::RenameChat {
            channel_id: session(&user, agent.id),
            conversation,
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

#[cfg(test)]
mod trace_tests {
    use super::*;

    #[test]
    fn a_stored_trace_is_read_into_known_parts_only() {
        let raw = serde_json::json!({
            "ms": 1200,
            "left_out": 2,
            "items": [
                {"kind": "thinking", "text": "hmm"},
                {"kind": "step", "tool": "shell", "input": "{}", "result": "ok", "error": false},
                {"kind": "step", "tool": "shell", "input": {}, "result": "an object is not text"},
                {"kind": "html", "text": "<script>"},
                "not an object"
            ]
        })
        .to_string();
        let view = trace_view(&raw).expect("a trace");
        assert_eq!((view.ms, view.left_out, view.items.len()), (1200, 2, 2));
        assert!(trace_view("not json").is_none());
        assert!(trace_view(r#"{"ms": 1}"#).is_none(), "no items");
        let long = serde_json::json!({"ms": 1, "items": [{"kind": "thinking", "text": "x".repeat(MAX_TRACE_PART + 5)}]}).to_string();
        match &trace_view(&long).expect("a trace").items[0] {
            TraceItem::Thinking { text } => assert_eq!(text.len(), MAX_TRACE_PART),
            TraceItem::Step { .. } => panic!("a thinking part"),
        }
    }
}
