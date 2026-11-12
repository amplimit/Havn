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
use std::time::Duration;

use anyhow::anyhow;
use havn_proto::codec::MAX_FRAME_BYTES;
use havn_proto::{
    AdminOutcome, AgentQueryOutcome, AgentQueryResponse, AgentQueryResult, AgentToGateway,
    DataResponse, FrameError, LlmOutcome, LlmRequest, LlmResponse, MemoryForgetResponse,
    SkillPinResponse, write_frame,
};
use serde_json::Value;
use tokio::io::AsyncWrite;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::system_prompt::SystemPrompt;
use crate::tools::agent_query::PendingQueries;
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
    pub max_tokens: u32,
    /// Billing override for cross-agent queries (spec §4.4 v0.7). When
    /// `Some(user_id)` every [`LlmRequest`] this loop emits carries the
    /// override, so the gateway resolves credentials and records usage
    /// against the *caller's* owner, not this agent's owner. `None` for
    /// every loop except the IncomingQuery handler. The same value is
    /// also handed to subagent loops spawned within (caller pays
    /// recursively).
    pub billing_user_id: Option<String>,
}

/// Default output-token budget for an agent LLM call when the agent's
/// `config.max_tokens` does not set one. Sized for tool use: a `file_write`
/// call carries the whole file in its input.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Default model when the per-call options don't override.
pub const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// Upper bound on waiting for the gateway's answer to one LLM request. The
/// gateway's provider calls time out long before this (120 s per
/// credential), so it only fires when an answer is lost.
pub const LLM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);

/// Stands in for a tool result removed to keep a request within
/// [`MAX_FRAME_BYTES`].
const ELIDED_TOOL_RESULT: &str =
    "[tool output removed: the conversation outgrew the size limit for one LLM request]";

/// Room left in a frame for the enum tag around an [`LlmRequest`].
const FRAME_TAG_BYTES: usize = 64;

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
        let response = call_llm(handle, context, messages, system_prompt)
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
    messages: &mut [Value],
    system_prompt: &SystemPrompt,
) -> anyhow::Result<LlmResponse> {
    let request_id = Uuid::now_v7().to_string();
    let mut options = serde_json::json!({ "max_tokens": handle.max_tokens });
    if let Some(prompt) = system_prompt.as_text() {
        options["system"] = Value::String(prompt.to_string());
    }
    if !handle.registry.is_empty_for(context) {
        options["tools"] = handle.registry.schemas_for(context);
    }

    let build = |messages: &[Value]| LlmRequest {
        request_id: request_id.clone(),
        model: handle.model.as_str().into(),
        messages: Value::Array(messages.to_vec()),
        options: options.clone(),
        billing_user_id: handle.billing_user_id.clone(),
    };
    let mut llm_req = build(messages);
    // The frame adds its `"type"` tag to the request's own JSON.
    let budget = MAX_FRAME_BYTES - FRAME_TAG_BYTES;
    let size = serde_json::to_vec(&llm_req)?.len();
    if size > budget {
        let excess = elide_tool_results(messages, size - budget);
        if excess > 0 {
            return Ok(LlmResponse {
                request_id,
                outcome: LlmOutcome::Error {
                    message: format!(
                        "the conversation is {excess} bytes over the {MAX_FRAME_BYTES}-byte \
                         limit for one LLM request, even without its tool results"
                    ),
                },
                provider_response: Value::Null,
                usage: None,
            });
        }
        warn!(
            size,
            "conversation over the frame limit; removed old tool results"
        );
        llm_req = build(messages);
    }

    request_llm(&handle.writer_tx, &handle.pending, llm_req).await
}

/// Replace tool results in `messages`, oldest first, with a short note until
/// at least `excess` bytes of serialized JSON are saved. Returns the bytes
/// still missing (0 once enough was removed).
fn elide_tool_results(messages: &mut [Value], mut excess: usize) -> usize {
    let note = Value::String(ELIDED_TOOL_RESULT.into());
    let note_len = json_len(&note);
    for message in messages.iter_mut() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks {
            if excess == 0 {
                return 0;
            }
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(content) = block.get_mut("content") else {
                continue;
            };
            let len = json_len(content);
            if len > note_len {
                *content = note.clone();
                excess = excess.saturating_sub(len - note_len);
            }
        }
    }
    excess
}

fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |v| v.len())
}

/// Send `req` and wait for its `LlmResponse`, at most [`LLM_RESPONSE_TIMEOUT`].
pub async fn request_llm(
    writer_tx: &mpsc::Sender<AgentToGateway>,
    pending: &Pending,
    req: LlmRequest,
) -> anyhow::Result<LlmResponse> {
    let request_id = req.request_id.clone();
    let (resp_tx, resp_rx) = oneshot::channel();
    pending.lock().await.insert(request_id.clone(), resp_tx);

    if let Err(e) = writer_tx.send(AgentToGateway::LlmRequest(req)).await {
        pending.lock().await.remove(&request_id);
        return Err(anyhow!("failed to enqueue LlmRequest: {e}"));
    }

    match tokio::time::timeout(LLM_RESPONSE_TIMEOUT, resp_rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err(anyhow!("LlmResponse channel closed for {request_id}")),
        Err(_) => {
            pending.lock().await.remove(&request_id);
            Err(anyhow!(
                "no LlmResponse for {request_id} within {}s",
                LLM_RESPONSE_TIMEOUT.as_secs()
            ))
        }
    }
}

/// Write one frame to the gateway. A frame over [`MAX_FRAME_BYTES`] is
/// rejected before anything is written, so the stream stays usable: whoever
/// waits on it gets an error (see [`oversized_frame`]).
pub async fn write_outbound<W>(
    write: &mut W,
    frame: AgentToGateway,
    pending: &Pending,
    queries: &PendingQueries,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let mut next = Some(frame);
    while let Some(frame) = next.take() {
        match write_frame(write, &frame).await {
            Ok(()) => {}
            Err(FrameError::FrameTooLarge(size)) => {
                error!(size, "outbound frame over the size limit");
                next = oversized_frame(frame, size, pending, queries).await;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Fail the local caller waiting on an unsendable request, or build the
/// smaller frame to send in place of an unsendable reply.
async fn oversized_frame(
    frame: AgentToGateway,
    size: usize,
    pending: &Pending,
    queries: &PendingQueries,
) -> Option<AgentToGateway> {
    let limit = format!("{size} bytes exceeds the {MAX_FRAME_BYTES}-byte frame limit");
    let admin_error = || AdminOutcome::Error {
        message: format!("reply of {limit}"),
    };
    match frame {
        AgentToGateway::LlmRequest(req) => {
            if let Some(tx) = pending.lock().await.remove(&req.request_id) {
                let _ = tx.send(LlmResponse {
                    request_id: req.request_id,
                    outcome: LlmOutcome::Error {
                        message: format!("LLM request of {limit}"),
                    },
                    provider_response: Value::Null,
                    usage: None,
                });
            }
            None
        }
        AgentToGateway::AgentQuery(query) => {
            if let Some(tx) = queries.lock().await.remove(&query.request_id) {
                let _ = tx.send(AgentQueryResult {
                    request_id: query.request_id,
                    outcome: AgentQueryOutcome::Error {
                        message: format!("agent query of {limit}"),
                    },
                    text: String::new(),
                    transcript: Value::Null,
                });
            }
            None
        }
        AgentToGateway::AgentQueryResponse(resp) if !resp.transcript.is_null() => {
            Some(AgentToGateway::AgentQueryResponse(AgentQueryResponse {
                transcript: Value::String(format!("transcript omitted: answer of {limit}")),
                ..resp
            }))
        }
        AgentToGateway::AgentQueryResponse(resp) => {
            Some(AgentToGateway::AgentQueryResponse(AgentQueryResponse {
                request_id: resp.request_id,
                outcome: AgentQueryOutcome::Error {
                    message: format!("answer of {limit}"),
                },
                text: String::new(),
                transcript: Value::Null,
            }))
        }
        AgentToGateway::MemoryForgetResponse(resp) => {
            Some(AgentToGateway::MemoryForgetResponse(MemoryForgetResponse {
                request_id: resp.request_id,
                outcome: admin_error(),
            }))
        }
        AgentToGateway::SkillPinResponse(resp) => {
            Some(AgentToGateway::SkillPinResponse(SkillPinResponse {
                request_id: resp.request_id,
                outcome: admin_error(),
            }))
        }
        AgentToGateway::DataResponse(resp) => Some(AgentToGateway::DataResponse(DataResponse {
            request_id: resp.request_id,
            outcome: admin_error(),
        })),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn request(request_id: &str) -> LlmRequest {
        LlmRequest {
            request_id: request_id.into(),
            model: DEFAULT_MODEL.into(),
            messages: serde_json::json!([{ "role": "user", "content": "hi" }]),
            options: serde_json::json!({}),
            billing_user_id: None,
        }
    }

    fn maps() -> (Pending, PendingQueries) {
        (
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    fn tool_turn(id: usize, size: usize) -> Value {
        serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": format!("t{id}"),
                "content": "x".repeat(size),
                "is_error": false,
            }],
        })
    }

    #[test]
    fn oldest_tool_results_are_removed_first() {
        let mut messages: Vec<Value> = (0..20).map(|i| tool_turn(i, 256 * 1024)).collect();
        let budget = MAX_FRAME_BYTES - FRAME_TAG_BYTES;
        let excess = json_len(&Value::Array(messages.clone())) - budget;

        assert_eq!(elide_tool_results(&mut messages, excess), 0);

        assert!(json_len(&Value::Array(messages.clone())) <= budget);
        assert_eq!(messages[0]["content"][0]["content"], ELIDED_TOOL_RESULT);
        assert_eq!(
            messages[19]["content"][0]["content"].as_str().map(str::len),
            Some(256 * 1024),
            "the newest tool result is kept"
        );
    }

    #[test]
    fn removal_reports_what_it_could_not_save() {
        let mut messages = vec![serde_json::json!({ "role": "user", "content": "x".repeat(1000) })];
        assert_eq!(elide_tool_results(&mut messages, 10), 10);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_an_llm_response_is_bounded() {
        let (tx, _rx) = mpsc::channel(8);
        let (pending, _) = maps();

        let err = request_llm(&tx, &pending, request("r1"))
            .await
            .expect_err("no answer ever arrives");

        assert!(err.to_string().contains("within"), "{err}");
        assert!(
            pending.lock().await.is_empty(),
            "the pending entry is removed"
        );
    }

    #[tokio::test]
    async fn oversized_llm_request_fails_its_caller() {
        let (tx, mut rx) = mpsc::channel(8);
        let (pending, queries) = maps();
        let mut req = request("big");
        req.messages =
            serde_json::json!([{ "role": "user", "content": "x".repeat(MAX_FRAME_BYTES) }]);
        let call = tokio::spawn({
            let pending = Arc::clone(&pending);
            async move { request_llm(&tx, &pending, req).await }
        });

        let frame = rx.recv().await.expect("queued");
        let mut wire = Vec::new();
        write_outbound(&mut wire, frame, &pending, &queries)
            .await
            .expect("the stream stays usable");
        assert!(wire.is_empty(), "nothing is written");

        let response = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("the caller is answered at once")
            .expect("join")
            .expect("response");
        assert!(
            matches!(response.outcome, LlmOutcome::Error { .. }),
            "{:?}",
            response.outcome
        );
    }

    #[tokio::test]
    async fn oversized_query_answer_loses_only_its_transcript() {
        let (pending, queries) = maps();
        let answer = AgentToGateway::AgentQueryResponse(AgentQueryResponse {
            request_id: "q".into(),
            outcome: AgentQueryOutcome::Ok,
            text: "answer".into(),
            transcript: serde_json::json!(["x".repeat(MAX_FRAME_BYTES)]),
        });

        let mut wire = Vec::new();
        write_outbound(&mut wire, answer, &pending, &queries)
            .await
            .expect("write");

        let mut reader = tokio::io::BufReader::new(wire.as_slice());
        match havn_proto::read_frame::<_, AgentToGateway>(&mut reader).await {
            Ok(Some(AgentToGateway::AgentQueryResponse(r))) => {
                assert!(
                    matches!(r.outcome, AgentQueryOutcome::Ok),
                    "{:?}",
                    r.outcome
                );
                assert_eq!(r.text, "answer");
                assert!(r.transcript.is_string(), "{:?}", r.transcript);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
