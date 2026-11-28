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
    /// A channel chat's scope (issue #184), one of the caller's linked
    /// senders; the caller's web chats when absent.
    pub scope: Option<String>,
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
        /// A subagent's own steps, for its call (issue #222).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        steps: Vec<TraceItem>,
        #[serde(default, skip_serializing_if = "is_zero")]
        left_out: u64,
    },
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// `item` with its texts within [`MAX_TRACE_PART`] and, at the top level,
/// a subagent's steps within [`MAX_TRACE_ITEMS`]; a subagent's own steps
/// have none.
fn bounded(item: TraceItem, top: bool) -> TraceItem {
    let cut = |s: String| -> String { s.chars().take(MAX_TRACE_PART).collect() };
    match item {
        TraceItem::Thinking { text } => TraceItem::Thinking { text: cut(text) },
        TraceItem::Step {
            tool,
            input,
            result,
            error,
            steps,
            left_out,
        } => TraceItem::Step {
            tool: cut(tool),
            input: cut(input),
            result: cut(result),
            error,
            steps: if top {
                steps
                    .into_iter()
                    .take(MAX_TRACE_ITEMS)
                    .map(|s| bounded(s, false))
                    .collect()
            } else {
                Vec::new()
            },
            left_out: if top { left_out } else { 0 },
        },
    }
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
    let items = trace
        .get("items")?
        .as_array()?
        .iter()
        .take(MAX_TRACE_ITEMS)
        .filter_map(|item| serde_json::from_value::<TraceItem>(item.clone()).ok())
        .map(|item| bounded(item, true))
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
    Query(q): Query<ScopeQuery>,
) -> Result<Json<TraceView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let row: TraceRow = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::TurnTrace {
            channel_id: scope_of(&state, &user, agent.id, q.scope.as_deref()).await?,
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

/// A deleted chat, and how many inbox files the runtime removed with it
/// (issue #322); an older runtime removes none.
#[derive(Deserialize)]
struct Deleted {
    found: bool,
    #[serde(default)]
    removed: usize,
}

#[derive(Debug, Deserialize)]
pub struct BranchSwitch {
    pub turn: String,
    /// A channel chat's scope, as in [`ConversationQuery::scope`].
    #[serde(default)]
    pub scope: Option<String>,
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

/// `?scope=`: a channel chat's scope, as in [`ConversationQuery::scope`].
#[derive(Debug, Deserialize)]
pub struct ScopeQuery {
    pub scope: Option<String>,
}

/// A channel chat scope of `user` with an agent (issue #184): a sender
/// they linked (issue #182), on an account whose messages reach the agent.
#[derive(Debug, Clone)]
pub struct LinkedScope {
    pub scope: String,
    pub channel: String,
    pub account_id: String,
    pub sender_id: String,
}

/// `user`'s channel chat scopes with `agent`. Only their own linked
/// senders, by their ids and the earlier ids whose link to them moved to
/// those (issue #262): other people's chats with the agent are not theirs
/// to see.
pub async fn linked_scopes(
    db: &sqlx::SqlitePool,
    bindings: &[crate::config::ChannelBindingConfig],
    user: havn_core::UserId,
    agent: havn_core::AgentId,
) -> Result<Vec<LinkedScope>, havn_db::DbError> {
    let links = havn_db::repo::channel_access::list_links(db, Some(user)).await?;
    let agent = agent.to_string();
    let mut scopes = Vec::new();
    for binding in bindings.iter().filter(|b| {
        b.agent_id == agent
            && crate::channel_router::agent_for_account(bindings, &b.channel, &b.account_id)
                == Some(agent.as_str())
    }) {
        for link in links.iter().filter(|l| l.channel == binding.channel) {
            let earlier = havn_db::repo::channel_access::aliases_linked_to(
                db,
                &binding.channel,
                &binding.account_id,
                &link.sender_id,
                user,
            )
            .await?;
            for sender_id in std::iter::once(link.sender_id.clone()).chain(earlier) {
                scopes.push(LinkedScope {
                    scope: crate::channel_id_codec::encode(
                        &binding.channel,
                        &binding.account_id,
                        &sender_id,
                    ),
                    channel: binding.channel.clone(),
                    account_id: binding.account_id.clone(),
                    sender_id,
                });
            }
        }
    }
    Ok(scopes)
}

/// The scope a request reads: `requested`, when it is one of the caller's
/// channel chat scopes with the agent, else their web chats.
async fn scope_of(
    state: &AppState,
    user: &AuthedUser,
    agent: havn_core::AgentId,
    requested: Option<&str>,
) -> Result<String, ApiError> {
    let Some(requested) = requested else {
        return Ok(session(user, agent));
    };
    linked_scopes(&state.db, &state.bindings, user.id, agent)
        .await?
        .into_iter()
        .find(|l| l.scope == requested)
        .map(|l| l.scope)
        .ok_or(ApiError::NotFound)
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
    let channel_id = scope_of(&state, &user, agent_id, q.scope.as_deref()).await?;
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

/// One of the caller's channel chats with the agent (issue #184).
#[derive(Debug, Serialize)]
pub struct ChannelChat {
    #[serde(flatten)]
    pub chat: Chat,
    pub channel: String,
    pub account_id: String,
    pub sender_id: String,
    /// Pass back as `scope` to read it, switch its versions or move it.
    pub scope: String,
}

/// `GET /agents/:id/channel-chats`: the caller's chats with the agent in
/// the chat apps they linked, most recently used first; read-only here.
pub async fn channel_chats(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<ChannelChat>>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let mut all = Vec::new();
    for linked in linked_scopes(&state.db, &state.bindings, user.id, agent.id).await? {
        let chats: Vec<Chat> = crate::api::agents::agent_data(
            &state,
            user.id,
            agent.id,
            havn_proto::DataQuery::Chats {
                channel_id: linked.scope.clone(),
                limit: MAX_CHATS,
            },
            true,
        )
        .await?;
        all.extend(chats.into_iter().map(|chat| ChannelChat {
            chat,
            channel: linked.channel.clone(),
            account_id: linked.account_id.clone(),
            sender_id: linked.sender_id.clone(),
            scope: linked.scope.clone(),
        }));
    }
    all.sort_by(|a, b| b.chat.updated_at.cmp(&a.chat.updated_at));
    Ok(Json(all))
}

#[derive(Debug, Deserialize)]
pub struct ChatMove {
    /// The channel chat's scope, from `channel-chats`.
    pub scope: String,
}

#[derive(Debug, Deserialize)]
struct Moved {
    moved: bool,
    busy: bool,
    was_active: bool,
    #[serde(default)]
    title: Option<String>,
}

/// What the chat app is told when its current chat moves to the web; an
/// earlier chat is named, so it is not taken for the current one.
const MOVED_NOTICE_EARLIER: &str =
    "Your earlier chat \"{title}\" continues in Havn's web chat now.";
const MOVED_NOTICE_NEW: &str =
    "This chat continues in Havn's web chat now. Your next message here starts a new chat.";

/// `POST /agents/:id/chats/:chat/move`: continue one of the caller's
/// channel chats on the web (issue #184). There is no way back: the chat
/// becomes one of their web chats, the channel's next message starts a new
/// chat if it was the channel's current one, and tasks made in it deliver
/// to the web. Refused while a reply runs or waits in it.
pub async fn chat_move(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, chat)): Path<(String, String)>,
    Json(req): Json<ChatMove>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let linked = linked_scopes(&state.db, &state.bindings, user.id, agent.id)
        .await?
        .into_iter()
        .find(|l| l.scope == req.scope)
        .ok_or(ApiError::NotFound)?;
    let conversation = chat_param(Some(chat))?.unwrap_or_default();
    let moved: Moved = crate::api::agents::agent_data(
        &state,
        user.id,
        agent.id,
        havn_proto::DataQuery::MoveChat {
            from: linked.scope.clone(),
            conversation: conversation.clone(),
            to: session(&user, agent.id),
        },
        true,
    )
    .await?;
    if moved.busy {
        return Err(ApiError::Conflict(
            "a reply is running or waiting in this chat; move it when it is done".into(),
        ));
    }
    if !moved.moved {
        return Err(ApiError::NotFound);
    }
    if let Err(e) =
        havn_db::repo::cron::redeliver_chat(&state.db, agent.id, &conversation, "webchat").await
    {
        tracing::warn!(error = %e, "moving a chat: its tasks still deliver to the chat app");
    }
    let notice = havn_proto::channel::ChannelMessageContent::Text {
        text: if moved.was_active {
            MOVED_NOTICE_NEW.to_string()
        } else {
            MOVED_NOTICE_EARLIER.replace(
                "{title}",
                moved
                    .title
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .unwrap_or("untitled"),
            )
        },
    };
    if let Err(e) = state
        .channel_router
        .deliver(
            &linked.channel,
            &linked.account_id,
            &linked.sender_id,
            None,
            notice,
        )
        .await
    {
        tracing::warn!(error = %e, "moving a chat: the chat app was not told");
    }
    crate::audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "chat.moved_to_web",
        serde_json::json!({ "chat": conversation, "channel": linked.channel, "account_id": linked.account_id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
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
    let title = req.title.as_deref().map(str::trim);
    if title.is_some_and(str::is_empty) {
        return Err(ApiError::BadRequest("title must be non-empty".into()));
    }
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
        let set: Found = crate::api::agents::agent_data(
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
        if !set.found {
            return Err(ApiError::NotFound);
        }
    }
    let Some(title) = title else {
        return Ok(StatusCode::NO_CONTENT);
    };
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
            channel_id: scope_of(&state, &user, agent.id, req.scope.as_deref()).await?,
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
    let deleted: Deleted = crate::api::agents::agent_data(
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
    // The files it carried go with it (issue #229): those the person sent,
    // which the runtime removed as it told that no other chat names them
    // (issue #322), and those the agent sent.
    let root = state.workspace_root.clone();
    let (agent_id, channel, chat) = (agent.id, session(&user, agent.id), conversation.clone());
    let sent =
        tokio::task::spawn_blocking(move || forget_chat_files(&root, agent_id, &channel, &chat))
            .await
            .unwrap_or(0);
    let named = deleted.removed + sent;
    crate::audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.chat.deleted",
        serde_json::json!({ "conversation": conversation, "files_named": named }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove what chat `chat` of conversation `channel` of agent `agent` left
/// with the gateway: the files the agent sent to it, and the images its
/// tools showed in it (issue #354). How many files. Blocking.
fn forget_chat_files(
    workspace_root: &std::path::Path,
    agent: havn_core::AgentId,
    channel: &str,
    chat: &str,
) -> usize {
    crate::llm_proxy::KeptImages::of(workspace_root, agent).forget(channel, chat);
    let outbox = crate::outbox::dir(workspace_root, agent);
    let sent = crate::outbox::list(&outbox, Some(chat), usize::MAX);
    for file in &sent {
        crate::outbox::remove(&outbox, &file.id);
    }
    sent.len()
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

    #[test]
    fn a_subagent_s_steps_come_with_its_call_one_level_deep() {
        let raw = serde_json::json!({"ms": 1, "items": [{
            "kind": "step", "tool": "subagent_spawn", "input": "{}", "result": "done",
            "left_out": 3,
            "steps": [
                {"kind": "thinking", "text": "plan"},
                {"kind": "step", "tool": "shell", "input": "{}", "result": "ok",
                 "steps": [{"kind": "thinking", "text": "deeper"}], "left_out": 9},
            ],
        }]})
        .to_string();
        let view = trace_view(&raw).expect("a trace");
        let TraceItem::Step {
            steps, left_out, ..
        } = &view.items[0]
        else {
            panic!("a step");
        };
        assert_eq!((steps.len(), *left_out), (2, 3));
        let TraceItem::Step {
            steps: deeper,
            left_out: more,
            ..
        } = &steps[1]
        else {
            panic!("a step");
        };
        assert!(
            deeper.is_empty() && *more == 0,
            "nothing below a subagent's steps"
        );
        let shown = serde_json::to_value(&view.items[0]).expect("json");
        assert!(shown["steps"][1].get("steps").is_none());
    }
}

#[cfg(test)]
mod scope_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use super::*;
    use havn_db::repo::channel_access;

    #[tokio::test]
    async fn only_the_callers_own_linked_senders_are_their_channel_chats() {
        let (mut state, owner) = crate::auth::tests::state_with(false).await;
        let agent = havn_db::repo::agents::create(
            &state.db,
            havn_db::repo::agents::NewAgent {
                owner_id: owner,
                team_id: None,
                name: "bound",
                config: serde_json::json!({}),
            },
        )
        .await
        .unwrap()
        .id;
        let other_agent = havn_core::AgentId::new();
        state.bindings = Arc::new(vec![
            crate::config::ChannelBindingConfig {
                agent_id: agent.to_string(),
                channel: "telegram".into(),
                account_id: "bot".into(),
            },
            // Its messages go to another agent: not this agent's chats.
            crate::config::ChannelBindingConfig {
                agent_id: other_agent.to_string(),
                channel: "telegram".into(),
                account_id: "other".into(),
            },
            crate::config::ChannelBindingConfig {
                agent_id: agent.to_string(),
                channel: "telegram".into(),
                account_id: "other".into(),
            },
        ]);
        let someone = havn_db::repo::users::create(
            &state.db,
            havn_db::repo::users::NewUser {
                display_name: "Someone",
            },
        )
        .await
        .unwrap()
        .id;
        for (user, sender) in [(owner, "tg:mine"), (someone, "tg:theirs")] {
            let code = channel_access::create_link_code(&state.db, "telegram", "bot", user)
                .await
                .unwrap();
            channel_access::redeem(
                &state.db,
                "telegram",
                "bot",
                sender,
                &code.code,
                Utc::now(),
                false,
            )
            .await
            .unwrap()
            .unwrap();
        }
        let me = AuthedUser {
            id: owner,
            display_name: "owner".into(),
        };
        let scopes = linked_scopes(&state.db, &state.bindings, me.id, agent)
            .await
            .unwrap();
        let mine = crate::channel_id_codec::encode("telegram", "bot", "tg:mine");
        assert_eq!(
            scopes.iter().map(|s| s.scope.as_str()).collect::<Vec<_>>(),
            [mine.as_str()]
        );
        assert_eq!(
            scope_of(&state, &me, agent, Some(&mine)).await.unwrap(),
            mine
        );
        let theirs = crate::channel_id_codec::encode("telegram", "bot", "tg:theirs");
        assert!(matches!(
            scope_of(&state, &me, agent, Some(&theirs)).await,
            Err(ApiError::NotFound)
        ));
        assert_eq!(
            scope_of(&state, &me, agent, None).await.unwrap(),
            session(&me, agent)
        );
    }

    #[test]
    fn a_deleted_chat_takes_the_files_sent_to_it_and_the_images_shown_in_it() {
        let root = tempfile::tempdir().unwrap();
        let agent = havn_core::AgentId::new();
        let ws = root.path().join(agent.to_string()).join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("report.txt"), b"report").unwrap();
        std::fs::write(ws.join("chart.png"), b"\x89PNG\r\n\x1a\nchart").unwrap();
        let outbox = crate::outbox::dir(root.path(), agent);
        for chat in ["c1", "c2"] {
            crate::outbox::send(&ws, &outbox, "report.txt", None, "web", Some(chat)).unwrap();
        }
        let kept = crate::llm_proxy::KeptImages::of(root.path(), agent);
        let show = |key: &str, chat: &str| {
            let mut request: crate::llm_proxy::AnthropicRequest =
                serde_json::from_value(serde_json::json!({
                    "model": "m",
                    "max_tokens": 10,
                    "messages": [{"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call_0", "content": [
                            {"type": "image", "source": {
                                "type": "workspace", "path": "chart.png", "key": key,
                                "channel": "web", "chat": chat,
                            }},
                        ]},
                    ]}],
                }))
                .unwrap();
            crate::llm_proxy::resolve_workspace_images(
                &mut request,
                &ws,
                true,
                &Default::default(),
                &|_| Err(String::new()),
                Some(&kept),
            );
            request.messages[0].content[0]["content"][0].clone()
        };
        show("k1", "c1");
        show("k2", "c2");
        let files = |dir: &std::path::Path| std::fs::read_dir(dir).unwrap().count();
        assert_eq!(files(&kept.dir), 4, "two images, each with its mark");

        assert_eq!(forget_chat_files(root.path(), agent, "web", "c1"), 1);
        assert_eq!(files(&kept.dir), 2, "the other chat's image stays");
        assert_eq!(crate::outbox::list(&outbox, None, 10).len(), 1);
        // Its image is gone from the gateway; the other chat's is shown on.
        std::fs::remove_file(ws.join("chart.png")).unwrap();
        assert_eq!(show("k2", "c2")["type"], "image");
        let gone = show("k1", "c1");
        assert!(
            gone["text"]
                .as_str()
                .is_some_and(|t| t.contains("removed before it could be shown")),
            "{gone}"
        );
    }
}
