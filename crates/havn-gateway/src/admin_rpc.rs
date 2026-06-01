//! Request/response correlation for dashboard-initiated agent RPCs
//! (spec Phase-2 §13: `MemoryForgetRequest/Response`,
//! `SkillPinRequest/Response`).
//!
//! The agent socket is bidirectional but the LLM proxy already owns
//! one form of req/res correlation (`LlmRequest`/`LlmResponse`). For
//! the new admin RPCs we want the same shape:
//!
//! 1. Caller picks a fresh `request_id` (UUID v7).
//! 2. Caller registers a `oneshot::Sender` keyed by that id.
//! 3. Gateway sends the `*Request` frame down the agent socket.
//! 4. Runtime processes, sends `*Response` back.
//! 5. The agent-socket reader matches on the response variant and
//!    calls [`AdminRpc::resolve`], which pops the oneshot and forwards
//!    the outcome.
//! 6. Caller awaits the receiver with a timeout.
//!
//! [`AdminRpc`] is one shared `Arc<Mutex<...>>` over a single map keyed
//! by `request_id`. Memory and Skill responses share the same map
//! because their request ids never overlap (UUIDv7) — separating per
//! variant would just add code without removing any race.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use havn_proto::AdminOutcome;
use thiserror::Error;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tracing::warn;

#[derive(Debug, Clone, Default)]
pub struct AdminRpc {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<AdminOutcome>>>>,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RpcError {
    #[error("agent did not reply within {0:?}")]
    Timeout(Duration),
    #[error("response channel closed before reply arrived")]
    ChannelClosed,
}

impl AdminRpc {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a slot for `request_id` and return the receiver. The
    /// caller sends the request frame down the agent socket and then
    /// awaits this receiver.
    pub async fn register(&self, request_id: &str) -> oneshot::Receiver<AdminOutcome> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.into(), tx);
        rx
    }

    /// Match an incoming `*Response` to its pending sender and forward
    /// the outcome. No-op (with a warn log) if the id has no pending
    /// entry — that means the caller already timed out, which is fine
    /// from the runtime's perspective.
    pub async fn resolve(&self, request_id: &str, outcome: AdminOutcome) {
        let sender = self.pending.lock().await.remove(request_id);
        if let Some(tx) = sender {
            // Receiver may have dropped (caller cancelled / timed out).
            // That's fine — silently discard.
            let _ = tx.send(outcome);
        } else {
            warn!(
                request_id,
                "admin RPC response with no pending entry — caller timed out or duplicate"
            );
        }
    }

    /// Convenience: register, run the supplied async block (which is
    /// expected to send the request frame), then await the response
    /// with the configured timeout. Cleans up the pending entry on
    /// timeout so it doesn't grow unbounded.
    ///
    /// `timeout_after` defaults to 10 s — admin RPCs touch agent.db
    /// which is a millisecond op; 10 s gives generous headroom for a
    /// briefly-blocked runtime.
    pub async fn call<F, Fut, E>(
        &self,
        request_id: &str,
        send: F,
        timeout_after: Duration,
    ) -> Result<AdminOutcome, AdminRpcCallError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), E>>,
    {
        let rx = self.register(request_id).await;
        if let Err(e) = send().await {
            // Roll back the pending entry — the request was never
            // dispatched; nobody will ever resolve it.
            self.pending.lock().await.remove(request_id);
            return Err(AdminRpcCallError::Send(e));
        }
        match timeout(timeout_after, rx).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(_)) => {
                // Sender was dropped without sending — runtime crashed
                // mid-RPC, or the registry detached. Treat as channel
                // closed.
                Err(AdminRpcCallError::Rpc(RpcError::ChannelClosed))
            }
            Err(_) => {
                // Timeout. Pop the pending entry to avoid memory growth
                // when a slow runtime eventually replies.
                self.pending.lock().await.remove(request_id);
                Err(AdminRpcCallError::Rpc(RpcError::Timeout(timeout_after)))
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum AdminRpcCallError<E> {
    #[error("send: {0}")]
    Send(E),
    #[error("rpc: {0}")]
    Rpc(RpcError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn register_then_resolve_completes_receiver() {
        let rpc = AdminRpc::new();
        let rx = rpc.register("req-1").await;
        rpc.resolve(
            "req-1",
            AdminOutcome::Ok {
                details: serde_json::json!({"k": "v"}),
            },
        )
        .await;
        let out = rx.await.expect("receiver");
        assert!(matches!(out, AdminOutcome::Ok { .. }));
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_a_warn_not_a_panic() {
        let rpc = AdminRpc::new();
        rpc.resolve("never-registered", AdminOutcome::NotFound)
            .await;
        // No assertions — just verifying we don't panic.
    }

    #[tokio::test]
    async fn call_succeeds_when_response_arrives() {
        let rpc = AdminRpc::new();
        let rpc_for_resolve = rpc.clone();
        // Spawn a "runtime" that resolves shortly after we send.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            rpc_for_resolve.resolve("r", AdminOutcome::NotFound).await;
        });
        let result: Result<AdminOutcome, AdminRpcCallError<std::io::Error>> = rpc
            .call(
                "r",
                || async { Ok::<_, std::io::Error>(()) },
                Duration::from_millis(500),
            )
            .await;
        assert!(matches!(result, Ok(AdminOutcome::NotFound)), "{result:?}");
    }

    #[tokio::test]
    async fn call_times_out_when_no_reply() {
        let rpc = AdminRpc::new();
        let result: Result<AdminOutcome, AdminRpcCallError<std::io::Error>> = rpc
            .call(
                "r",
                || async { Ok::<_, std::io::Error>(()) },
                Duration::from_millis(50),
            )
            .await;
        assert!(matches!(
            result,
            Err(AdminRpcCallError::Rpc(RpcError::Timeout(_)))
        ));
        // Pending entry was cleaned up.
        assert!(rpc.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn call_send_error_drops_pending_entry() {
        let rpc = AdminRpc::new();
        let result: Result<AdminOutcome, AdminRpcCallError<&'static str>> = rpc
            .call(
                "r",
                || async { Err::<(), _>("send fail") },
                Duration::from_secs(1),
            )
            .await;
        assert!(matches!(result, Err(AdminRpcCallError::Send(_))));
        assert!(rpc.pending.lock().await.is_empty());
    }
}
