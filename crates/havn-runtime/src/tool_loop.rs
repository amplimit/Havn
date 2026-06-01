//! Reusable LLM ↔ tool loop, factored out so both the parent runtime
//! (handle_inbound / handle_heartbeat / handle_cron) and same-process
//! subagents (spec §4.5) share the same semantics.
//!
//! Why extract: the subagent_spawn tool needs to recursively run a tool
//! loop with a *different* tool registry (subagent_spawn itself stripped
//! per spec §4.5 rule 3) and a forked message vec, but otherwise the
//! exact same per-iteration logic — append assistant turn, dispatch
//! tool_use blocks, append tool_result user turn, repeat until
//! stop_reason != tool_use or the iteration cap is hit. Duplicating that
//! logic between main.rs and a subagent module would invite drift; one
//! function called from both places is the cheap correct answer.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use havn_proto::{AgentToGateway, LlmOutcome, LlmRequest, LlmResponse};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::system_prompt::SystemPrompt;
use crate::tools::{ToolCtx, ToolRegistry, ToolResult};

/// Shared map of in-flight LLM requests keyed by request_id. Both the
/// parent and any subagents register their oneshots here; the runtime's
/// reader loop dispatches `LlmResponse` frames into them.
pub type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<LlmResponse>>>>;

/// The subset of the runtime's plumbing a tool-use loop needs. Cheaply
/// cloned per call (everything is Arc / mpsc::Sender).
#[derive(Clone)]
pub struct LoopHandle {
    pub writer_tx: mpsc::Sender<AgentToGateway>,
    pub pending: Pending,
    pub registry: ToolRegistry,
    pub tool_env: ToolCtx,
    /// Hard cap on the LLM ↔ tool round-trip count for this loop. The
    /// parent uses [`crate::MAX_TOOL_ITERATIONS`]; subagents use the
    /// tighter [`crate::tools::subagent::SUBAGENT_MAX_ITERATIONS`].
    pub max_iterations: u32,
    /// When `Some`, the loop writes the current `messages` vec into this
    /// mirror before every LLM call. The parent runtime sets this to its
    /// `parent_messages` `RwLock` so that any `subagent_spawn` invoked in
    /// the LLM's response sees an up-to-date view (not stale from a prior
    /// turn). Subagents pass `None` — they don't republish their own
    /// state and recursion is forbidden anyway.
    pub messages_mirror: Option<Arc<RwLock<Vec<Value>>>>,
    /// Model the gateway resolved for this session (from the agent's
    /// `config.model`, fallback to [`DEFAULT_MODEL`]). Subagents
    /// inherit the parent's model — same agent, same compute budget.
    pub model: Arc<String>,
    /// Billing override for cross-agent queries (spec §4.4 v0.7). When
    /// `Some(user_id)` every [`LlmRequest`] this loop emits carries the
    /// override, so the gateway resolves credentials and records usage
    /// against the *caller's* owner, not this agent's owner. `None` for
    /// every loop except the IncomingQuery handler. The same value is
    /// also handed to subagent loops spawned within (caller pays
    /// recursively).
    pub billing_user_id: Option<String>,
}

/// Default token budget for an LLM call. Mirrors `main::DEFAULT_MAX_TOKENS`
/// — kept as a separate constant here so the loop module compiles standalone
/// in tests.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Default model when the per-call options don't override.
pub const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// Typed loop outcome — caller-distinguishable success vs. degraded paths.
///
/// [`run`] flattens [`Self::Llm`] and [`Self::IterationCap`] into a
/// single `String` (with `(LLM error: …)` / `(reached the N-iteration
/// tool-use cap…)` sentinel prefixes) for callers that only have one
/// channel to surface a result, and re-bubbles [`Self::Infra`] as
/// `Err(anyhow::Error)` so genuine infrastructure failures still
/// propagate. [`run_typed`] returns the structured form so callers like
/// `handle_incoming_query` can translate degraded outcomes into a real
/// error code (spec §4.4 `AgentQueryOutcome::Error`) instead of
/// dressing them up as a normal reply.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunError {
    /// The gateway's LLM proxy returned `LlmOutcome::Error` mid-loop.
    /// The string is verbatim from the proxy.
    Llm(String),
    /// `handle.max_iterations` round-trips elapsed without a stop. The
    /// included `partial_text` is whatever assistant text the loop had
    /// captured up to the cap — useful for surfacing a "we got this
    /// far" partial answer rather than nothing.
    IterationCap { cap: u32, partial_text: String },
    /// Infrastructure failure — couldn't even send the LLM request
    /// (writer_tx closed, oneshot dropped, …). Distinct from `Llm`
    /// because the LLM itself never spoke; the wrapper [`run`] re-
    /// bubbles this as `Err(anyhow::Error)` to preserve the v0.6
    /// contract that bare infra failures terminate the loop with an
    /// `Err` rather than getting smuggled into the success channel.
    Infra(anyhow::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Llm(msg) => write!(f, "LLM error: {msg}"),
            Self::IterationCap { cap, .. } => {
                write!(
                    f,
                    "reached the {cap}-iteration tool-use cap before finishing"
                )
            }
            Self::Infra(e) => write!(f, "infrastructure error: {e}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Typed loop runner. Same body as [`run`], but degraded outcomes
/// surface as [`RunError`] variants instead of being smuggled into
/// the success channel as sentinel strings.
///
/// Most callers want [`run`]'s flatten-to-String shape (heartbeat / cron
/// / parent-session / subagent all do). [`run_typed`] is for callers
/// that need to forward a structured outcome — currently only
/// `handle_incoming_query` (spec §4.4 v0.7), which translates `RunError`
/// into `AgentQueryOutcome::Error` so the calling agent's `tool_result`
/// gets `is_error: true` rather than an error-shaped success string.
pub async fn run_typed(
    handle: &LoopHandle,
    context: &'static str,
    messages: &mut Vec<Value>,
    system_prompt: &SystemPrompt,
) -> Result<String, RunError> {
    let mut last_text = String::new();
    for iteration in 0..handle.max_iterations {
        if let Some(mirror) = &handle.messages_mirror {
            *mirror.write().await = messages.clone();
        }
        let response = call_llm(handle, context, messages.clone(), system_prompt)
            .await
            .map_err(RunError::Infra)?;
        let provider_response = match response.outcome {
            LlmOutcome::Ok => response.provider_response,
            LlmOutcome::Error { message } => return Err(RunError::Llm(message)),
        };

        let stop_reason = provider_response
            .get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn")
            .to_string();
        let content = provider_response
            .get("content")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));

        // Always append the assistant turn so the next call has context.
        messages.push(serde_json::json!({ "role": "assistant", "content": content.clone() }));
        last_text = extract_text(&content).unwrap_or_default();

        if stop_reason != "tool_use" {
            return Ok(last_text);
        }

        let tool_results = execute_tool_uses(handle, context, &content).await;
        if tool_results.is_empty() {
            warn!(iteration, "stop_reason=tool_use but no tool_use blocks");
            return Ok(last_text);
        }
        messages.push(serde_json::json!({ "role": "user", "content": tool_results }));
    }

    Err(RunError::IterationCap {
        cap: handle.max_iterations,
        partial_text: last_text,
    })
}

/// Run the LLM ↔ tool loop until `stop_reason != "tool_use"` or
/// `handle.max_iterations` is reached. Returns the assistant's final
/// visible text.
///
/// LLM errors and iteration-cap exhaustion fold into the returned
/// `String` with sentinel prefixes — preserving the v0.6 single-channel
/// contract for parent / cron / heartbeat / subagent callers.
/// Callers that need to distinguish those degraded paths should use
/// [`run_typed`] and translate `RunError` themselves.
pub async fn run(
    handle: &LoopHandle,
    context: &'static str,
    messages: &mut Vec<Value>,
    system_prompt: &SystemPrompt,
) -> anyhow::Result<String> {
    match run_typed(handle, context, messages, system_prompt).await {
        Ok(text) => Ok(text),
        Err(RunError::Llm(msg)) => Ok(format!("(LLM error: {msg})")),
        Err(RunError::IterationCap { cap, .. }) => {
            warn!(max = cap, "tool-use loop hit iteration cap");
            Ok(format!(
                "(reached the {cap}-iteration tool-use cap before finishing)"
            ))
        }
        // Infrastructure failures (mpsc closed, oneshot dropped) propagate
        // as Err — the v0.6 wrapper raised these too, and downstream
        // handlers (heartbeat, cron, parent) log + reply (internal error).
        // Smuggling them into the success channel would silence genuine
        // bug reports.
        Err(RunError::Infra(e)) => Err(e),
    }
}

async fn execute_tool_uses(
    handle: &LoopHandle,
    context: &'static str,
    assistant_content: &Value,
) -> Vec<Value> {
    let Some(blocks) = assistant_content.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        let input = block.get("input").cloned().unwrap_or(Value::Null);

        debug!(name, id, context, "tool call");
        let result = handle
            .registry
            .execute_for(context, name, input, &handle.tool_env)
            .await;
        let (text, is_error) = match result {
            ToolResult::Ok(s) => (s, false),
            ToolResult::Error(s) => (s, true),
        };
        out.push(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": text,
            "is_error": is_error,
        }));
    }
    out
}

async fn call_llm(
    handle: &LoopHandle,
    context: &'static str,
    messages: Vec<Value>,
    system_prompt: &SystemPrompt,
) -> anyhow::Result<LlmResponse> {
    let request_id = Uuid::now_v7().to_string();
    let mut options = serde_json::json!({ "max_tokens": DEFAULT_MAX_TOKENS });
    if let Some(prompt) = system_prompt.as_text() {
        options["system"] = Value::String(prompt.to_string());
    }
    if !handle.registry.is_empty_for(context) {
        options["tools"] = handle.registry.schemas_for(context);
    }

    let llm_req = LlmRequest {
        request_id: request_id.clone(),
        model: handle.model.as_str().into(),
        messages: Value::Array(messages),
        options,
        billing_user_id: handle.billing_user_id.clone(),
    };

    let (resp_tx, resp_rx) = oneshot::channel();
    handle
        .pending
        .lock()
        .await
        .insert(request_id.clone(), resp_tx);

    if let Err(e) = handle
        .writer_tx
        .send(AgentToGateway::LlmRequest(llm_req))
        .await
    {
        handle.pending.lock().await.remove(&request_id);
        return Err(anyhow!("failed to enqueue LlmRequest: {e}"));
    }

    resp_rx
        .await
        .map_err(|_| anyhow!("LlmResponse channel closed for {request_id}"))
}

/// Concatenate `text` blocks from an Anthropic content array.
fn extract_text(content: &Value) -> Option<String> {
    let blocks = content.as_array()?;
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(t) = block.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
    }
    if text.is_empty() { None } else { Some(text) }
}
