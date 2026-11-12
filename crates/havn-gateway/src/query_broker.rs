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

use havn_core::AgentId;
use havn_proto::AgentQueryResponse;
use tokio::sync::{Mutex, oneshot};
use tracing::warn;

#[derive(Debug)]
struct Pending {
    /// The only agent allowed to answer.
    target: AgentId,
    tx: oneshot::Sender<AgentQueryResponse>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryBroker {
    pending: Arc<Mutex<HashMap<String, Pending>>>,
}

impl QueryBroker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a slot for a query dispatched to `target` and return the id
    /// to put in its `IncomingQuery`. The broker chooses the id, so a
    /// calling agent cannot pick or collide with it, and only `target` can
    /// answer. The caller awaits the receiver under a wall-clock timeout.
    pub async fn register(
        &self,
        target: AgentId,
    ) -> (String, oneshot::Receiver<AgentQueryResponse>) {
        let request_id = uuid::Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(request_id.clone(), Pending { target, tx });
        (request_id, rx)
    }

    /// Demultiplex an [`AgentQueryResponse`] sent by `responder`. A
    /// response from any agent other than the query's target is dropped and
    /// leaves the pending query untouched. No-op (with a warn) if the id has
    /// no pending entry — the caller most likely timed out and tore down its
    /// receiver.
    pub async fn resolve(&self, responder: AgentId, response: AgentQueryResponse) {
        let request_id = response.request_id.clone();
        let pending = {
            let mut map = self.pending.lock().await;
            match map.get(&request_id) {
                Some(p) if p.target == responder => map.remove(&request_id),
                Some(_) => {
                    warn!(
                        request_id,
                        %responder,
                        "AgentQueryResponse from an agent that was not asked; dropping"
                    );
                    return;
                }
                None => None,
            }
        };
        if let Some(p) = pending {
            // Receiver may have dropped (caller cancelled / timed out).
            // Silently discard — the receiver-side error path already
            // surfaces the timeout to the LLM as a tool_result.
            let _ = p.tx.send(response);
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

    fn response(request_id: &str, text: &str) -> AgentQueryResponse {
        AgentQueryResponse {
            request_id: request_id.into(),
            outcome: AgentQueryOutcome::Ok,
            text: text.into(),
            transcript: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn register_resolve_round_trip() {
        let broker = QueryBroker::new();
        let target = AgentId::new();
        let (id, rx) = broker.register(target).await;
        broker.resolve(target, response(&id, "hi")).await;
        assert_eq!(rx.await.expect("recv").text, "hi");
    }

    #[tokio::test]
    async fn only_the_target_can_answer() {
        let broker = QueryBroker::new();
        let target = AgentId::new();
        let (id, mut rx) = broker.register(target).await;

        broker
            .resolve(AgentId::new(), response(&id, "forged"))
            .await;
        assert!(rx.try_recv().is_err(), "a forged answer must not arrive");

        broker.resolve(target, response(&id, "real")).await;
        assert_eq!(rx.await.expect("recv").text, "real");
    }

    #[tokio::test]
    async fn ids_are_chosen_by_the_broker() {
        let broker = QueryBroker::new();
        let target = AgentId::new();
        let (first, _rx1) = broker.register(target).await;
        let (second, _rx2) = broker.register(target).await;
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn cancel_removes_pending_entry() {
        let broker = QueryBroker::new();
        let target = AgentId::new();
        let (id, mut rx) = broker.register(target).await;
        broker.cancel(&id).await;
        broker.resolve(target, response(&id, "late")).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_request_id_is_ignored() {
        let broker = QueryBroker::new();
        broker
            .resolve(AgentId::new(), response("rid-orphan", ""))
            .await;
    }
}
