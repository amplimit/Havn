//! `agent_query` tool — same-owner cross-agent question/answer
//! (spec §4.4 v0.7).
//!
//! When an LLM in agent A invokes this tool with `{target_agent_id,
//! prompt}`, the runtime sends an [`AgentToGateway::AgentQuery`] frame
//! down the agent socket. The gateway broker validates same-owner,
//! checks the target is connected, dispatches an
//! [`GatewayToAgent::IncomingQuery`] to agent B, and routes B's reply
//! back as [`GatewayToAgent::AgentQueryResult`]. The tool's pending
//! oneshot wakes; we return the text (and optional transcript) as a
//! `tool_result`.
//!
//! Recursion guard: this tool is **not** registered when the runtime is
//! processing an `IncomingQuery` (caller-side registry only). Combined
//! with the gateway's caller-self check and the same-owner enforcement,
//! the call graph is bounded to depth 1.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use havn_proto::{AgentQuery, AgentQueryOutcome, AgentQueryResult, AgentToGateway};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::warn;
use uuid::Uuid;

use crate::tools::{Tool, ToolCtx, ToolResult};

/// Hard cap mirroring the gateway's [`AGENT_QUERY_MAX_TIMEOUT_SECS`].
/// Kept in sync deliberately — the gateway clamps too, but a shorter
/// runtime ceiling means an LLM that asks for an absurd timeout still
/// gets a useful tool_result instead of waiting full 5 minutes for
/// nothing.
const TIMEOUT_HARD_CAP_SECS: u32 = 300;

/// Pending map shared between the tool (registers oneshot) and the
/// runtime reader loop (resolves on `AgentQueryResult`).
pub type PendingQueries = Arc<Mutex<HashMap<String, oneshot::Sender<AgentQueryResult>>>>;

#[derive(Debug, Deserialize)]
struct Input {
    target_agent_id: String,
    prompt: String,
    #[serde(default = "default_timeout")]
    timeout_seconds: u32,
    #[serde(default)]
    include_transcript: bool,
}

fn default_timeout() -> u32 {
    60
}

#[derive(Clone)]
pub struct AgentQueryTool {
    pending: PendingQueries,
    writer_tx: mpsc::Sender<AgentToGateway>,
}

impl AgentQueryTool {
    pub fn new(pending: PendingQueries, writer_tx: mpsc::Sender<AgentToGateway>) -> Self {
        Self { pending, writer_tx }
    }
}

impl std::fmt::Debug for AgentQueryTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentQueryTool").finish()
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for AgentQueryTool {
    fn name(&self) -> &str {
        "agent_query"
    }

    fn description(&self) -> &str {
        "Ask another agent owned by the same user a question and get its answer back. \
         Useful when the other agent has memory, skills, or context that this agent doesn't \
         (e.g. a research agent, a code-review agent). Same-owner only — cross-owner \
         queries are refused. The target agent must be running. \
         Inputs: target_agent_id (uuid), prompt (string), timeout_seconds (default 60, max 300), \
         include_transcript (default false; when true the response includes the target's full \
         content blocks — text + tool_use + tool_result — alongside the final text)."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target_agent_id": {
                    "type": "string",
                    "description": "UUID of the agent to ask. Must be owned by the same user."
                },
                "prompt": {
                    "type": "string",
                    "description": "The question / instruction to send."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": TIMEOUT_HARD_CAP_SECS,
                    "default": 60
                },
                "include_transcript": {
                    "type": "boolean",
                    "default": false
                }
            },
            "required": ["target_agent_id", "prompt"]
        })
    }

    async fn execute(&self, _env: &ToolCtx, args: Value) -> ToolResult {
        let input: Input = match serde_json::from_value(args) {
            Ok(i) => i,
            Err(e) => return ToolResult::Error(format!("invalid arguments: {e}")),
        };
        if input.prompt.trim().is_empty() {
            return ToolResult::Error("prompt must be non-empty".into());
        }
        if Uuid::parse_str(&input.target_agent_id).is_err() {
            return ToolResult::Error(format!(
                "target_agent_id is not a valid UUID: {:?}",
                input.target_agent_id
            ));
        }
        let timeout_secs = input.timeout_seconds.clamp(1, TIMEOUT_HARD_CAP_SECS);

        // Register the oneshot before sending so a fast gateway can't
        // race us — if the result frame arrives before insertion, the
        // resolver warns "no pending" and the receiver here would hang
        // forever.
        let request_id = Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);

        let frame = AgentToGateway::AgentQuery(AgentQuery {
            request_id: request_id.clone(),
            target_agent_id: input.target_agent_id.clone(),
            prompt: input.prompt,
            timeout_seconds: timeout_secs,
            include_transcript: input.include_transcript,
        });
        if let Err(e) = self.writer_tx.send(frame).await {
            self.pending.lock().await.remove(&request_id);
            return ToolResult::Error(format!("failed to enqueue AgentQuery: {e}"));
        }

        // Cap the wait at slightly more than the gateway's timeout so
        // we always observe the gateway's `Timeout` outcome instead of
        // tearing down our oneshot first. +5s buys time for round-trip
        // delivery.
        let wait = std::time::Duration::from_secs(u64::from(timeout_secs) + 5);
        let result = match tokio::time::timeout(wait, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                // Sender was dropped without delivering — would only
                // happen on a runtime panic / abort or a stray
                // `pending.clear()` we don't currently call. Best-effort
                // cleanup of the pending entry to avoid map growth on
                // long-running gateways even though the future graph is
                // already broken.
                self.pending.lock().await.remove(&request_id);
                return ToolResult::Error("agent_query reply channel closed".into());
            }
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                warn!(
                    request_id,
                    target = %input.target_agent_id,
                    "agent_query exceeded local wait — gateway should have already timed out"
                );
                return ToolResult::Error(format!("agent_query timed out after {timeout_secs}s",));
            }
        };

        match result.outcome {
            AgentQueryOutcome::Ok => {
                if input.include_transcript && !result.transcript.is_null() {
                    let payload = serde_json::json!({
                        "text": result.text,
                        "transcript": result.transcript,
                    });
                    ToolResult::Ok(payload.to_string())
                } else {
                    ToolResult::Ok(result.text)
                }
            }
            AgentQueryOutcome::Error { message } => ToolResult::Error(message),
            AgentQueryOutcome::Timeout => {
                ToolResult::Error(format!("target agent did not reply within {timeout_secs}s",))
            }
            // The proto enum is `#[non_exhaustive]`; any future variant
            // an older runtime doesn't recognise becomes a generic error.
            _ => ToolResult::Error("agent_query: unrecognised outcome variant".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use std::sync::Arc;

    async fn empty_env() -> ToolCtx {
        // We don't actually exercise the env in agent_query — it goes
        // through the writer_tx + pending machinery — so a minimal stub
        // is fine.
        let policy = Arc::new(Policy::default());
        ToolCtx {
            workspace_dir: std::path::PathBuf::from("/tmp"),
            agent_db: sqlx::SqlitePool::connect(":memory:").await.unwrap(),
            http: reqwest::Client::new(),
            policy,
            embedder: None,
        }
    }

    #[tokio::test]
    async fn rejects_invalid_uuid() {
        let (tx, _rx) = mpsc::channel(8);
        let pending: PendingQueries = Arc::new(Mutex::new(HashMap::new()));
        let tool = AgentQueryTool::new(pending, tx);
        let env = empty_env().await;
        let r = tool
            .execute(
                &env,
                serde_json::json!({"target_agent_id": "not-a-uuid", "prompt": "hi"}),
            )
            .await;
        assert!(matches!(r, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn rejects_empty_prompt() {
        let (tx, _rx) = mpsc::channel(8);
        let pending: PendingQueries = Arc::new(Mutex::new(HashMap::new()));
        let tool = AgentQueryTool::new(pending, tx);
        let env = empty_env().await;
        let r = tool
            .execute(
                &env,
                serde_json::json!({
                    "target_agent_id": Uuid::now_v7().to_string(),
                    "prompt": "   "
                }),
            )
            .await;
        assert!(matches!(r, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn happy_path_returns_text() {
        let (tx, mut rx) = mpsc::channel(8);
        let pending: PendingQueries = Arc::new(Mutex::new(HashMap::new()));
        let tool = AgentQueryTool::new(Arc::clone(&pending), tx);
        let env = empty_env().await;

        // Stub the broker: when the AgentQuery frame is enqueued, mock
        // a reply by resolving the oneshot directly.
        let pending_for_task = Arc::clone(&pending);
        tokio::spawn(async move {
            let frame = rx.recv().await.expect("frame");
            let request_id = match frame {
                AgentToGateway::AgentQuery(q) => q.request_id,
                _ => panic!("expected AgentQuery"),
            };
            let sender = pending_for_task
                .lock()
                .await
                .remove(&request_id)
                .expect("pending entry");
            sender
                .send(AgentQueryResult {
                    request_id,
                    outcome: AgentQueryOutcome::Ok,
                    text: "the answer is 42".into(),
                    transcript: Value::Null,
                })
                .unwrap();
        });

        let r = tool
            .execute(
                &env,
                serde_json::json!({
                    "target_agent_id": Uuid::now_v7().to_string(),
                    "prompt": "what is the answer"
                }),
            )
            .await;
        match r {
            ToolResult::Ok(s) => assert_eq!(s, "the answer is 42"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }
}
