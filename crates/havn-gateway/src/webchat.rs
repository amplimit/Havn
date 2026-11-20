//! In-memory router for `WebSocket` `WebChat` sessions.
//!
//! Each open browser `WebSocket` is one tab of a session; the `SessionId` is
//! used as the `channel_id` in inbound/outbound messages and is shared by
//! every tab the same user has open on the same agent. When an agent
//! returns an `OutboundMessage`, the gateway pushes the reply onto the mpsc
//! of every tab of the matching session; each tab's writer task drains its
//! mpsc onto its `WebSocket`.
//!
//! WebChat is havn's built-in console (spec §3.6), not a channel adapter.
//! External channel adapters (Telegram, Slack, …) speak the HTTP+WS protocol
//! at `/api/v1/channel` and never touch this router.

use std::collections::HashMap;
use std::sync::Arc;

use havn_core::{AgentId, MessageContent, OutboundMessage};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::warn;
use uuid::Uuid;

pub type SessionId = Uuid;

/// One open `WebSocket`.
pub type TabId = Uuid;

/// Longest chat id a tab may name.
pub const MAX_CONVERSATION_ID: usize = 64;

/// The dashboard session of user `sender_id` with `agent_id`: the same on
/// every reconnect, so its chats stay reachable.
#[must_use]
pub fn session_for(sender_id: &str, agent_id: AgentId) -> SessionId {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("havn-webchat:{sender_id}:{agent_id}").as_bytes(),
    )
}

#[derive(Debug, Clone, Default)]
pub struct WebChatRouter {
    inner: Arc<Mutex<HashMap<TabId, Tab>>>,
    /// Tool calls waiting for the owner's approval, by request id; shown
    /// to every tab of the agent, also ones opened later.
    approvals: Arc<std::sync::Mutex<HashMap<String, PendingApproval>>>,
    /// Requests the gateway itself asked (the egress proxy), answered
    /// here instead of by an agent.
    gateway_waiters:
        Arc<std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<GatewayAnswer>>>>,
    /// When chat commands were sent to an agent for a session and not
    /// answered yet: an agent's notice is shown only as such an answer.
    command_replies: Arc<std::sync::Mutex<HashMap<(AgentId, SessionId), Vec<std::time::Instant>>>>,
    /// Questions agents asked with `ask_choice` (issue #204), by id, until
    /// answered or [`crate::choice::KEPT`] passes.
    choices: Arc<std::sync::Mutex<HashMap<String, PendingChoice>>>,
}

#[derive(Debug, Clone)]
struct PendingChoice {
    agent_id: AgentId,
    channel_id: String,
    asked: crate::choice::Asked,
    since: std::time::Instant,
}

/// Questions one conversation, and one agent, may have waiting; past
/// that, the oldest goes.
const MAX_CHOICES_WAITING: usize = 20;
const MAX_AGENT_CHOICES_WAITING: usize = 1000;

/// Unanswered chat commands kept per session; more are not sent.
const MAX_COMMANDS_WAITING: usize = 16;
/// How long a command's answer is awaited: an agent that restarted or
/// crashed meanwhile never sends it.
const COMMAND_REPLY_KEPT: std::time::Duration = std::time::Duration::from_secs(60);

/// The owner's answer to a question of the gateway's own: allow, always,
/// and who decided.
pub type GatewayAnswer = (bool, bool, String);

#[derive(Debug, Clone)]
struct PendingApproval {
    agent_id: AgentId,
    request: havn_proto::ApprovalRequest,
    since: std::time::Instant,
}

/// Approval requests one agent may have waiting.
pub const MAX_PENDING_APPROVALS: usize = 16;
/// Longer than the runtime waits, which then closes the request itself.
const APPROVAL_KEPT: std::time::Duration = std::time::Duration::from_secs(11 * 60);

#[derive(Debug)]
struct Tab {
    session_id: SessionId,
    /// Agent the session is bound to. Only that agent's messages are
    /// delivered to it.
    agent_id: AgentId,
    tx: mpsc::Sender<ServerMessage>,
    closed: Arc<Notify>,
}

/// A registered tab. `closed` fires when the router drops the tab because
/// it stopped reading; its handler then closes the socket.
#[derive(Debug)]
pub struct Registration {
    pub tab: TabId,
    pub closed: Arc<Notify>,
}

/// Wire types over the WebSocket frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// User-typed message, in chat `conversation` (a new one when the id
    /// is new), or the active chat when absent; with `edit`, a new
    /// version of that user turn of the chat.
    Send {
        content: String,
        #[serde(default)]
        conversation: Option<String>,
        #[serde(default)]
        edit: Option<String>,
        /// Files uploaded to the agent's inbox for this message.
        #[serde(default)]
        attachments: Vec<havn_core::Attachment>,
    },
    /// Another reply in place of reply `turn` of the chat, or of its last.
    Regenerate {
        #[serde(default)]
        conversation: Option<String>,
        #[serde(default)]
        turn: Option<String>,
    },
    /// Stop the reply the agent is working on in a chat of this session.
    Stop {
        #[serde(default)]
        conversation: Option<String>,
    },
    /// Answer a [`ServerMessage::Approval`].
    Decide {
        id: String,
        approve: bool,
        #[serde(default)]
        remember: bool,
        /// What the owner adds to an approval of memory (issue #249).
        #[serde(default)]
        note: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Reply from the agent (assistant turn).
    AgentMessage {
        content: String,
        /// Sent to every tab on its own (heartbeat, cron), not a reply to
        /// a message: the dashboard does not count it as one.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        broadcast: bool,
        /// The chat it answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        /// Buttons under it (issue #204): a press sends the button's data
        /// as the next message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        buttons: Vec<Vec<havn_core::Button>>,
        /// A question of several answers: the chosen numbers go after its
        /// `answer` as the next message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        poll: Option<havn_core::Poll>,
    },
    /// Soft error visible to the user (e.g. agent disconnected).
    Error { message: String },
    /// A tool call waits for the owner's approval.
    Approval {
        id: String,
        tool: String,
        /// The whole input, as JSON.
        input: String,
        reason: String,
        /// The conversation it came from, when not this chat.
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        /// `network` for the egress proxy's questions about a domain:
        /// "always" then adds it to the agent's allowed domains. `memory`
        /// for what memory_search found for someone else (issue #249):
        /// "always" allows it in that conversation, and a note says what
        /// not to say.
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    /// The approval request `id` was decided or ended; stop showing it.
    ApprovalClosed { id: String },
    /// A model titled chat `conversation`.
    ChatTitle { conversation: String, title: String },
    /// The answer to a chat command (`/status`, `/undo`, ...): shown in
    /// the chat, not kept in it, and not a reply to a message.
    Notice {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
    },
    /// A file the agent sent, to download from `url` (relative to the
    /// gateway's API).
    File {
        url: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
    },
    /// The text of a model call of the reply being written, as it
    /// arrives: `text` goes after the first `at` UTF-16 units of what the
    /// call showed, and with `at` 0 is all of it. A tab whose text is not
    /// `at` long waits for the next whole. The reply replaces it all.
    ReplyText {
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        /// The model call it comes from.
        call: String,
        text: String,
        at: usize,
        /// The call's reasoning (issue #178), not its reply: shown apart,
        /// and the reply replaces it too.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        thinking: bool,
    },
    /// The tool call the reply being written is about to make.
    Step {
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        tool: String,
        detail: String,
    },
    /// What the reply just sent saved to or forgot from memory.
    Memory {
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
        /// The reply's stored turn: the note goes under it.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn: Option<String>,
        changes: Vec<havn_proto::MemoryChange>,
    },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeliverError {
    #[error("channel_id {0:?} is not a valid webchat session id")]
    InvalidChannelId(String),
    #[error("no webchat session matches {0}")]
    NoSession(SessionId),
    #[error("session writer closed")]
    WriterClosed,
    #[error("webchat session {0} is not reading; closed it")]
    Stalled(SessionId),
}

impl WebChatRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tab to `session_id`. Other open tabs of the session stay
    /// registered and keep getting replies.
    pub async fn register(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        tx: mpsc::Sender<ServerMessage>,
    ) -> Registration {
        let tab = Uuid::now_v7();
        let closed = Arc::new(Notify::new());
        // Approvals already waiting reach a tab opened later, or reloaded.
        for request in self.pending_approvals(agent_id) {
            let _ = tx.try_send(approval_message(&request));
        }
        self.inner.lock().await.insert(
            tab,
            Tab {
                session_id,
                agent_id,
                tx,
                closed: Arc::clone(&closed),
            },
        );
        Registration { tab, closed }
    }

    fn lock_approvals(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pending_approvals(&self, agent_id: AgentId) -> Vec<havn_proto::ApprovalRequest> {
        let mut pending = self.lock_approvals();
        pending.retain(|_, p| p.since.elapsed() < APPROVAL_KEPT);
        pending
            .values()
            .filter(|p| p.agent_id == agent_id)
            .map(|p| p.request.clone())
            .collect()
    }

    /// Keep `request` and show it to every tab of `agent_id`. False, and
    /// not kept, when the agent already has [`MAX_PENDING_APPROVALS`]
    /// waiting or the id is taken.
    pub async fn ask_approval(
        &self,
        agent_id: AgentId,
        request: havn_proto::ApprovalRequest,
    ) -> bool {
        {
            let mut pending = self.lock_approvals();
            pending.retain(|_, p| p.since.elapsed() < APPROVAL_KEPT);
            let waiting = pending.values().filter(|p| p.agent_id == agent_id).count();
            // One question about memory per conversation at a time (issue
            // #249): someone else cannot fill the owner's slots.
            let memory_waits = request.kind == "memory"
                && pending.values().any(|p| {
                    p.agent_id == agent_id
                        && p.request.kind == "memory"
                        && p.request.channel_id == request.channel_id
                });
            if waiting >= MAX_PENDING_APPROVALS
                || memory_waits
                || pending.contains_key(&request.request_id)
            {
                return false;
            }
            pending.insert(
                request.request_id.clone(),
                PendingApproval {
                    agent_id,
                    request: request.clone(),
                    since: std::time::Instant::now(),
                },
            );
        }
        self.fan_out(agent_id, approval_message(&request)).await;
        true
    }

    /// Ask the owner a question of the gateway's own; the answer comes on
    /// the receiver. `None` when it cannot be asked (too many waiting).
    pub async fn ask_gateway_approval(
        &self,
        agent_id: AgentId,
        request: havn_proto::ApprovalRequest,
    ) -> Option<tokio::sync::oneshot::Receiver<GatewayAnswer>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = request.request_id.clone();
        self.lock_waiters().insert(id.clone(), tx);
        if self.ask_approval(agent_id, request).await {
            Some(rx)
        } else {
            self.lock_waiters().remove(&id);
            None
        }
    }

    /// The gateway's own waiter for request `id`, if it asked it.
    pub fn take_gateway_waiter(
        &self,
        id: &str,
    ) -> Option<tokio::sync::oneshot::Sender<GatewayAnswer>> {
        self.lock_waiters().remove(id)
    }

    fn lock_waiters(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, tokio::sync::oneshot::Sender<GatewayAnswer>>>
    {
        self.gateway_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take the request `id` of `agent_id` out, and close it in every tab.
    pub async fn close_approval(
        &self,
        agent_id: AgentId,
        id: &str,
    ) -> Option<havn_proto::ApprovalRequest> {
        let taken = {
            let mut pending = self.lock_approvals();
            match pending.get(id) {
                Some(p) if p.agent_id == agent_id => pending.remove(id).map(|p| p.request),
                _ => None,
            }
        };
        if taken.is_some() {
            self.fan_out(
                agent_id,
                ServerMessage::ApprovalClosed { id: id.to_string() },
            )
            .await;
        }
        taken
    }

    pub async fn unregister(&self, tab: TabId) {
        self.inner.lock().await.remove(&tab);
    }

    /// Forward an `OutboundMessage` from `agent_id` to every open tab of its
    /// originating `WebChat` session. `channel_id` MUST be the session id
    /// (set by the WS handler when it constructed the `InboundMessage`).
    /// Sessions of other agents are reported as missing. Succeeds when at
    /// least one tab took the message.
    pub async fn deliver(
        &self,
        agent_id: AgentId,
        msg: OutboundMessage,
    ) -> Result<(), DeliverError> {
        let session_id: SessionId = msg
            .channel_id
            .parse()
            .map_err(|_| DeliverError::InvalidChannelId(msg.channel_id.clone()))?;
        let conversation = msg.conversation;
        let server_msg = match msg.content {
            MessageContent::Text { text } => ServerMessage::AgentMessage {
                content: text,
                broadcast: false,
                conversation,
                buttons: msg.buttons,
                poll: msg.poll,
            },
            MessageContent::File {
                url,
                filename,
                caption,
            } => ServerMessage::File {
                url,
                name: filename,
                caption,
                conversation,
            },
            other => ServerMessage::AgentMessage {
                content: format!("(unsupported content variant: {other:?})"),
                broadcast: false,
                conversation,
                buttons: Vec::new(),
                poll: None,
            },
        };
        self.send_to_session(agent_id, session_id, server_msg).await
    }

    /// Show the tabs of `session_id` the text a model call of the reply
    /// being written has added; a tab with no room for it misses it.
    #[allow(
        clippy::too_many_arguments,
        reason = "the frame's fields; a struct for one caller would only rename them"
    )]
    pub async fn reply_text(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        conversation: Option<String>,
        call: &str,
        text: String,
        at: usize,
        thinking: bool,
    ) -> Result<(), DeliverError> {
        self.show_to_session(
            agent_id,
            session_id,
            ServerMessage::ReplyText {
                conversation,
                call: call.to_string(),
                text,
                at,
                thinking,
            },
        )
        .await
    }

    /// Show the tabs of the session a turn answers the tool call it is
    /// about to make. Ignored for conversations that are not webchat
    /// sessions.
    pub async fn step(
        &self,
        agent_id: AgentId,
        step: havn_proto::Step,
    ) -> Result<(), DeliverError> {
        let session_id: SessionId = step
            .channel_id
            .parse()
            .map_err(|_| DeliverError::InvalidChannelId(step.channel_id.clone()))?;
        let line = |text: &str, max: usize| -> String {
            text.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(max)
                .collect()
        };
        self.show_to_session(
            agent_id,
            session_id,
            ServerMessage::Step {
                conversation: step.conversation.filter(|c| c.len() <= MAX_CONVERSATION_ID),
                tool: line(&step.tool, 64),
                detail: line(&step.detail, havn_proto::MAX_STEP_DETAIL_CHARS + 1),
            },
        )
        .await
    }

    /// Show the tabs of the session a reply went to what its turn changed
    /// in memory. Ignored for conversations that are not webchat sessions
    /// (a channel's sender).
    pub async fn memory_changed(
        &self,
        agent_id: AgentId,
        changed: havn_proto::MemoryChanged,
    ) -> Result<(), DeliverError> {
        let session_id: SessionId = changed
            .channel_id
            .parse()
            .map_err(|_| DeliverError::InvalidChannelId(changed.channel_id.clone()))?;
        let changes = changed
            .changes
            .into_iter()
            .take(havn_proto::MAX_MEMORY_CHANGES)
            .map(|mut change| {
                change.key = change
                    .key
                    .chars()
                    .take(havn_proto::MAX_MEMORY_KEY_CHARS)
                    .collect();
                change.value = change
                    .value
                    .chars()
                    .take(havn_proto::MAX_MEMORY_CHANGE_CHARS)
                    .collect();
                change.kind = change.kind.chars().take(32).collect();
                // Row ids are UUIDs; anything else cannot be undone.
                let row = |id: Option<String>| id.filter(|id| id.len() <= 64);
                change.before = row(change.before);
                change.after = row(change.after);
                change
            })
            .collect();
        let turn = changed.turn.filter(|t| t.len() <= 64);
        let conversation = changed
            .conversation
            .filter(|c| c.len() <= MAX_CONVERSATION_ID);
        self.send_to_session(
            agent_id,
            session_id,
            ServerMessage::Memory {
                conversation,
                turn,
                changes,
            },
        )
        .await
    }

    /// Tell the tabs of `session_id` a model titled chat `conversation`.
    pub async fn chat_titled(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        conversation: String,
        title: String,
    ) -> Result<(), DeliverError> {
        self.show_to_session(
            agent_id,
            session_id,
            ServerMessage::ChatTitle {
                conversation,
                title,
            },
        )
        .await
    }

    /// Show the tabs of `session_id` the answer to a chat command.
    pub async fn command_notice(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        conversation: Option<String>,
        content: String,
    ) -> Result<(), DeliverError> {
        self.send_to_session(
            agent_id,
            session_id,
            ServerMessage::Notice {
                content,
                conversation,
            },
        )
        .await
    }

    /// A chat command went to `agent_id` for `session_id`: its answer is
    /// awaited. False when too many are awaited already.
    pub fn expect_command_reply(&self, agent_id: AgentId, session_id: SessionId) -> bool {
        let mut waiting = self
            .command_replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        waiting.retain(|_, sent| {
            sent.retain(|at| at.elapsed() < COMMAND_REPLY_KEPT);
            !sent.is_empty()
        });
        let sent = waiting.entry((agent_id, session_id)).or_default();
        if sent.len() >= MAX_COMMANDS_WAITING {
            return false;
        }
        sent.push(std::time::Instant::now());
        true
    }

    /// Whether an answer of `agent_id` to a chat command of `session_id`
    /// was awaited; it no longer is.
    pub fn take_command_reply(&self, agent_id: AgentId, session_id: SessionId) -> bool {
        let mut waiting = self
            .command_replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (agent_id, session_id);
        let Some(sent) = waiting.get_mut(&key) else {
            return false;
        };
        sent.retain(|at| at.elapsed() < COMMAND_REPLY_KEPT);
        let awaited = !sent.is_empty();
        if awaited {
            sent.remove(0);
        }
        if sent.is_empty() {
            waiting.remove(&key);
        }
        awaited
    }

    /// Keep `asked`, a question of `agent_id` in conversation `channel_id`:
    /// its id. The oldest of the conversation's, or the agent's, goes when
    /// too many wait.
    pub fn ask_choice(
        &self,
        agent_id: AgentId,
        channel_id: String,
        asked: crate::choice::Asked,
    ) -> String {
        let mut choices = self
            .choices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        choices.retain(|_, c| c.since.elapsed() < crate::choice::KEPT);
        for (cap, same) in [
            (
                MAX_CHOICES_WAITING,
                Box::new(|c: &PendingChoice| c.agent_id == agent_id && c.channel_id == channel_id)
                    as Box<dyn Fn(&PendingChoice) -> bool>,
            ),
            (
                MAX_AGENT_CHOICES_WAITING,
                Box::new(|c: &PendingChoice| c.agent_id == agent_id),
            ),
        ] {
            while choices.values().filter(|c| same(c)).count() >= cap {
                let oldest = choices
                    .iter()
                    .filter(|(_, c)| same(c))
                    .min_by_key(|(_, c)| c.since)
                    .map(|(id, _)| id.clone());
                match oldest {
                    Some(id) => choices.remove(&id),
                    None => break,
                };
            }
        }
        // The random end of a v7 id (its start is the time).
        let id = Uuid::now_v7().simple().to_string()[20..].to_string();
        choices.insert(
            id.clone(),
            PendingChoice {
                agent_id,
                channel_id,
                asked,
                since: std::time::Instant::now(),
            },
        );
        id
    }

    /// Forget question `id`: it could not be asked, or it was answered.
    pub fn forget_choice(&self, id: &str) {
        self.choices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// The answer `numbers` to question `id` of `agent_id`, asked in
    /// conversation `channel_id`, as the person's message to the agent.
    /// The question waits until [`Self::forget_choice`], once the answer
    /// reached the agent; a wrong answer leaves it waiting.
    pub fn answer_choice(
        &self,
        agent_id: AgentId,
        channel_id: &str,
        id: &str,
        numbers: &[usize],
        language: havn_core::Language,
    ) -> Result<String, String> {
        let mut choices = self
            .choices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        choices.retain(|_, c| c.since.elapsed() < crate::choice::KEPT);
        let Some(pending) = choices
            .get(id)
            .filter(|c| c.agent_id == agent_id && c.channel_id == channel_id)
        else {
            return Err(language
                .pick(
                    "(That question was already answered, or it expired.)",
                    "（那个问题已经回答过了，或者已经过期。）",
                )
                .into());
        };
        crate::choice::chosen(&pending.asked, numbers, language)
    }

    /// Approval request `id` of `agent_id`, when it waits in conversation
    /// `channel_id`.
    pub fn approval(
        &self,
        agent_id: AgentId,
        channel_id: &str,
        id: &str,
    ) -> Option<havn_proto::ApprovalRequest> {
        let mut pending = self.lock_approvals();
        pending.retain(|_, p| p.since.elapsed() < APPROVAL_KEPT);
        pending
            .values()
            .find(|p| {
                p.agent_id == agent_id
                    && p.request.channel_id == channel_id
                    && p.request.request_id == id
            })
            .map(|p| p.request.clone())
    }

    /// The approval of memory `id` of `agent_id`, in whichever conversation
    /// it waits: the owner decides it from their own (issue #249).
    pub fn memory_approval(
        &self,
        agent_id: AgentId,
        id: &str,
    ) -> Option<havn_proto::ApprovalRequest> {
        let mut pending = self.lock_approvals();
        pending.retain(|_, p| p.since.elapsed() < APPROVAL_KEPT);
        pending
            .values()
            .find(|p| {
                p.agent_id == agent_id && p.request.request_id == id && p.request.kind == "memory"
            })
            .map(|p| p.request.clone())
    }

    /// The approval request of `agent_id` that has waited longest in
    /// conversation `channel_id`.
    pub fn oldest_approval(
        &self,
        agent_id: AgentId,
        channel_id: &str,
    ) -> Option<havn_proto::ApprovalRequest> {
        let mut pending = self.lock_approvals();
        pending.retain(|_, p| p.since.elapsed() < APPROVAL_KEPT);
        pending
            .values()
            .filter(|p| p.agent_id == agent_id && p.request.channel_id == channel_id)
            .min_by_key(|p| p.since)
            .map(|p| p.request.clone())
    }

    /// Show `message` as an error in every open tab of `session_id`.
    pub async fn error(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        message: String,
    ) -> Result<(), DeliverError> {
        self.send_to_session(agent_id, session_id, ServerMessage::Error { message })
            .await
    }

    /// Offer `server_msg` to the tabs of the session whose queue is at
    /// least half free, for what only shows progress: no tab is closed for
    /// it, nor filled so that the next reply would close it.
    async fn show_to_session(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        server_msg: ServerMessage,
    ) -> Result<(), DeliverError> {
        let targets: Vec<_> = {
            let map = self.inner.lock().await;
            map.values()
                .filter(|t| t.agent_id == agent_id && t.session_id == session_id)
                .map(|t| t.tx.clone())
                .collect()
        };
        if targets.is_empty() {
            return Err(DeliverError::NoSession(session_id));
        }
        // Half of each queue stays free for what must arrive.
        for tx in targets {
            if tx.capacity() * 2 >= tx.max_capacity() {
                let _ = tx.try_send(server_msg.clone());
            }
        }
        Ok(())
    }

    async fn send_to_session(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
        server_msg: ServerMessage,
    ) -> Result<(), DeliverError> {
        let targets: Vec<_> = {
            let map = self.inner.lock().await;
            map.iter()
                .filter(|(_, t)| t.agent_id == agent_id && t.session_id == session_id)
                .map(|(id, t)| (*id, t.tx.clone()))
                .collect()
        };
        if targets.is_empty() {
            return Err(DeliverError::NoSession(session_id));
        }

        let mut result = Err(DeliverError::WriterClosed);
        for (tab, tx) in targets {
            match self.offer(session_id, tab, &tx, server_msg.clone()).await {
                Ok(()) => result = Ok(()),
                Err(e) if result.is_err() => result = Err(e),
                Err(_) => {}
            }
        }
        result
    }

    /// Queue `msg` without waiting. The caller is an agent's reader loop, so
    /// a browser that stops reading must not hold it up: its tab is dropped
    /// instead and told to close its socket.
    async fn offer(
        &self,
        session_id: SessionId,
        tab: TabId,
        tx: &mpsc::Sender<ServerMessage>,
        msg: ServerMessage,
    ) -> Result<(), DeliverError> {
        match tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(DeliverError::WriterClosed),
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Some(dropped) = self.inner.lock().await.remove(&tab) {
                    dropped.closed.notify_one();
                }
                warn!(%session_id, %tab, "webchat tab outbound queue is full; closing the tab");
                Err(DeliverError::Stalled(session_id))
            }
        }
    }

    #[allow(dead_code, reason = "consumed by upcoming dashboard endpoints")]
    pub async fn agent_for_session(&self, session_id: SessionId) -> Option<AgentId> {
        self.inner
            .lock()
            .await
            .values()
            .find(|t| t.session_id == session_id)
            .map(|t| t.agent_id)
    }

    #[cfg(test)]
    pub(crate) async fn tab_count(&self, session_id: SessionId) -> usize {
        self.inner
            .lock()
            .await
            .values()
            .filter(|t| t.session_id == session_id)
            .count()
    }

    /// Fan-out a proactive `OutboundMessage` (e.g. heartbeat output) to every
    /// open `WebChat` tab bound to `agent_id`. Returns the number of tabs the
    /// message was successfully queued onto. Closed senders are skipped
    /// silently — the per-tab writer task already handles that path — and
    /// tabs whose queue is full are dropped (see [`Self::offer`]).
    pub async fn broadcast(&self, agent_id: AgentId, msg: OutboundMessage) -> usize {
        let conversation = msg.conversation;
        let server_msg = match msg.content {
            MessageContent::Text { text } => ServerMessage::AgentMessage {
                content: text,
                broadcast: true,
                conversation,
                buttons: Vec::new(),
                poll: None,
            },
            other => ServerMessage::AgentMessage {
                content: format!("(unsupported content variant: {other:?})"),
                broadcast: true,
                conversation,
                buttons: Vec::new(),
                poll: None,
            },
        };
        self.fan_out(agent_id, server_msg).await
    }

    /// Show `message` as an error in every open tab of `agent_id`.
    pub async fn notice(&self, agent_id: AgentId, message: &str) -> usize {
        let server_msg = ServerMessage::Error {
            message: message.to_owned(),
        };
        self.fan_out(agent_id, server_msg).await
    }

    async fn fan_out(&self, agent_id: AgentId, server_msg: ServerMessage) -> usize {
        let targets: Vec<_> = {
            let map = self.inner.lock().await;
            map.iter()
                .filter(|(_, t)| t.agent_id == agent_id)
                .map(|(id, t)| (t.session_id, *id, t.tx.clone()))
                .collect()
        };
        let mut delivered = 0usize;
        for (session_id, tab, tx) in targets {
            if self
                .offer(session_id, tab, &tx, server_msg.clone())
                .await
                .is_ok()
            {
                delivered += 1;
            }
        }
        delivered
    }
}

fn approval_message(request: &havn_proto::ApprovalRequest) -> ServerMessage {
    ServerMessage::Approval {
        id: request.request_id.clone(),
        tool: request.tool.clone(),
        input: request.input.clone(),
        reason: request.reason.clone(),
        conversation: (!request.channel_id.is_empty()
            && request.channel_id.parse::<SessionId>().is_err())
        .then(|| request.channel_id.clone()),
        kind: if request
            .request_id
            .starts_with(crate::egress_proxy::REQUEST_PREFIX)
        {
            Some("network".to_string())
        } else {
            (!request.kind.is_empty()).then(|| request.kind.clone())
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn approval_request(id: &str) -> havn_proto::ApprovalRequest {
        havn_proto::ApprovalRequest {
            request_id: id.into(),
            channel_id: "ch|telegram|bot|42".into(),
            tool: "shell".into(),
            input: "{}".into(),
            reason: "rule".into(),
            kind: String::new(),
        }
    }

    #[test]
    fn a_question_is_answered_in_its_conversation_until_forgotten() {
        let router = WebChatRouter::new();
        let agent = AgentId::new();
        let asked = crate::choice::read(&serde_json::json!({
            "question": "Which?", "options": ["a", "b"]
        }))
        .expect("asked");
        let chat = "ch|telegram|bot|tg:1";
        let id = router.ask_choice(agent, chat.into(), asked.clone());
        assert!(
            router
                .answer_choice(
                    agent,
                    "ch|telegram|bot|tg:2",
                    &id,
                    &[1],
                    havn_core::Language::English
                )
                .is_err()
        );
        assert!(
            router
                .answer_choice(
                    AgentId::new(),
                    chat,
                    &id,
                    &[1],
                    havn_core::Language::English
                )
                .is_err()
        );
        assert!(
            router
                .answer_choice(agent, chat, &id, &[5], havn_core::Language::English)
                .is_err(),
            "not an option"
        );
        let text = router
            .answer_choice(agent, chat, &id, &[2], havn_core::Language::English)
            .expect("answered");
        assert!(text.contains("\"b\""), "{text}");
        // Until the answer reached the agent, the question waits.
        assert!(
            router
                .answer_choice(agent, chat, &id, &[2], havn_core::Language::English)
                .is_ok()
        );
        router.forget_choice(&id);
        assert!(
            router
                .answer_choice(agent, chat, &id, &[2], havn_core::Language::English)
                .is_err(),
            "once"
        );
        // Too many in a conversation: the oldest goes.
        let first = router.ask_choice(agent, chat.into(), asked.clone());
        for _ in 0..MAX_CHOICES_WAITING {
            router.ask_choice(agent, chat.into(), asked.clone());
        }
        assert!(
            router
                .answer_choice(agent, chat, &first, &[1], havn_core::Language::English)
                .is_err(),
            "the oldest went"
        );
        let other = router.ask_choice(agent, "ch|telegram|bot|tg:9".into(), asked);
        assert!(
            router
                .answer_choice(
                    agent,
                    "ch|telegram|bot|tg:9",
                    &other,
                    &[1],
                    havn_core::Language::English
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn an_approval_is_found_by_its_id_in_its_conversation_only() {
        let router = WebChatRouter::new();
        let agent = AgentId::new();
        assert!(router.ask_approval(agent, approval_request("r1")).await);
        assert!(router.ask_approval(agent, approval_request("r2")).await);
        let chat = "ch|telegram|bot|42";
        assert_eq!(
            router.approval(agent, chat, "r2").map(|r| r.request_id),
            Some("r2".to_string())
        );
        assert!(router.approval(agent, "ch|telegram|bot|43", "r2").is_none());
        assert!(router.approval(AgentId::new(), chat, "r2").is_none());
        assert!(router.approval(agent, chat, "r3").is_none());
        assert_eq!(
            router.oldest_approval(agent, chat).map(|r| r.request_id),
            Some("r1".to_string())
        );
    }

    #[tokio::test]
    async fn approvals_reach_every_tab_and_later_ones_until_closed() {
        let router = WebChatRouter::new();
        let agent = AgentId::new();
        let (tx, mut rx) = mpsc::channel(8);
        let _tab = router.register(Uuid::now_v7(), agent, tx).await;
        assert!(router.ask_approval(agent, approval_request("r1")).await);
        assert!(
            !router.ask_approval(agent, approval_request("r1")).await,
            "the id is taken"
        );
        assert!(matches!(
            rx.recv().await,
            Some(ServerMessage::Approval { ref id, ref conversation, .. })
                if id == "r1" && conversation.as_deref() == Some("ch|telegram|bot|42")
        ));
        // A tab opened later gets what still waits.
        let (tx2, mut rx2) = mpsc::channel(8);
        let _late = router.register(Uuid::now_v7(), agent, tx2).await;
        assert!(
            matches!(rx2.recv().await, Some(ServerMessage::Approval { ref id, .. }) if id == "r1")
        );

        assert!(
            router.close_approval(AgentId::new(), "r1").await.is_none(),
            "another agent"
        );
        assert!(router.close_approval(agent, "r1").await.is_some());
        assert!(
            router.close_approval(agent, "r1").await.is_none(),
            "closed once"
        );
        assert!(
            matches!(rx.recv().await, Some(ServerMessage::ApprovalClosed { ref id }) if id == "r1")
        );

        for i in 0..MAX_PENDING_APPROVALS {
            assert!(
                router
                    .ask_approval(agent, approval_request(&format!("p{i}")))
                    .await
            );
        }
        assert!(
            !router.ask_approval(agent, approval_request("over")).await,
            "capped"
        );
    }

    #[tokio::test]
    async fn memory_changes_reach_the_session_cut_to_size() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id: SessionId = Uuid::now_v7();
        let (tx, mut rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, agent_id, tx).await;

        let change = |n: usize| havn_proto::MemoryChange {
            action: havn_proto::MemoryAction::Saved,
            key: format!("k{n}"),
            value: "v".repeat(2000),
            kind: "preference".into(),
            before: None,
            after: Some(format!("row{n}")),
        };
        let changed = havn_proto::MemoryChanged {
            channel_id: session_id.to_string(),
            conversation: Some("c".repeat(65)),
            turn: Some("t".into()),
            changes: (0..30).map(change).collect(),
        };
        router
            .memory_changed(agent_id, changed.clone())
            .await
            .expect("shown");
        match rx.recv().await.expect("frame") {
            ServerMessage::Memory {
                conversation,
                turn,
                changes,
            } => {
                assert_eq!(turn.as_deref(), Some("t"));
                assert_eq!(changes[3].after.as_deref(), Some("row3"));
                assert!(conversation.is_none(), "an oversized chat id is dropped");
                assert_eq!(changes.len(), havn_proto::MAX_MEMORY_CHANGES);
                assert_eq!(
                    changes[0].value.chars().count(),
                    havn_proto::MAX_MEMORY_CHANGE_CHARS
                );
            }
            other => panic!("expected Memory, got {other:?}"),
        }
        // Another agent's session is not shown them.
        assert!(
            router
                .memory_changed(AgentId::new(), changed)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deliver_routes_outbound_to_matching_session() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id: SessionId = Uuid::now_v7();

        let (tx, mut rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, agent_id, tx).await;

        let outbound = OutboundMessage {
            conversation: None,
            channel_id: session_id.to_string(),
            content: MessageContent::Text { text: "hi".into() },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: false,
        };
        router.deliver(agent_id, outbound).await.expect("deliver");

        let received = rx.recv().await.expect("recv");
        match received {
            ServerMessage::AgentMessage { content, .. } => assert_eq!(content, "hi"),
            ServerMessage::Error { .. }
            | ServerMessage::Approval { .. }
            | ServerMessage::ApprovalClosed { .. }
            | ServerMessage::Memory { .. }
            | ServerMessage::File { .. }
            | ServerMessage::Step { .. }
            | ServerMessage::Notice { .. }
            | ServerMessage::ChatTitle { .. }
            | ServerMessage::ReplyText { .. } => {
                panic!("expected AgentMessage, got Error")
            }
        }
    }

    #[test]
    fn an_agents_notice_must_answer_a_command() {
        let router = WebChatRouter::new();
        let (agent, other) = (AgentId::new(), AgentId::new());
        let session: SessionId = Uuid::now_v7();
        assert!(!router.take_command_reply(agent, session), "nothing asked");
        assert!(router.expect_command_reply(agent, session));
        assert!(router.expect_command_reply(agent, session));
        assert!(
            !router.take_command_reply(other, session),
            "another agent's"
        );
        assert!(router.take_command_reply(agent, session));
        assert!(router.take_command_reply(agent, session));
        assert!(!router.take_command_reply(agent, session), "both answered");
        for _ in 0..MAX_COMMANDS_WAITING {
            assert!(router.expect_command_reply(agent, session));
        }
        assert!(
            !router.expect_command_reply(agent, session),
            "too many waiting"
        );
        // Answers that never came are forgotten after a while.
        router
            .command_replies
            .lock()
            .expect("lock")
            .get_mut(&(agent, session))
            .expect("waiting")
            .iter_mut()
            .for_each(|at| *at -= COMMAND_REPLY_KEPT);
        assert!(!router.take_command_reply(agent, session), "expired");
        assert!(router.expect_command_reply(agent, session));
    }

    #[tokio::test]
    async fn deliver_refuses_another_agents_session() {
        let router = WebChatRouter::new();
        let owner = AgentId::new();
        let session_id: SessionId = Uuid::now_v7();
        let (tx, mut rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, owner, tx).await;

        let outbound = OutboundMessage {
            conversation: None,
            channel_id: session_id.to_string(),
            content: MessageContent::Text {
                text: "injected".into(),
            },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: false,
        };
        let err = router
            .deliver(AgentId::new(), outbound)
            .await
            .expect_err("another agent must not reach this session");
        assert!(matches!(err, DeliverError::NoSession(_)), "{err:?}");
        assert!(rx.try_recv().is_err(), "nothing may reach the session");
    }

    #[tokio::test]
    async fn deliver_to_unknown_session_errors() {
        let router = WebChatRouter::new();
        let outbound = OutboundMessage {
            conversation: None,
            channel_id: Uuid::now_v7().to_string(),
            content: MessageContent::Text { text: "hi".into() },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: false,
        };
        let err = router
            .deliver(AgentId::new(), outbound)
            .await
            .expect_err("should fail");
        assert!(matches!(err, DeliverError::NoSession(_)), "{err:?}");
    }

    #[tokio::test]
    async fn deliver_with_invalid_channel_id_errors() {
        let router = WebChatRouter::new();
        let outbound = OutboundMessage {
            conversation: None,
            channel_id: "not-a-uuid".into(),
            content: MessageContent::Text { text: "hi".into() },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: false,
        };
        let err = router
            .deliver(AgentId::new(), outbound)
            .await
            .expect_err("should fail");
        assert!(matches!(err, DeliverError::InvalidChannelId(_)), "{err:?}");
    }

    #[tokio::test]
    async fn broadcast_fans_out_to_all_sessions_for_agent() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let other_agent = AgentId::new();

        let (tx_a, mut rx_a) = mpsc::channel::<ServerMessage>(8);
        let (tx_b, mut rx_b) = mpsc::channel::<ServerMessage>(8);
        let (tx_other, mut rx_other) = mpsc::channel::<ServerMessage>(8);
        router.register(Uuid::now_v7(), agent_id, tx_a).await;
        router.register(Uuid::now_v7(), agent_id, tx_b).await;
        router.register(Uuid::now_v7(), other_agent, tx_other).await;

        let outbound = OutboundMessage {
            conversation: None,
            channel_id: "broadcast".into(),
            content: MessageContent::Text {
                text: "hello".into(),
            },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: false,
        };
        let n = router.broadcast(agent_id, outbound).await;
        assert_eq!(n, 2, "should reach both sessions for the target agent");

        for rx in [&mut rx_a, &mut rx_b] {
            match rx.recv().await.expect("frame") {
                ServerMessage::AgentMessage { content, .. } => assert_eq!(content, "hello"),
                ServerMessage::Error { .. }
                | ServerMessage::Approval { .. }
                | ServerMessage::ApprovalClosed { .. }
                | ServerMessage::Memory { .. }
                | ServerMessage::File { .. }
                | ServerMessage::Step { .. }
                | ServerMessage::Notice { .. }
                | ServerMessage::ChatTitle { .. }
                | ServerMessage::ReplyText { .. } => {
                    panic!("unexpected Error")
                }
            }
        }
        // The unrelated agent's session must NOT have received it.
        assert!(rx_other.try_recv().is_err());
    }

    fn text(t: &str) -> OutboundMessage {
        OutboundMessage {
            conversation: None,
            channel_id: "broadcast".into(),
            content: MessageContent::Text { text: t.into() },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: false,
        }
    }

    #[tokio::test]
    async fn stalled_session_does_not_block_others_and_is_dropped() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let stalled = Uuid::now_v7();
        let (stalled_tx, _stalled_rx) = mpsc::channel::<ServerMessage>(1);
        let stalled_tab = router.register(stalled, agent_id, stalled_tx).await;
        let (healthy_tx, mut healthy_rx) = mpsc::channel::<ServerMessage>(8);
        router.register(Uuid::now_v7(), agent_id, healthy_tx).await;

        assert_eq!(router.broadcast(agent_id, text("one")).await, 2);
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            router.broadcast(agent_id, text("two")),
        )
        .await
        .expect("a stalled session must not block the broadcast");
        assert_eq!(n, 1);
        for expected in ["one", "two"] {
            match healthy_rx.try_recv().expect("frame") {
                ServerMessage::AgentMessage { content, .. } => assert_eq!(content, expected),
                ServerMessage::Error { .. }
                | ServerMessage::Approval { .. }
                | ServerMessage::ApprovalClosed { .. }
                | ServerMessage::Memory { .. }
                | ServerMessage::File { .. }
                | ServerMessage::Step { .. }
                | ServerMessage::Notice { .. }
                | ServerMessage::ChatTitle { .. }
                | ServerMessage::ReplyText { .. } => {
                    panic!("unexpected Error")
                }
            }
        }
        assert!(
            router.agent_for_session(stalled).await.is_none(),
            "the stalled session is dropped"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            stalled_tab.closed.notified(),
        )
        .await
        .expect("its handler is told to close the socket");
    }

    #[tokio::test]
    async fn deliver_to_a_stalled_session_fails_at_once() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (tx, _rx) = mpsc::channel::<ServerMessage>(1);
        router.register(session_id, agent_id, tx).await;
        let to_session = |t: &str| OutboundMessage {
            conversation: None,
            channel_id: session_id.to_string(),
            ..text(t)
        };

        router
            .deliver(agent_id, to_session("one"))
            .await
            .expect("first fits");
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            router.deliver(agent_id, to_session("two")),
        )
        .await
        .expect("deliver must not wait for the browser")
        .expect_err("queue is full");
        assert!(matches!(err, DeliverError::Stalled(_)), "{err:?}");
    }

    #[tokio::test]
    async fn unregister_removes_session() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (tx, _rx) = mpsc::channel::<ServerMessage>(8);
        let registration = router.register(session_id, agent_id, tx).await;
        router.unregister(registration.tab).await;
        assert!(router.agent_for_session(session_id).await.is_none());
    }

    #[tokio::test]
    async fn every_open_tab_of_a_session_gets_replies() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (first, mut first_rx) = mpsc::channel::<ServerMessage>(8);
        let first_tab = router.register(session_id, agent_id, first).await;
        let (second, mut second_rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, agent_id, second).await;
        let reply = |t: &str| OutboundMessage {
            conversation: None,
            channel_id: session_id.to_string(),
            ..text(t)
        };

        router
            .deliver(agent_id, reply("both"))
            .await
            .expect("deliver");
        assert!(first_rx.try_recv().is_ok());
        assert!(second_rx.try_recv().is_ok());

        router.unregister(first_tab.tab).await;
        router
            .deliver(agent_id, reply("second only"))
            .await
            .expect("the other tab is still open");
        assert!(second_rx.try_recv().is_ok());
    }
}
