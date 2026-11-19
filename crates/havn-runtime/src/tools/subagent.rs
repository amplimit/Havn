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
    /// The models the agent may use (issue #222): what a subagent may be
    /// given. Empty from an older gateway.
    pub models: Arc<Vec<havn_proto::ModelChoice>>,
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
    /// The model it runs on (issue #222): one of the agent's, by id or
    /// label. Absent, the turn's.
    #[serde(default)]
    model: Option<String>,
    /// How much it thinks, as `/think` takes it, within what its model
    /// takes. Absent, the turn's choice on the turn's model, else the
    /// model's own default.
    #[serde(default)]
    thinking: Option<String>,
}

impl Bridge {
    /// The model `wanted` names: its id or its label, in any case.
    fn model(&self, wanted: &str) -> Option<&havn_proto::ModelChoice> {
        let lower = wanted.to_lowercase();
        self.models
            .iter()
            .find(|m| m.id.to_lowercase() == lower)
            .or_else(|| {
                self.models.iter().find(|m| {
                    m.label
                        .as_deref()
                        .is_some_and(|l| l.to_lowercase() == lower)
                })
            })
    }

    /// The models, one per line, with the thinking each takes.
    fn listed(&self) -> String {
        self.models
            .iter()
            .map(|m| {
                let mut line = m.id.clone();
                if let Some(label) = &m.label {
                    line.push_str(&format!(" ({label})"));
                }
                if m.thinking.is_empty() {
                    line.push_str(": no thinking");
                } else {
                    line.push_str(&format!(": thinking {}", m.thinking.join(", ")));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The thinking a subagent on `model` runs with: `wanted` when that model
/// takes it, else why not.
fn thinking_for(model: Option<&havn_proto::ModelChoice>, wanted: &str) -> Result<String, String> {
    let Some(model) = model else {
        // A model not in the list (the gateway's default, or an older
        // gateway): the gateway applies what the model takes.
        return Ok(wanted.to_string());
    };
    if model.thinking.is_empty() && wanted.eq_ignore_ascii_case("off") {
        return Ok("off".to_string());
    }
    if model.thinking.is_empty() {
        return Err(format!(
            "{} is not asked to think; leave thinking out",
            model.id
        ));
    }
    model
        .thinking
        .iter()
        .find(|t| t.eq_ignore_ascii_case(wanted))
        .cloned()
        .ok_or_else(|| {
            format!(
                "{} takes thinking {}, not {wanted:?}",
                model.id,
                model.thinking.join(", ")
            )
        })
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
         and by default sees the last 10 messages of the conversation. Use this when you have multiple \
         independent items to handle at once (review N PRs, summarise N papers, check N \
         endpoints). Call subagent_spawn multiple times in a single assistant turn to run \
         them concurrently. Each returns its final assistant text. You may give one a model \
         and thinking of its own (a cheaper model for simple work, more thinking for hard \
         work), and choose whether it sees the conversation so far. Subagents cannot \
         themselves spawn subagents."
    }

    fn input_schema(&self) -> Value {
        let model = if self.bridge.models.is_empty() {
            serde_json::json!({
                "type": "string",
                "description": "Not offered by this gateway: omit it; it runs on your model."
            })
        } else {
            serde_json::json!({
                "type": "string",
                "enum": self.bridge.models.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
                "description": format!(
                    "The model it runs on; omit for yours. The models you may use:\n{}",
                    self.bridge.listed()
                )
            })
        };
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
                    "description": "Whether it sees the conversation so far. fork (default): \
                                    its last 10 messages. isolated: none, only the system \
                                    prompt; use for fully independent work like 'review PR #3'."
                },
                "model": model,
                "thinking": {
                    "type": "string",
                    "description": "How much it thinks: one of the choices its model takes, \
                                    as listed under model (off, on, a level or a budget). \
                                    Omit for your own choice on your model, or its model's \
                                    default on another."
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
        let turn_model = tool_loop::TURN_MODEL
            .try_with(Arc::clone)
            .unwrap_or_else(|_| Arc::clone(&self.bridge.child_handle.model));
        let turn_model_id = turn_model.to_string();
        let chosen = match parsed
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            None => None,
            Some(wanted) => match self.bridge.model(wanted) {
                Some(choice) => Some(choice),
                None if self.bridge.models.is_empty() => {
                    return ToolResult::Error(
                        "subagent_spawn: this gateway offers no choice of model; omit model".into(),
                    );
                }
                None => {
                    return ToolResult::Error(format!(
                        "subagent_spawn: {wanted:?} is not a model you may use. The models:\n{}",
                        self.bridge.listed()
                    ));
                }
            },
        };
        let model = chosen.map_or(turn_model, |m| Arc::new(m.id.clone()));
        let thinking = match parsed
            .thinking
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(wanted) => {
                let of = chosen.or_else(|| self.bridge.model(&model));
                match thinking_for(of, wanted) {
                    Ok(t) => Some(t),
                    Err(why) => return ToolResult::Error(format!("subagent_spawn: {why}")),
                }
            }
            // The turn's own choice goes with the turn's model only.
            None if *model == turn_model_id => tool_loop::TURN_THINKING
                .try_with(Clone::clone)
                .ok()
                .flatten(),
            None => None,
        };
        let other_model = *model != turn_model_id;

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
            let snapshot = match tool_loop::FORK_FROM.try_with(Arc::clone) {
                Ok(mirror) => mirror.read().await.clone(),
                Err(_) => Vec::new(),
            };
            let mut start = snapshot.len().saturating_sub(FORK_HISTORY_DEPTH);
            // Not from a tool's result: the call it answers would be cut
            // off, and a model takes no result without its call.
            while snapshot.get(start).is_some_and(answers_a_call) {
                start += 1;
            }
            let mut forked = snapshot[start..].to_vec();
            // Reasoning is the turn's model's own: another model is given
            // the conversation without it.
            if other_model {
                for message in &mut forked {
                    if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                        blocks.retain(|b| {
                            !matches!(
                                b.get("type").and_then(Value::as_str),
                                Some("thinking" | "redacted_thinking")
                            )
                        });
                    }
                }
                forked.retain(|m| {
                    m.get("content")
                        .and_then(Value::as_array)
                        .is_none_or(|blocks| !blocks.is_empty())
                });
            }
            forked
        };
        messages.push(serde_json::json!({
            "role": "user",
            "content": parsed.task.clone(),
        }));

        info!(
            context = ?parsed.context,
            history_len = messages.len() - 1,
            model = %model,
            thinking = ?thinking,
            "subagent spawned"
        );

        // What it ran with heads its answer, so the step shows it.
        let ran_with = format!(
            "[Subagent on {}, thinking {}, {}]\n",
            if model.is_empty() {
                "the default model"
            } else {
                model.as_str()
            },
            thinking.as_deref().unwrap_or("as its model is set"),
            if parsed.context == ContextHint::Isolated {
                "without the conversation"
            } else {
                "with the conversation so far"
            }
        );
        let mut handle = self.bridge.child_handle.clone();
        handle.model = Arc::clone(&model);
        let prompt = Arc::clone(&self.bridge.parent_system_prompt);
        let run = tool_loop::TURN_MODEL.scope(
            model,
            tool_loop::TURN_THINKING.scope(
                thinking,
                tool_loop::run(&handle, context::SUBAGENT, &mut messages, &prompt),
            ),
        );
        match run.await {
            Ok(text) => ToolResult::Ok(format!("{ran_with}{text}")),
            Err(e) => {
                warn!(error = %e, "subagent loop failed");
                ToolResult::Error(format!("{ran_with}subagent failed: {e}"))
            }
        }
    }
}

/// Whether `message` is the results of tool calls made in the message
/// before it.
fn answers_a_call(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_fork_starts_after_results_cut_from_their_calls() {
        let result = serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
        ]});
        assert!(answers_a_call(&result));
        assert!(!answers_a_call(
            &serde_json::json!({"role": "user", "content": "hi"})
        ));
        assert!(!answers_a_call(
            &serde_json::json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "x", "input": {}},
            ]})
        ));
    }

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
    fn a_subagent_gets_a_listed_model_and_the_thinking_it_takes() {
        let bridge = dummy_bridge_for_schema_only();
        let deepseek = bridge.model("deepseek").expect("by label");
        assert_eq!(deepseek.id, "openrouter:deepseek/deepseek-v4");
        assert!(
            bridge.model("OPENAI:GPT-5-MINI").is_some(),
            "by id, any case"
        );
        assert!(bridge.model("gpt-4o").is_none());
        assert_eq!(thinking_for(Some(deepseek), "HIGH"), Ok("high".into()));
        assert!(thinking_for(Some(deepseek), "on").is_err());
        let mini = bridge.model("openai:gpt-5-mini");
        assert!(thinking_for(mini, "low").is_err(), "it does not think");
        assert_eq!(thinking_for(mini, "OFF"), Ok("off".into()));
        assert_eq!(
            thinking_for(None, "on"),
            Ok("on".into()),
            "the gateway decides"
        );
        let schema = SubagentSpawnTool {
            bridge: Arc::new(bridge),
        }
        .input_schema();
        assert_eq!(
            schema["properties"]["model"]["enum"][1],
            "openai:gpt-5-mini"
        );
        let listed = schema["properties"]["model"]["description"]
            .as_str()
            .expect("text");
        assert!(
            listed.contains("(DeepSeek): thinking off, low, high"),
            "{listed}"
        );
        assert!(listed.contains("gpt-5-mini: no thinking"));
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
            policy: Arc::clone(&policy),
            embedder: None,
        };
        Bridge {
            parent_system_prompt: Arc::new(BootstrapFiles::default().system_prompt(None)),
            child_handle: LoopHandle {
                writer_tx: tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                registry,
                tool_env: env,
                max_iterations: 15,
                messages_mirror: false,
                model: Arc::new(String::new()),
                max_tokens: crate::tool_loop::DEFAULT_MAX_TOKENS,
                billing_user_id: None,
                stop: None,
                segments: None,
            },
            models: Arc::new(vec![
                havn_proto::ModelChoice {
                    id: "openrouter:deepseek/deepseek-v4".into(),
                    label: Some("DeepSeek".into()),
                    thinking: vec!["off".into(), "low".into(), "high".into()],
                },
                havn_proto::ModelChoice {
                    id: "openai:gpt-5-mini".into(),
                    label: None,
                    thinking: Vec::new(),
                },
            ]),
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
