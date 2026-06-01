//! `subagent_spawn` tool — same-process tokio-task subagents (spec §4.5).
//!
//! What the user sees: their main agent calls `subagent_spawn` once or
//! several times in a single assistant turn (Anthropic supports parallel
//! tool calls). Each subagent runs concurrently, sharing the parent's
//! frozen system prompt and (by default) the parent's recent conversation
//! turns. Each returns its final assistant text as a `tool_result`. The
//! parent then synthesises across results in its next turn.
//!
//! Why same-process: see spec §4.5 rationale. Short version: subagents
//! satisfy every spec §4.5 boundary rule for free in-process (parent
//! process death = OS reaps the whole tokio runtime; cgroup ceiling
//! already covers the whole process; no tmpfs to clean up). Spawn cost
//! is microseconds; prefix-cache stays warm because the subagent reuses
//! the parent's system prompt by `Arc::clone`.
//!
//! Boundary rule 3 (no recursion) is enforced two ways:
//! 1. The parent's [`crate::tools::ToolRegistry`] only contains
//!    `subagent_spawn` when `Bridge` is wired in. Subagent invocations
//!    construct their own loop with a registry that lacks the tool.
//! 2. Defence in depth: even if a stale `tool_use` block from a prior
//!    turn somehow asks for `subagent_spawn` inside a subagent, the
//!    `subagent` context's `disabled` set (preconfigurable in
//!    `policy.context_toolsets.subagent.disabled`) catches it at
//!    `execute_for` time.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::system_prompt::SystemPrompt;
use crate::tool_loop::{self, LoopHandle};
use crate::tools::{Tool, ToolCtx, ToolResult, context};

/// Hard cap on per-parent concurrent subagents (spec §4.5 rule 6).
/// Bounds a buggy / prompt-injected parent that asks for thousands at once.
pub const SUBAGENT_MAX_CONCURRENT: usize = 8;

/// Per-subagent iteration cap. Tighter than the parent's cap because a
/// subagent's job is by definition a bounded sub-task — if it's going past
/// 15 round trips it should hand back to the parent rather than spin its
/// own deeper loop. (Mirrors the constant previously declared in main.rs.)
pub const SUBAGENT_MAX_ITERATIONS: u32 = 15;

/// Default snapshot depth — number of most-recent parent turns the subagent
/// inherits when `context: "fork"` (the default). Tuned tight so token
/// overhead stays bounded; the user-facing rationale is in spec §4.5.
pub const FORK_HISTORY_DEPTH: usize = 10;

/// Shared state the [`SubagentSpawnTool`] uses to launch sub-loops.
/// Constructed once per parent runtime startup; cloned (cheaply, all Arc /
/// mpsc::Sender) into every subagent task that's spawned.
pub struct Bridge {
    /// Parent's frozen system prompt — shared by Arc, not deep-copied.
    /// This is what keeps prefix-cache hits free across parent + subagents.
    pub parent_system_prompt: Arc<SystemPrompt>,
    /// Loop handle for *child* tool loops: same writer_tx / pending /
    /// tool_env as the parent, but a registry that lacks `subagent_spawn`
    /// (spec §4.5 rule 3).
    pub child_handle: LoopHandle,
    /// Snapshot of the parent's message vec, refreshed by the parent loop
    /// before each LLM call. The subagent reads it once at spawn time when
    /// `context: "fork"`. `Arc<RwLock<...>>` so refresh is cheap and
    /// readers don't block each other.
    pub parent_messages: Arc<tokio::sync::RwLock<Vec<Value>>>,
    /// Concurrency limiter; see [`SUBAGENT_MAX_CONCURRENT`].
    pub semaphore: Arc<Semaphore>,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("subagent::Bridge")
            .field("max_concurrent", &SUBAGENT_MAX_CONCURRENT)
            .finish()
    }
}

/// The tool itself. Holds an `Arc<Bridge>` so dispatch is cheap.
#[derive(Clone)]
pub struct SubagentSpawnTool {
    bridge: Arc<Bridge>,
}

impl SubagentSpawnTool {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

#[derive(Debug, Deserialize)]
struct SpawnInput {
    /// What the subagent should do. Treated as the first user turn.
    task: String,
    /// "fork" (default) — subagent inherits the parent's last
    /// [`FORK_HISTORY_DEPTH`] turns.
    /// "isolated" — subagent starts with no prior turns; only the
    /// (shared) frozen system prompt.
    #[serde(default)]
    context: ContextHint,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ContextHint {
    #[default]
    Fork,
    Isolated,
}

#[async_trait]
impl Tool for SubagentSpawnTool {
    fn name(&self) -> &'static str {
        "subagent_spawn"
    }

    fn description(&self) -> &'static str {
        "Spawn a subagent to handle a self-contained sub-task in parallel with other work. \
         The subagent runs as the same agent (shares your persona, user knowledge, and memory) \
         and by default sees the last 10 conversation turns. Use this when you have multiple \
         independent items to handle at once (review N PRs, summarise N papers, check N \
         endpoints). Call subagent_spawn multiple times in a single assistant turn to run \
         them concurrently. Each returns its final assistant text. Subagents cannot themselves \
         spawn subagents."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "What the subagent should do (one self-contained sub-task)."
                },
                "context": {
                    "type": "string",
                    "enum": ["fork", "isolated"],
                    "default": "fork",
                    "description": "fork (default): inherit the last 10 parent turns. \
                                    isolated: start fresh with only the system prompt — \
                                    use for fully independent work like 'review PR #3'."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, _ctx: &ToolCtx, input: Value) -> ToolResult {
        let parsed: SpawnInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("invalid subagent_spawn input: {e}")),
        };
        if parsed.task.trim().is_empty() {
            return ToolResult::Error("subagent_spawn: `task` must be non-empty".into());
        }

        // Capacity gate — silently waits, but logs if the wait was non-trivial
        // so operators can tune SUBAGENT_MAX_CONCURRENT if it shows up.
        let _permit = match self.bridge.semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                debug!("subagent_spawn: at concurrency cap; waiting for capacity");
                match self.bridge.semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(e) => {
                        return ToolResult::Error(format!("subagent semaphore closed: {e}"));
                    }
                }
            }
        };

        // Build the subagent's initial message vec.
        // - "fork" (default): parent's recent turns + the new task as the
        //   final user turn.
        // - "isolated": just the task.
        let mut messages: Vec<Value> = if parsed.context == ContextHint::Isolated {
            Vec::with_capacity(1)
        } else {
            let snapshot = self.bridge.parent_messages.read().await.clone();
            let start = snapshot.len().saturating_sub(FORK_HISTORY_DEPTH);
            snapshot[start..].to_vec()
        };
        messages.push(serde_json::json!({
            "role": "user",
            "content": parsed.task.clone(),
        }));

        info!(
            context = ?parsed.context,
            history_len = messages.len() - 1,
            "subagent spawned"
        );

        let handle = self.bridge.child_handle.clone();
        let prompt = Arc::clone(&self.bridge.parent_system_prompt);
        match tool_loop::run(&handle, context::SUBAGENT, &mut messages, &prompt).await {
            Ok(text) => ToolResult::Ok(text),
            Err(e) => {
                warn!(error = %e, "subagent loop failed");
                ToolResult::Error(format!("subagent failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn schema_declares_required_task() {
        let t = SubagentSpawnTool {
            bridge: Arc::new(dummy_bridge_for_schema_only()),
        };
        let schema = t.input_schema();
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .expect("required array");
        assert!(required.iter().any(|v| v == "task"));
    }

    #[test]
    fn input_defaults_to_fork() {
        let parsed: SpawnInput = serde_json::from_value(serde_json::json!({
            "task": "do a thing"
        }))
        .expect("parse");
        assert_eq!(parsed.context, ContextHint::Fork);
    }

    #[test]
    fn input_accepts_isolated() {
        let parsed: SpawnInput = serde_json::from_value(serde_json::json!({
            "task": "do a thing",
            "context": "isolated"
        }))
        .expect("parse");
        assert_eq!(parsed.context, ContextHint::Isolated);
    }

    /// A bridge that's only valid for inspecting schemas / parsing input.
    /// The mpsc / pool / registry inside aren't usable for actual dispatch
    /// (the receiver is dropped immediately, etc.); used only by tests
    /// that call `schema()` or parse `input`.
    fn dummy_bridge_for_schema_only() -> Bridge {
        use crate::system_prompt::BootstrapFiles;
        use crate::tool_loop::LoopHandle;
        use crate::tools::ToolRegistry;
        use havn_core::Policy;
        use std::collections::HashMap;
        use tokio::sync::Mutex;

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let policy = Arc::new(Policy::default());
        let registry = ToolRegistry::standard(&policy);
        let pool = futures_executor_block_on(async {
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory sqlite")
        });
        let env = ToolCtx {
            workspace_dir: std::path::PathBuf::from("/tmp"),
            agent_db: pool,
            http: reqwest::Client::new(),
            policy: Arc::clone(&policy),
            embedder: None,
        };
        Bridge {
            parent_system_prompt: Arc::new(BootstrapFiles::default().system_prompt()),
            child_handle: LoopHandle {
                writer_tx: tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                registry,
                tool_env: env,
                max_iterations: SUBAGENT_MAX_ITERATIONS,
                messages_mirror: None,
                model: Arc::new(crate::tool_loop::DEFAULT_MODEL.to_string()),
                billing_user_id: None,
            },
            parent_messages: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            semaphore: Arc::new(Semaphore::new(SUBAGENT_MAX_CONCURRENT)),
        }
    }

    /// Tiny block_on for sync-context test helpers. Avoids dragging
    /// tokio_test into the cfg-test deps for one helper call.
    fn futures_executor_block_on<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(f)
    }
}
