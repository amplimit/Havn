//! Cross-agent query correlation (spec §4.4 v0.7).
//!
//! Same shape as [`crate::admin_rpc::AdminRpc`] but for the
//! [`havn_proto::AgentQueryResponse`] payload. The agent socket reader
//! demultiplexes incoming response frames into per-request oneshots so
//! the broker handler that issued the [`havn_proto::IncomingQuery`] can
//! await its specific reply.
//!
//! Why not reuse `AdminRpc`? `AdminOutcome` and `AgentQueryOutcome` carry
//! different payload shapes and live on different protocol paths;
//! retrofitting `AdminRpc` to be generic over the outcome type churns
//! every existing call site for no functional gain. A second small
//! broker is cheaper than the refactor.

use std::collections::HashMap;
use std::sync::Arc;

use havn_proto::AgentQueryResponse;
use tokio::sync::{Mutex, oneshot};
use tracing::warn;

#[derive(Debug, Clone, Default)]
pub struct QueryBroker {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<AgentQueryResponse>>>>,
}

impl QueryBroker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a slot for `request_id`. Caller sends the
    /// `IncomingQuery` frame down the target's agent socket, then
    /// awaits the returned receiver under a wall-clock timeout.
    pub async fn register(&self, request_id: &str) -> oneshot::Receiver<AgentQueryResponse> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.into(), tx);
        rx
    }

    /// Demultiplex an incoming [`AgentQueryResponse`] from the target
    /// agent. No-op (with a warn) if the id has no pending entry — the
    /// caller most likely timed out and tore down its receiver.
    pub async fn resolve(&self, response: AgentQueryResponse) {
        let request_id = response.request_id.clone();
        let sender = self.pending.lock().await.remove(&request_id);
        if let Some(tx) = sender {
            // Receiver may have dropped (caller cancelled / timed out).
            // Silently discard — the receiver-side error path already
            // surfaces the timeout to the LLM as a tool_result.
            let _ = tx.send(response);
        } else {
            warn!(
                request_id,
                "AgentQueryResponse with no pending entry — caller timed out or duplicate id"
            );
        }
    }

    /// Drop the pending slot if registered. Used by the broker handler
    /// when its receiver future is cancelled (timeout, abort) — keeps
    /// the map from leaking entries on long-running gateways.
    pub async fn cancel(&self, request_id: &str) {
        self.pending.lock().await.remove(request_id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use havn_proto::AgentQueryOutcome;

    #[tokio::test]
    async fn register_resolve_round_trip() {
        let broker = QueryBroker::new();
        let rx = broker.register("rid-1").await;

        broker
            .resolve(AgentQueryResponse {
                request_id: "rid-1".into(),
                outcome: AgentQueryOutcome::Ok,
                text: "hi".into(),
                transcript: serde_json::Value::Null,
            })
            .await;

        let r = rx.await.expect("recv");
        assert_eq!(r.text, "hi");
    }

    #[tokio::test]
    async fn cancel_removes_pending_entry() {
        let broker = QueryBroker::new();
        let _rx = broker.register("rid-2").await;
        broker.cancel("rid-2").await;
        // resolving after cancel hits the "no pending" warn path,
        // exercised by the absence of a panic.
        broker
            .resolve(AgentQueryResponse {
                request_id: "rid-2".into(),
                outcome: AgentQueryOutcome::Ok,
                text: String::new(),
                transcript: serde_json::Value::Null,
            })
            .await;
    }

    #[tokio::test]
    async fn unknown_request_id_is_ignored() {
        let broker = QueryBroker::new();
        // No register call. Resolve must not panic.
        broker
            .resolve(AgentQueryResponse {
                request_id: "rid-orphan".into(),
                outcome: AgentQueryOutcome::Ok,
                text: String::new(),
                transcript: serde_json::Value::Null,
            })
            .await;
    }
}
