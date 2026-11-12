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
use tracing::warn;
use uuid::Uuid;

pub type SessionId = Uuid;

#[derive(Debug, Clone, Default)]
pub struct WebChatRouter {
    inner: Arc<Mutex<HashMap<SessionId, Session>>>,
}

#[derive(Debug)]
struct Session {
    /// Agent the session is bound to. Only that agent's messages are
    /// delivered to it.
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
    #[error("webchat session {0} is not reading; closed it")]
    Stalled(SessionId),
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

    /// Remove the session registered with `tx`. A newer tab that re-used
    /// the session id under its own sender keeps its entry.
    pub async fn unregister(&self, session_id: SessionId, tx: &mpsc::Sender<ServerMessage>) {
        let mut map = self.inner.lock().await;
        if map.get(&session_id).is_some_and(|s| s.tx.same_channel(tx)) {
            map.remove(&session_id);
        }
    }

    /// Forward an `OutboundMessage` from `agent_id` to its originating `WebChat` session.
    /// `channel_id` MUST be the session's UUID v7 string (set by the WS handler when
    /// it constructed the `InboundMessage`). Sessions of other agents are reported
    /// as missing.
    pub async fn deliver(
        &self,
        agent_id: AgentId,
        msg: OutboundMessage,
    ) -> Result<(), DeliverError> {
        let session_id: SessionId = msg
            .channel_id
            .parse()
            .map_err(|_| DeliverError::InvalidChannelId(msg.channel_id.clone()))?;

        let tx = {
            let map = self.inner.lock().await;
            map.get(&session_id)
                .filter(|s| s.agent_id == agent_id)
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
        self.offer(session_id, &tx, server_msg).await
    }

    /// Queue `msg` without waiting. The caller is an agent's reader loop, so
    /// a browser that stops reading must not hold it up: its session is
    /// dropped instead, which ends its writer task and closes the socket.
    async fn offer(
        &self,
        session_id: SessionId,
        tx: &mpsc::Sender<ServerMessage>,
        msg: ServerMessage,
    ) -> Result<(), DeliverError> {
        match tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(DeliverError::WriterClosed),
            Err(mpsc::error::TrySendError::Full(_)) => {
                let mut map = self.inner.lock().await;
                if map.get(&session_id).is_some_and(|s| s.tx.same_channel(tx)) {
                    map.remove(&session_id);
                }
                warn!(%session_id, "webchat session outbound queue is full; closing the session");
                Err(DeliverError::Stalled(session_id))
            }
        }
    }

    #[allow(dead_code, reason = "consumed by upcoming dashboard endpoints")]
    pub async fn agent_for_session(&self, session_id: SessionId) -> Option<AgentId> {
        self.inner.lock().await.get(&session_id).map(|s| s.agent_id)
    }

    /// Fan-out a proactive `OutboundMessage` (e.g. heartbeat output) to every
    /// open `WebChat` session bound to `agent_id`. Returns the number of sessions
    /// the message was successfully queued onto. Closed senders are skipped
    /// silently — the per-session writer task already handles that path —
    /// and sessions whose queue is full are dropped (see [`Self::offer`]).
    pub async fn broadcast(&self, agent_id: AgentId, msg: OutboundMessage) -> usize {
        let server_msg = match msg.content {
            MessageContent::Text { text } => ServerMessage::AgentMessage { content: text },
            other => ServerMessage::AgentMessage {
                content: format!("(unsupported content variant: {other:?})"),
            },
        };
        let targets: Vec<(SessionId, mpsc::Sender<ServerMessage>)> = self
            .inner
            .lock()
            .await
            .iter()
            .filter(|(_, s)| s.agent_id == agent_id)
            .map(|(id, s)| (*id, s.tx.clone()))
            .collect();
        let mut delivered = 0usize;
        for (session_id, tx) in targets {
            if self
                .offer(session_id, &tx, server_msg.clone())
                .await
                .is_ok()
            {
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
        router.deliver(agent_id, outbound).await.expect("deliver");

        let received = rx.recv().await.expect("recv");
        match received {
            ServerMessage::AgentMessage { content } => assert_eq!(content, "hi"),
            ServerMessage::Error { .. } => panic!("expected AgentMessage, got Error"),
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

    fn text(t: &str) -> OutboundMessage {
        OutboundMessage {
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
        router.register(stalled, agent_id, stalled_tx).await;
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
                ServerMessage::AgentMessage { content } => assert_eq!(content, expected),
                ServerMessage::Error { .. } => panic!("unexpected Error"),
            }
        }
        assert!(
            router.agent_for_session(stalled).await.is_none(),
            "the stalled session is dropped"
        );
    }

    #[tokio::test]
    async fn deliver_to_a_stalled_session_fails_at_once() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (tx, _rx) = mpsc::channel::<ServerMessage>(1);
        router.register(session_id, agent_id, tx).await;
        let to_session = |t: &str| OutboundMessage {
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
        router.register(session_id, agent_id, tx.clone()).await;
        router.unregister(session_id, &tx).await;
        assert!(router.agent_for_session(session_id).await.is_none());
    }

    #[tokio::test]
    async fn stale_unregister_keeps_a_newer_tab() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (old, _old_rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, agent_id, old.clone()).await;
        let (new, _new_rx) = mpsc::channel::<ServerMessage>(8);
        router.register(session_id, agent_id, new).await;

        router.unregister(session_id, &old).await;

        assert_eq!(router.agent_for_session(session_id).await, Some(agent_id));
    }
}
