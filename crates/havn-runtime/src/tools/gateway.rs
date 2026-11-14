//! Tools the gateway runs for the agent (`web_search`). The gateway lists
//! them in `Welcome`; each call goes out as a `GatewayToolCall` frame and
//! the reader loop hands the matching `GatewayToolResult` back here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use havn_proto::{
    AgentToGateway, GatewayToolCall, GatewayToolOutcome, GatewayToolResult, GatewayToolSpec,
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::tools::{Tool, ToolCtx, ToolResult};

/// Longest the runtime waits for the gateway. The gateway gives each
/// backend less than this, so its own error normally arrives first.
const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// Largest tool input sent to the gateway, well under the frame limit.
const MAX_INPUT_BYTES: usize = 64 * 1024;

pub type PendingGatewayCalls = Arc<Mutex<HashMap<String, oneshot::Sender<GatewayToolResult>>>>;

pub struct GatewayTool {
    spec: GatewayToolSpec,
    pending: PendingGatewayCalls,
    writer_tx: mpsc::Sender<AgentToGateway>,
}

impl GatewayTool {
    pub fn new(
        spec: GatewayToolSpec,
        pending: PendingGatewayCalls,
        writer_tx: mpsc::Sender<AgentToGateway>,
    ) -> Self {
        Self {
            spec,
            pending,
            writer_tx,
        }
    }
}

/// Wake the call waiting on `result`. Returns false when none is.
pub async fn resolve(pending: &PendingGatewayCalls, result: GatewayToolResult) -> bool {
    let waiter = pending.lock().await.remove(&result.request_id);
    waiter.is_some_and(|tx| tx.send(result).is_ok())
}

#[async_trait]
impl Tool for GatewayTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn input_schema(&self) -> Value {
        self.spec.input_schema.clone()
    }

    async fn execute(&self, _ctx: &ToolCtx, input: Value) -> ToolResult {
        let size = input.to_string().len();
        if size > MAX_INPUT_BYTES {
            return ToolResult::Error(format!(
                "input of {size} bytes exceeds the {MAX_INPUT_BYTES}-byte limit"
            ));
        }
        let request_id = Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        // Registered before sending, so a fast reply always finds it, and
        // removed however the call ends, also when this future is dropped.
        self.pending.lock().await.insert(request_id.clone(), tx);
        let _entry = crate::tool_loop::PendingEntry {
            pending: &self.pending,
            request_id: request_id.clone(),
        };
        let frame = AgentToGateway::GatewayToolCall(GatewayToolCall {
            request_id: request_id.clone(),
            name: self.spec.name.clone(),
            input,
        });
        if self.writer_tx.send(frame).await.is_err() {
            return ToolResult::Error("the gateway connection is closed".into());
        }
        let result = match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => return ToolResult::Error("the gateway connection is closed".into()),
            Err(_) => {
                return ToolResult::Error(format!(
                    "{} got no answer from the gateway within {}s",
                    self.spec.name,
                    CALL_TIMEOUT.as_secs()
                ));
            }
        };
        match result.outcome {
            GatewayToolOutcome::Ok { content } => ToolResult::Ok(content),
            GatewayToolOutcome::Error { message } => ToolResult::Error(message),
            _ => ToolResult::Error("the gateway sent a result this runtime cannot read".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn spec() -> GatewayToolSpec {
        GatewayToolSpec {
            name: "web_search".into(),
            description: "search".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    async fn ctx() -> ToolCtx {
        ToolCtx {
            workspace_dir: std::env::temp_dir(),
            agent_db: havn_db::agent::connect_in_memory().await.expect("agent db"),
            http: reqwest::Client::new(),
            policy: Arc::new(havn_core::Policy::default()),
            embedder: None,
        }
    }

    #[tokio::test]
    async fn a_call_goes_to_the_gateway_and_returns_its_answer() {
        let pending: PendingGatewayCalls = Arc::default();
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        let tool = GatewayTool::new(spec(), Arc::clone(&pending), writer_tx);
        let gateway = tokio::spawn({
            let pending = Arc::clone(&pending);
            async move {
                let Some(AgentToGateway::GatewayToolCall(call)) = writer_rx.recv().await else {
                    panic!("expected a GatewayToolCall");
                };
                assert_eq!(call.name, "web_search");
                assert_eq!(call.input["query"], "havn");
                resolve(
                    &pending,
                    GatewayToolResult {
                        request_id: call.request_id,
                        outcome: GatewayToolOutcome::Ok {
                            content: "1. Havn".into(),
                        },
                    },
                )
                .await
            }
        });
        let result = tool
            .execute(&ctx().await, serde_json::json!({"query": "havn"}))
            .await;
        assert!(gateway.await.expect("gateway task"));
        assert_eq!(result.text(), "1. Havn");
        assert!(!result.is_error());
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn gateway_errors_reach_the_model_as_errors() {
        let pending: PendingGatewayCalls = Arc::default();
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        let tool = GatewayTool::new(spec(), Arc::clone(&pending), writer_tx);
        let gateway = tokio::spawn({
            let pending = Arc::clone(&pending);
            async move {
                let Some(AgentToGateway::GatewayToolCall(call)) = writer_rx.recv().await else {
                    panic!("expected a GatewayToolCall");
                };
                resolve(
                    &pending,
                    GatewayToolResult {
                        request_id: call.request_id,
                        outcome: GatewayToolOutcome::Error {
                            message: "no search backend".into(),
                        },
                    },
                )
                .await
            }
        });
        let result = tool.execute(&ctx().await, serde_json::json!({})).await;
        assert!(gateway.await.expect("gateway task"));
        assert!(result.is_error());
        assert_eq!(result.text(), "no search backend");
    }

    #[tokio::test]
    async fn oversized_input_is_refused_before_sending() {
        let pending: PendingGatewayCalls = Arc::default();
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        let tool = GatewayTool::new(spec(), Arc::clone(&pending), writer_tx);
        let input = serde_json::json!({"query": "x".repeat(MAX_INPUT_BYTES)});
        let result = tool.execute(&ctx().await, input).await;
        assert!(result.is_error());
        assert!(writer_rx.try_recv().is_err());
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn a_dropped_call_leaves_no_pending_entry() {
        let pending: PendingGatewayCalls = Arc::default();
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        let tool = GatewayTool::new(spec(), Arc::clone(&pending), writer_tx);
        let ctx = ctx().await;
        let mut call = Box::pin(tool.execute(&ctx, serde_json::json!({"query": "q"})));
        // Poll until the frame is out, then abandon the call.
        tokio::select! {
            _ = &mut call => panic!("no reply was sent"),
            frame = writer_rx.recv() => assert!(frame.is_some()),
        }
        assert_eq!(pending.lock().await.len(), 1);
        drop(call);
        assert!(pending.lock().await.is_empty());
    }
}
