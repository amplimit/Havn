//! In-memory router for `WebSocket` `WebChat` sessions.
//!
//! Each open browser `WebSocket` maps 1:1 to a `SessionId` (UUID v7) which is
//! used as the `channel_id` in inbound/outbound messages. When an agent
//! returns an `OutboundMessage`, the gateway looks up the matching session
//! here and pushes the reply onto its mpsc; the per-session writer task
//! drains that mpsc onto the `WebSocket`.
//!
//! WebChat is havn's built-in console (spec §3.6), not a channel adapter.
//! External channel adapters (Telegram, Slack, …) speak the HTTP+WS protocol
//! at `/api/v1/channel` and never touch this router.

use std::collections::HashMap;
use std::sync::Arc;

use havn_core::{AgentId, MessageContent, OutboundMessage};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

pub type SessionId = Uuid;

#[derive(Debug, Clone, Default)]
pub struct WebChatRouter {
    inner: Arc<Mutex<HashMap<SessionId, Session>>>,
}

#[derive(Debug)]
struct Session {
    /// Agent the session is bound to. Used by `agent_for_session` for the
    /// dashboard's agent-status surface; no other code currently reads it.
    #[allow(dead_code, reason = "consumed by upcoming dashboard endpoints")]
    agent_id: AgentId,
    tx: mpsc::Sender<ServerMessage>,
}

/// Wire types over the WebSocket frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// User-typed message.
    Send { content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Reply from the agent (assistant turn).
    AgentMessage { content: String },
    /// Soft error visible to the user (e.g. agent disconnected).
    Error { message: String },
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
}

impl WebChatRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        tx: mpsc::Sender<ServerMessage>,
    ) {
        self.inner
            .lock()
            .await
            .insert(session_id, Session { agent_id, tx });
    }

    pub async fn unregister(&self, session_id: SessionId) {
        self.inner.lock().await.remove(&session_id);
    }

    /// Forward an `OutboundMessage` from an agent to its originating `WebChat` session.
    /// `channel_id` MUST be the session's UUID v7 string (set by the WS handler when
    /// it constructed the `InboundMessage`).
    pub async fn deliver(&self, msg: OutboundMessage) -> Result<(), DeliverError> {
        let session_id: SessionId = msg
            .channel_id
            .parse()
            .map_err(|_| DeliverError::InvalidChannelId(msg.channel_id.clone()))?;

        let tx = {
            let map = self.inner.lock().await;
            map.get(&session_id)
                .map(|s| s.tx.clone())
                .ok_or(DeliverError::NoSession(session_id))?
        };

        let server_msg = match msg.content {
            MessageContent::Text { text } => ServerMessage::AgentMessage { content: text },
            // Phase 1 webchat is text-only. Stub other variants with a placeholder.
            other => ServerMessage::AgentMessage {
                content: format!("(unsupported content variant: {other:?})"),
            },
        };
        tx.send(server_msg)
            .await
            .map_err(|_| DeliverError::WriterClosed)
    }

    #[allow(dead_code, reason = "consumed by upcoming dashboard endpoints")]
    pub async fn agent_for_session(&self, session_id: SessionId) -> Option<AgentId> {
        self.inner.lock().await.get(&session_id).map(|s| s.agent_id)
    }

    /// Fan-out a proactive `OutboundMessage` (e.g. heartbeat output) to every
    /// open `WebChat` session bound to `agent_id`. Returns the number of sessions
    /// the message was successfully queued onto. Closed senders are skipped
    /// silently — the per-session writer task already handles that path.
    pub async fn broadcast(&self, agent_id: AgentId, msg: OutboundMessage) -> usize {
        let server_msg = match msg.content {
            MessageContent::Text { text } => ServerMessage::AgentMessage { content: text },
            other => ServerMessage::AgentMessage {
                content: format!("(unsupported content variant: {other:?})"),
            },
        };
        let map = self.inner.lock().await;
        let mut delivered = 0usize;
        for session in map.values() {
            if session.agent_id == agent_id && session.tx.send(server_msg.clone()).await.is_ok() {
                delivered += 1;
            }
        }
        delivered
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn deliver_routes_outbound_to_matching_session() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id: SessionId = Uuid::now_v7();

        let (tx, mut rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, agent_id, tx).await;

        let outbound = OutboundMessage {
            channel_id: session_id.to_string(),
            content: MessageContent::Text { text: "hi".into() },
            reply_to: None,
        };
        router.deliver(outbound).await.expect("deliver");

        let received = rx.recv().await.expect("recv");
        match received {
            ServerMessage::AgentMessage { content } => assert_eq!(content, "hi"),
            ServerMessage::Error { .. } => panic!("expected AgentMessage, got Error"),
        }
    }

    #[tokio::test]
    async fn deliver_to_unknown_session_errors() {
        let router = WebChatRouter::new();
        let outbound = OutboundMessage {
            channel_id: Uuid::now_v7().to_string(),
            content: MessageContent::Text { text: "hi".into() },
            reply_to: None,
        };
        let err = router.deliver(outbound).await.expect_err("should fail");
        assert!(matches!(err, DeliverError::NoSession(_)), "{err:?}");
    }

    #[tokio::test]
    async fn deliver_with_invalid_channel_id_errors() {
        let router = WebChatRouter::new();
        let outbound = OutboundMessage {
            channel_id: "not-a-uuid".into(),
            content: MessageContent::Text { text: "hi".into() },
            reply_to: None,
        };
        let err = router.deliver(outbound).await.expect_err("should fail");
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
                ServerMessage::AgentMessage { content } => assert_eq!(content, "hello"),
                ServerMessage::Error { .. } => panic!("unexpected Error"),
            }
        }
        // The unrelated agent's session must NOT have received it.
        assert!(rx_other.try_recv().is_err());
    }

    #[tokio::test]
    async fn unregister_removes_session() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (tx, _rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, agent_id, tx).await;
        router.unregister(session_id).await;
        assert!(router.agent_for_session(session_id).await.is_none());
    }
}
