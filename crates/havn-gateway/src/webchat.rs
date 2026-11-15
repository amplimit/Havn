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
}

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
    /// is new), or the active chat when absent.
    Send {
        content: String,
        #[serde(default)]
        conversation: Option<String>,
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
        /// "always" then adds it to the agent's allowed domains.
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    /// The approval request `id` was decided or ended; stop showing it.
    ApprovalClosed { id: String },
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
            if waiting >= MAX_PENDING_APPROVALS || pending.contains_key(&request.request_id) {
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
            },
            // Phase 1 webchat is text-only. Stub other variants with a placeholder.
            other => ServerMessage::AgentMessage {
                content: format!("(unsupported content variant: {other:?})"),
                broadcast: false,
                conversation,
            },
        };
        self.send_to_session(agent_id, session_id, server_msg).await
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
            },
            other => ServerMessage::AgentMessage {
                content: format!("(unsupported content variant: {other:?})"),
                broadcast: true,
                conversation,
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
        kind: request
            .request_id
            .starts_with(crate::egress_proxy::REQUEST_PREFIX)
            .then(|| "network".to_string()),
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
        }
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
        };
        router.deliver(agent_id, outbound).await.expect("deliver");

        let received = rx.recv().await.expect("recv");
        match received {
            ServerMessage::AgentMessage { content, .. } => assert_eq!(content, "hi"),
            ServerMessage::Error { .. }
            | ServerMessage::Approval { .. }
            | ServerMessage::ApprovalClosed { .. } => {
                panic!("expected AgentMessage, got Error")
            }
        }
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
        };
        let n = router.broadcast(agent_id, outbound).await;
        assert_eq!(n, 2, "should reach both sessions for the target agent");

        for rx in [&mut rx_a, &mut rx_b] {
            match rx.recv().await.expect("frame") {
                ServerMessage::AgentMessage { content, .. } => assert_eq!(content, "hello"),
                ServerMessage::Error { .. }
                | ServerMessage::Approval { .. }
                | ServerMessage::ApprovalClosed { .. } => {
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
                | ServerMessage::ApprovalClosed { .. } => {
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
