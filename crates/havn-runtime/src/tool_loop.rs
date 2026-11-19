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
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc, oneshot};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::system_prompt::SystemPrompt;
use crate::tools::agent_query::PendingQueries;
use crate::tools::{ToolCtx, ToolRegistry, ToolResult};

tokio::task_local! {
    /// The model of the turn whose tool loop runs (a chat's own, or the
    /// agent's when the turn began): a subagent it spawns uses it too.
    pub static TURN_MODEL: Arc<String>;
    /// The chat's thinking choice for the turn whose tool loop runs (issue
    /// #190); `None` for its model's default.
    pub static TURN_THINKING: Option<String>;
    /// What the conversation turn whose tool loop runs did on the way:
    /// its reasoning and tool calls, kept with its reply (issue #178).
    pub static TRACE: Arc<std::sync::Mutex<Trace>>;
    /// The stop of the turn whose tools run: a command it starts in the
    /// background ends with it (issue #215).
    pub static TURN_STOP: tokio_util::sync::CancellationToken;
    /// The messages of the loop whose tools run, as of its last model
    /// call: a subagent it forks starts from them. One per loop, so a
    /// subagent never starts from another chat's turn running at the same
    /// time.
    pub static FORK_FROM: Arc<RwLock<Vec<Value>>>;
}

/// Entries one turn's trace keeps at most, characters of all its parts,
/// and of each part.
const TRACE_ENTRIES: usize = 60;
const TRACE_TOTAL_CHARS: usize = 48_000;
const TRACE_THINKING_CHARS: usize = 8_000;
const TRACE_INPUT_CHARS: usize = 1_500;
const TRACE_RESULT_CHARS: usize = 2_000;

/// A turn's reasoning and tool calls, in order.
#[derive(Debug, Default)]
pub struct Trace {
    pub items: Vec<Value>,
    /// Entries that did not fit.
    pub left_out: usize,
    /// The gateway left part of a request out for it to fit the model's
    /// context window (issue #214): the chat is due to be compacted.
    pub trimmed: bool,
    /// The model that answers in place of the turn's, which was
    /// unavailable (issue #216): the turn's later calls go to it.
    pub fallback: Option<String>,
    chars: usize,
}

impl Trace {
    fn push(&mut self, item: Value, chars: usize) {
        if self.items.len() < TRACE_ENTRIES && self.chars + chars <= TRACE_TOTAL_CHARS {
            self.items.push(item);
            self.chars += chars;
        } else {
            self.left_out += 1;
        }
    }
}

/// `text` cut to `max` characters, marked when cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

/// Take the thinking blocks out of the assistant messages before the last
/// message someone wrote (one that is not only tool results): the turn
/// being answered keeps its reasoning, as Anthropic and DeepSeek want it
/// back with each tool call.
fn drop_thinking(messages: &mut [Value]) {
    let tool_results_only = |m: &Value| {
        m.get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                !blocks.is_empty()
                    && blocks
                        .iter()
                        .all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            })
    };
    let turn = messages
        .iter()
        .rposition(|m| {
            m.get("role").and_then(Value::as_str) == Some("user") && !tool_results_only(m)
        })
        .unwrap_or(0);
    for message in messages[..turn].iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            blocks.retain(|b| {
                !matches!(
                    b.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                )
            });
        }
    }
}

/// Add to the running turn's trace, when one is kept; `chars` is the
/// length of its text.
fn trace(item: Value, chars: usize) {
    let _ = TRACE.try_with(|t| {
        t.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(item, chars);
    });
}

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
    /// Hard cap on the LLM ↔ tool round-trip count for this loop: the
    /// policy's `tool_rounds.max` for a message, `tool_rounds.subagent`
    /// for a subagent's task or another agent's question (issue #219).
    pub max_iterations: u32,
    /// Whether the loop keeps its messages in [`FORK_FROM`] before every
    /// model call, for a `subagent_spawn` in the reply to fork from. A
    /// subagent's own loop does not: it cannot spawn one.
    pub messages_mirror: bool,
    /// The agent's `config.model`, or empty for the gateway's default
    /// model. Subagents inherit the parent's model — same agent, same
    /// compute budget.
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
    /// Cancelled when the user stops the turn: the loop ends at once,
    /// dropping the model call or tool call in flight.
    pub stop: Option<tokio_util::sync::CancellationToken>,
    /// Where the text the model writes before calling a tool goes at once
    /// (issue #212): a chat app's conversation, which gets it as a message
    /// while the tools run. `None` keeps it for the reply alone.
    pub segments: Option<Segments>,
}

/// A chat app's conversation, which gets each part of a reply as it is
/// written (issue #212), and what it got.
#[derive(Clone)]
pub struct Segments {
    pub channel_id: String,
    pub chat: String,
    pub sent: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Segments {
    #[must_use]
    pub fn new(channel_id: String, chat: String) -> Self {
        Self {
            channel_id,
            chat,
            sent: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Send `text`, a part of the reply with more to come.
    async fn send(&self, writer_tx: &mpsc::Sender<AgentToGateway>, text: &str) {
        let outbound = havn_core::OutboundMessage {
            channel_id: self.channel_id.clone(),
            conversation: Some(self.chat.clone()),
            content: havn_core::MessageContent::Text {
                text: text.to_string(),
            },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: true,
        };
        if writer_tx
            .send(AgentToGateway::OutboundMessage(outbound))
            .await
            .is_ok()
        {
            self.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(text.to_string());
        }
    }

    /// What of `reply` (every part, joined) was not sent yet: all of it
    /// when it does not start with what was.
    #[must_use]
    pub fn rest_of(&self, reply: &str) -> String {
        let sent = self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .join(SEGMENT_BREAK);
        if sent.is_empty() {
            return reply.to_string();
        }
        match reply.strip_prefix(&sent) {
            Some(rest) => rest.trim_start().to_string(),
            None => reply.to_string(),
        }
    }
}

/// What separates the parts of a reply.
pub const SEGMENT_BREAK: &str = "\n\n";

/// Default output-token budget for an agent LLM call when the agent's
/// `config.max_tokens` does not set one. Sized for tool use: a `file_write`
/// call carries the whole file in its input.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

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

/// Replacements tried for one outbound frame over the size limit (a query
/// answer first loses its transcript, then becomes an error); then it is
/// dropped.
const MAX_SUBSTITUTIONS: usize = 2;

/// LLM requests this runtime keeps outstanding at once, the same as the
/// gateway's per-agent cap: further calls wait here rather than being
/// refused by the gateway.
const MAX_INFLIGHT_LLM_REQUESTS: usize = 16;
static LLM_SLOTS: Semaphore = Semaphore::const_new(MAX_INFLIGHT_LLM_REQUESTS);

/// Typed loop outcome — caller-distinguishable success vs. degraded paths.
///
/// [`run`] flattens [`Self::Llm`] and [`Self::IterationCap`] into a
/// single `String` (with `(LLM error: …)` / `(Stopped after N tool
/// rounds…)` sentinel prefixes) for callers that only have one
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
    /// The user stopped the turn; `partial_text` is the assistant text
    /// so far.
    Stopped { partial_text: String },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped { .. } => write!(f, "stopped by the user"),
            Self::Llm(msg) => write!(f, "LLM error: {msg}"),
            Self::IterationCap { cap, .. } => {
                write!(f, "stopped after {cap} tool rounds, the limit")
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
/// Most callers want [`run`]'s flatten-to-String shape (heartbeat /
/// parent-session / subagent all do). [`run_typed`] is for callers
/// that need to forward a structured outcome: `handle_incoming_query`
/// (spec §4.4 v0.7), which translates `RunError` into
/// `AgentQueryOutcome::Error` so the calling agent's `tool_result` gets
/// `is_error: true` rather than an error-shaped success string, and
/// `handle_cron`, which reports a failed run as `CronOutcome::Error`.
pub async fn run_typed(
    handle: &LoopHandle,
    context: &'static str,
    messages: &mut Vec<Value>,
    system_prompt: &SystemPrompt,
) -> Result<String, RunError> {
    if handle.messages_mirror {
        let mirror = Arc::new(RwLock::new(Vec::new()));
        return FORK_FROM
            .scope(mirror, rounds(handle, context, messages, system_prompt))
            .await;
    }
    rounds(handle, context, messages, system_prompt).await
}

async fn rounds(
    handle: &LoopHandle,
    context: &'static str,
    messages: &mut Vec<Value>,
    system_prompt: &SystemPrompt,
) -> Result<String, RunError> {
    // Every part the model wrote, in order (issue #212): the reply is all
    // of them, not only the last.
    let mut said: Vec<String> = Vec::new();
    let stopped = |said: &[String]| RunError::Stopped {
        partial_text: said.join(SEGMENT_BREAK),
    };
    for iteration in 0..handle.max_iterations {
        if handle.messages_mirror
            && let Ok(mirror) = FORK_FROM.try_with(Arc::clone)
        {
            *mirror.write().await = messages.clone();
        }
        let response = unless_stopped(
            handle.stop.as_ref(),
            call_llm(handle, context, messages, system_prompt),
        )
        .await
        .ok_or_else(|| stopped(&said))?
        .map_err(RunError::Infra)?;
        if response.trimmed {
            let _ = TRACE.try_with(|trace| {
                trace
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .trimmed = true;
            });
        }
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
        // The model's reasoning, kept for the dashboard. It stays in this
        // turn's messages, as Anthropic's thinking with tools needs; the
        // adapters of other providers leave it out, and later turns are
        // built from the stored replies, which never hold it.
        if context == crate::tools::context::WEBCHAT {
            for block in content.as_array().into_iter().flatten() {
                if block.get("type").and_then(Value::as_str) == Some("thinking")
                    && let Some(text) = block.get("thinking").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    let text = clip(text.trim(), TRACE_THINKING_CHARS);
                    let chars = text.chars().count();
                    trace(
                        serde_json::json!({ "kind": "thinking", "text": text }),
                        chars,
                    );
                }
            }
        }

        // Only this turn keeps its thinking: its tool calls need it back,
        // and older turns' only fill the frame.
        drop_thinking(messages);
        // Always append the assistant turn so the next call has context.
        messages.push(serde_json::json!({ "role": "assistant", "content": content.clone() }));
        let text = extract_text(&content).unwrap_or_default();
        let text = text.trim();
        if !text.is_empty() {
            said.push(text.to_string());
        }

        if stop_reason != "tool_use" {
            return Ok(said.join(SEGMENT_BREAK));
        }
        // What it said before calling a tool reaches a chat app now.
        if !text.is_empty()
            && let Some(segments) = &handle.segments
        {
            segments.send(&handle.writer_tx, text).await;
        }

        let tool_results = unless_stopped(handle.stop.as_ref(), async {
            match &handle.stop {
                Some(stop) => {
                    TURN_STOP
                        .scope(stop.clone(), execute_tool_uses(handle, context, &content))
                        .await
                }
                None => execute_tool_uses(handle, context, &content).await,
            }
        })
        .await
        .ok_or_else(|| stopped(&said))?;
        if tool_results.is_empty() {
            warn!(iteration, "stop_reason=tool_use but no tool_use blocks");
            return Ok(said.join(SEGMENT_BREAK));
        }
        messages.push(serde_json::json!({ "role": "user", "content": tool_results }));
    }

    // The rounds are spent: the model is asked once more to sum up what it
    // did and what is left (issue #219), told not to call tools. Its text
    // is kept; a call it makes all the same is not run. The instruction is
    // taken back after, whatever came of it: it is not part of the chat.
    let asked = messages
        .last_mut()
        .filter(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_array_mut())
        .map(|blocks| {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": format!(
                    "[You have used the {} tool rounds allowed here. Do not call any tool now: \
                     sum up what you did, what you found, and what is left to do.]",
                    handle.max_iterations
                ),
            }));
        })
        .is_some();
    if asked {
        let summed = unless_stopped(
            handle.stop.as_ref(),
            call_llm(handle, context, messages, system_prompt),
        )
        .await;
        if let Some(blocks) = messages
            .last_mut()
            .and_then(|m| m["content"].as_array_mut())
        {
            blocks.pop();
        }
        let summed = summed.ok_or_else(|| stopped(&said))?;
        match summed {
            Ok(response) => {
                if response.trimmed {
                    let _ = TRACE.try_with(|trace| {
                        trace
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .trimmed = true;
                    });
                }
                match response.outcome {
                    LlmOutcome::Ok => {
                        let mut content = response
                            .provider_response
                            .get("content")
                            .cloned()
                            .unwrap_or_else(|| Value::Array(Vec::new()));
                        if let Some(blocks) = content.as_array_mut() {
                            blocks.retain(|b| {
                                b.get("type").and_then(Value::as_str) != Some("tool_use")
                            });
                        }
                        let text = extract_text(&content).unwrap_or_default();
                        let text = text.trim();
                        if !text.is_empty() {
                            said.push(text.to_string());
                            messages.push(
                                serde_json::json!({ "role": "assistant", "content": content }),
                            );
                        }
                    }
                    LlmOutcome::Error { message } => {
                        warn!(error = %message, "the model could not sum up at the round limit");
                    }
                }
            }
            Err(e) => warn!(error = %e, "the model could not sum up at the round limit"),
        }
    }

    Err(RunError::IterationCap {
        cap: handle.max_iterations,
        partial_text: said.join(SEGMENT_BREAK),
    })
}

/// Run the LLM ↔ tool loop until `stop_reason != "tool_use"` or
/// `handle.max_iterations` is reached. Returns the assistant's final
/// visible text.
///
/// LLM errors and iteration-cap exhaustion fold into the returned
/// `String` with sentinel prefixes — preserving the v0.6 single-channel
/// contract for parent / heartbeat / subagent callers.
/// Callers that need to distinguish those degraded paths should use
/// [`run_typed`] and translate `RunError` themselves.
pub async fn run(
    handle: &LoopHandle,
    context: &'static str,
    messages: &mut Vec<Value>,
    system_prompt: &SystemPrompt,
) -> anyhow::Result<String> {
    as_text(run_typed(handle, context, messages, system_prompt).await)
}

/// A loop's outcome as the one text a caller with one channel shows: an
/// LLM error, the round limit or a stop said in the text; an
/// infrastructure failure an error.
pub fn as_text(outcome: Result<String, RunError>) -> anyhow::Result<String> {
    match outcome {
        Ok(text) => Ok(text),
        Err(RunError::Llm(msg)) => Ok(format!("(LLM error: {msg})")),
        Err(RunError::IterationCap { cap, partial_text }) => {
            warn!(max = cap, "tool-use loop hit iteration cap");
            let note = format!("(Stopped after {cap} tool rounds, the limit here.)");
            // What it said on the way stays part of the reply (issue #212).
            let said = partial_text.trim();
            Ok(if said.is_empty() {
                note
            } else {
                format!("{said}{SEGMENT_BREAK}{note}")
            })
        }
        // Infrastructure failures (mpsc closed, oneshot dropped) propagate
        // as Err — the v0.6 wrapper raised these too, and downstream
        // handlers (heartbeat, cron, parent) log + reply (internal error).
        // Smuggling them into the success channel would silence genuine
        // bug reports.
        Err(RunError::Infra(e)) => Err(e),
        Err(RunError::Stopped { partial_text }) => Ok(stopped_reply(&partial_text)),
    }
}

/// What a stopped turn leaves as its reply: the text so far, marked.
#[must_use]
pub fn stopped_reply(partial_text: &str) -> String {
    let partial = partial_text.trim();
    if partial.is_empty() {
        "(stopped)".to_string()
    } else {
        format!("{partial}\n\n(stopped)")
    }
}

/// `work`, unless `stop` is cancelled first: then `None`, and `work` is
/// dropped, which ends a running shell command and abandons a model call.
async fn unless_stopped<T>(
    stop: Option<&tokio_util::sync::CancellationToken>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    match stop {
        Some(stop) => tokio::select! {
            biased;
            () = stop.cancelled() => None,
            out = work => Some(out),
        },
        None => Some(work.await),
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
        if context == crate::tools::context::WEBCHAT {
            show_step(handle, name, &input);
        }
        let result = handle
            .registry
            .execute_for(context, name, input, &handle.tool_env)
            .await;
        let (text, is_error) = match result {
            ToolResult::Ok(s) => (s, false),
            ToolResult::Error(s) => (s, true),
        };
        if context == crate::tools::context::WEBCHAT {
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            let input = clip(&input.to_string(), TRACE_INPUT_CHARS);
            let result = clip(&text, TRACE_RESULT_CHARS);
            let chars = input.chars().count() + result.chars().count();
            trace(
                serde_json::json!({
                    "kind": "step",
                    "tool": name,
                    "input": input,
                    "result": result,
                    "error": is_error,
                }),
                chars,
            );
        }
        out.push(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": text,
            "is_error": is_error,
        }));
    }
    out
}

/// Tell the conversation the turn answers which tool it is about to call
/// and on what. Never waits, and leaves most of the queue to the gateway
/// to what must arrive: a step that does not fit is not shown.
fn show_step(handle: &LoopHandle, tool: &str, input: &Value) {
    if handle.writer_tx.capacity() * 2 < handle.writer_tx.max_capacity() {
        return;
    }
    let Ok(channel_id) = crate::tools::gateway::CONVERSATION.try_with(Clone::clone) else {
        return;
    };
    let conversation = crate::tools::gateway::CHAT.try_with(Clone::clone).ok();
    let _ = handle
        .writer_tx
        .try_send(AgentToGateway::Step(havn_proto::Step {
            channel_id,
            conversation,
            tool: tool.to_string(),
            detail: step_detail(input),
        }));
}

/// What a tool call works on, as one line: the first of the fields that
/// name it, never a file's or message's content.
fn step_detail(input: &Value) -> String {
    const NAMING: [&str; 8] = [
        "command", "query", "url", "path", "key", "name", "prompt", "task",
    ];
    let Some(text) = NAMING
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
    else {
        return String::new();
    };
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = line
        .chars()
        .take(havn_proto::MAX_STEP_DETAIL_CHARS)
        .collect();
    if out.len() < line.len() {
        out.push('…');
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
    if let Ok(Some(thinking)) = TURN_THINKING.try_with(Clone::clone) {
        options["thinking"] = Value::String(thinking);
    }
    if let Some(prompt) = system_prompt.as_text() {
        options["system"] = Value::String(prompt.to_string());
    }
    if !handle.registry.is_empty_for(context) {
        options["tools"] = handle.registry.schemas_for(context);
    }

    // A turn answering someone shows its text as the model writes it.
    let reply = (context == crate::tools::context::WEBCHAT)
        .then(|| {
            crate::tools::gateway::CONVERSATION
                .try_with(Clone::clone)
                .ok()
        })
        .flatten()
        .map(|channel_id| havn_proto::ReplyStream {
            channel_id,
            conversation: crate::tools::gateway::CHAT.try_with(Clone::clone).ok(),
        });
    // A model that answers in place of the turn's goes on for the turn
    // (issue #216), without thinking: it may not take the turn's reasoning
    // so far, nor its own replies of this turn without any.
    let fallback = TRACE
        .try_with(|trace| {
            trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fallback
                .clone()
        })
        .ok()
        .flatten();
    if fallback.is_some() {
        options["thinking"] = Value::String("off".into());
    }
    let mut model = fallback.unwrap_or_else(|| handle.model.as_str().to_string());
    let build = |messages: &[Value], model: &str| LlmRequest {
        request_id: request_id.clone(),
        model: model.into(),
        messages: Value::Array(messages.to_vec()),
        options: options.clone(),
        billing_user_id: handle.billing_user_id.clone(),
        reply: reply.clone(),
    };
    let mut llm_req = build(messages, &model);
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
                trimmed: false,
                fallback: None,
            });
        }
        warn!(
            size,
            "conversation over the frame limit; removed old tool results"
        );
        llm_req = build(messages, &model);
    }

    // A stopped turn drops this future: the gateway is told, and stops
    // showing the call's text.
    let mut cancel = CancelOnDrop {
        writer_tx: &handle.writer_tx,
        request_id: llm_req.reply.is_some().then(|| request_id.clone()),
    };
    let mut response = request_llm(&handle.writer_tx, &handle.pending, llm_req).await;
    // The model was down: its fallback answers, a few in turn at most.
    let mut asked = vec![model.clone()];
    while let Ok(answer) = &response
        && matches!(answer.outcome, LlmOutcome::Error { .. })
        && let Some(next) = answer.fallback.clone()
        && !asked.contains(&next)
        && asked.len() <= MAX_FALLBACKS
    {
        warn!(from = %model, to = %next, "the model is unavailable; asking its fallback");
        model.clone_from(&next);
        asked.push(next);
        let mut retry = build(messages, &model);
        retry.request_id = Uuid::now_v7().to_string();
        // Another model may not take the turn's reasoning so far (an
        // Anthropic model with thinking on wants its own in the last
        // assistant turn): it answers without thinking.
        retry.options["thinking"] = Value::String("off".into());
        cancel.request_id = retry.reply.is_some().then(|| retry.request_id.clone());
        response = request_llm(&handle.writer_tx, &handle.pending, retry).await;
        // It answered: the turn's later calls go to it, and the person is
        // told.
        if matches!(&response, Ok(answer) if matches!(answer.outcome, LlmOutcome::Ok)) {
            let _ = TRACE.try_with(|trace| {
                trace
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fallback = Some(model.clone());
            });
        }
    }
    cancel.request_id = None;
    response
}

/// Fallback models one call goes through at most (issue #216).
const MAX_FALLBACKS: usize = 3;

/// Sends [`AgentToGateway::LlmCancel`] for its call when dropped before
/// the call's answer came.
struct CancelOnDrop<'a> {
    writer_tx: &'a mpsc::Sender<AgentToGateway>,
    request_id: Option<String>,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(request_id) = self.request_id.take() {
            let _ = self
                .writer_tx
                .try_send(AgentToGateway::LlmCancel { request_id });
        }
    }
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
    let _slot = LLM_SLOTS
        .acquire()
        .await
        .map_err(|e| anyhow!("LLM request slots closed: {e}"))?;
    let request_id = req.request_id.clone();
    let (resp_tx, resp_rx) = oneshot::channel();
    pending.lock().await.insert(request_id.clone(), resp_tx);
    let _entry = PendingEntry {
        pending,
        request_id: request_id.clone(),
    };

    writer_tx
        .send(AgentToGateway::LlmRequest(req))
        .await
        .map_err(|e| anyhow!("failed to enqueue LlmRequest: {e}"))?;

    match tokio::time::timeout(LLM_RESPONSE_TIMEOUT, resp_rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err(anyhow!("LlmResponse channel closed for {request_id}")),
        Err(_) => Err(anyhow!(
            "no LlmResponse for {request_id} within {}s",
            LLM_RESPONSE_TIMEOUT.as_secs()
        )),
    }
}

/// Removes a request's pending entry however its call ends, including when
/// the calling future is dropped.
pub(crate) struct PendingEntry<'a, T: Send + 'static> {
    pub(crate) pending: &'a Arc<Mutex<HashMap<String, oneshot::Sender<T>>>>,
    pub(crate) request_id: String,
}

impl<T: Send + 'static> Drop for PendingEntry<'_, T> {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.try_lock() {
            map.remove(&self.request_id);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let pending = Arc::clone(self.pending);
            let request_id = std::mem::take(&mut self.request_id);
            runtime.spawn(async move {
                pending.lock().await.remove(&request_id);
            });
        }
    }
}

/// Write one frame to the gateway. A frame over [`MAX_FRAME_BYTES`] is
/// rejected before anything is written, so the stream stays usable: whoever
/// waits on it gets an error (see [`oversized_frame`]). A replacement that
/// is still too large is tried at most [`MAX_SUBSTITUTIONS`] times.
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
    let mut substitutions = 0;
    while let Some(frame) = next.take() {
        match write_frame(write, &frame).await {
            Ok(()) => {}
            Err(FrameError::FrameTooLarge(size)) if substitutions < MAX_SUBSTITUTIONS => {
                error!(size, "outbound frame over the size limit");
                substitutions += 1;
                next = oversized_frame(frame, size, substitutions, pending, queries).await;
            }
            Err(FrameError::FrameTooLarge(size)) => {
                error!(
                    size,
                    "replacement frame is still over the size limit; dropping it"
                );
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Fail the local caller waiting on an unsendable request, or build the
/// smaller frame to send in place of an unsendable reply. `attempt` counts
/// the replacements tried for this frame, from 1.
async fn oversized_frame(
    frame: AgentToGateway,
    size: usize,
    attempt: usize,
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
                    trimmed: false,
                    fallback: None,
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
        AgentToGateway::AgentQueryResponse(resp) if attempt == 1 && !resp.transcript.is_null() => {
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
pub(crate) fn extract_text(content: &Value) -> Option<String> {
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

    #[test]
    fn only_the_turn_being_answered_keeps_its_thinking() {
        let tool_call = |id: &str, thought: &str| {
            serde_json::json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": thought, "signature": "s"},
                {"type": "tool_use", "id": id, "name": "shell", "input": {}}
            ]})
        };
        let result = |id: &str| serde_json::json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "ok"}]});
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "earlier"}),
            tool_call("t0", "old"),
            result("t0"),
            serde_json::json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "old too"}, {"type": "text", "text": "done"}]}),
            serde_json::json!({"role": "user", "content": "now"}),
            tool_call("t1", "first"),
            result("t1"),
            tool_call("t2", "second"),
            result("t2"),
        ];
        drop_thinking(&mut messages);
        assert_eq!(messages[1]["content"].as_array().map(Vec::len), Some(1));
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[3]["content"][0]["type"], "text");
        assert_eq!(messages[5]["content"][0]["thinking"], "first");
        assert_eq!(messages[7]["content"][0]["thinking"], "second");
        assert_eq!(messages[8]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn a_step_names_what_a_call_works_on_never_its_content() {
        let detail = |v: Value| step_detail(&v);
        assert_eq!(
            detail(serde_json::json!({"command": "ls -la
  /tmp"})),
            "ls -la /tmp"
        );
        assert_eq!(
            detail(serde_json::json!({"path": "notes.md", "content": "secret"})),
            "notes.md"
        );
        assert_eq!(detail(serde_json::json!({"content": "secret"})), "");
        let long = detail(serde_json::json!({"query": "x".repeat(500)}));
        assert_eq!(long.chars().count(), havn_proto::MAX_STEP_DETAIL_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn stopped_replies_keep_the_text_so_far() {
        assert_eq!(stopped_reply("  "), "(stopped)");
        assert_eq!(
            stopped_reply("half an answer"),
            "half an answer

(stopped)"
        );
    }

    #[tokio::test]
    async fn stopping_drops_the_work_in_flight() {
        let stop = tokio_util::sync::CancellationToken::new();
        assert_eq!(unless_stopped(Some(&stop), async { 1 }).await, Some(1));
        stop.cancel();
        let never = std::future::pending::<u8>();
        assert_eq!(unless_stopped(Some(&stop), never).await, None);
        assert_eq!(unless_stopped(None, async { 2 }).await, Some(2));
    }

    fn request(request_id: &str) -> LlmRequest {
        LlmRequest {
            request_id: request_id.into(),
            model: String::new(),
            messages: serde_json::json!([{ "role": "user", "content": "hi" }]),
            options: serde_json::json!({}),
            billing_user_id: None,
            reply: None,
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
    async fn at_the_round_limit_the_model_sums_up() {
        use crate::tools::{ToolCtx, ToolRegistry};
        let (tx, mut rx) = mpsc::channel(8);
        let (pending, _) = maps();
        let policy = Arc::new(havn_core::Policy::default());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let handle = LoopHandle {
            writer_tx: tx,
            pending: Arc::clone(&pending),
            registry: ToolRegistry::standard(&policy),
            tool_env: ToolCtx {
                workspace_dir: std::path::PathBuf::from("/tmp"),
                agent_db: pool,
                policy: Arc::clone(&policy),
                embedder: None,
            },
            max_iterations: 1,
            messages_mirror: false,
            model: Arc::new(String::new()),
            max_tokens: DEFAULT_MAX_TOKENS,
            billing_user_id: None,
            stop: None,
            segments: None,
        };
        // A gateway that answers a tool call first, then the summing up.
        let gateway = tokio::spawn({
            let pending = Arc::clone(&pending);
            async move {
                let mut asked = Vec::new();
                for content in [
                    serde_json::json!([{"type": "tool_use", "id": "t1", "name": "no_such_tool", "input": {}}]),
                    serde_json::json!([{"type": "text", "text": "I looked once; the rest is left."}]),
                ] {
                    let Some(AgentToGateway::LlmRequest(req)) = rx.recv().await else {
                        panic!("no request");
                    };
                    asked.push(req.messages.clone());
                    let stop = if content[0]["type"] == "tool_use" {
                        "tool_use"
                    } else {
                        "end_turn"
                    };
                    let reply = LlmResponse {
                        request_id: req.request_id.clone(),
                        outcome: LlmOutcome::Ok,
                        provider_response: serde_json::json!({"content": content, "stop_reason": stop}),
                        usage: None,
                        trimmed: false,
                        fallback: None,
                    };
                    loop {
                        if let Some(answer) = pending.lock().await.remove(&req.request_id) {
                            let _ = answer.send(reply);
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                }
                asked
            }
        });
        let mut messages = vec![serde_json::json!({"role": "user", "content": "do it"})];
        let prompt = crate::system_prompt::BootstrapFiles::default().system_prompt(None);
        let ended = run_typed(
            &handle,
            crate::tools::context::WEBCHAT,
            &mut messages,
            &prompt,
        )
        .await;
        let asked = gateway.await.expect("gateway");
        match ended {
            Err(RunError::IterationCap { cap, partial_text }) => {
                assert_eq!(cap, 1);
                assert_eq!(partial_text, "I looked once; the rest is left.");
            }
            other => panic!("unexpected {other:?}"),
        }
        // The model was told the rounds were spent, after the results.
        let last = asked[1]
            .as_array()
            .and_then(|m| m.last())
            .cloned()
            .unwrap_or_default();
        let told = last["content"]
            .as_array()
            .and_then(|b| b.last())
            .cloned()
            .unwrap_or_default();
        assert!(
            told["text"]
                .as_str()
                .unwrap_or_default()
                .contains("tool rounds allowed here")
        );
        assert_eq!(
            messages.last().map(|m| m["role"].clone()),
            Some(serde_json::json!("assistant"))
        );
        // The instruction is not kept in the chat.
        let results = &messages[messages.len() - 2];
        assert!(
            results["content"]
                .as_array()
                .is_some_and(|b| b.iter().all(|b| b["type"] == "tool_result"))
        );
    }

    #[tokio::test]
    async fn an_unavailable_model_gives_way_to_its_fallback_for_the_turn() {
        use crate::tools::{ToolCtx, ToolRegistry};
        let (tx, mut rx) = mpsc::channel(8);
        let (pending, _) = maps();
        let policy = Arc::new(havn_core::Policy::default());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite");
        let handle = LoopHandle {
            writer_tx: tx,
            pending: Arc::clone(&pending),
            registry: ToolRegistry::standard(&policy),
            tool_env: ToolCtx {
                workspace_dir: std::path::PathBuf::from("/tmp"),
                agent_db: pool,
                policy: Arc::clone(&policy),
                embedder: None,
            },
            max_iterations: 3,
            messages_mirror: false,
            model: Arc::new("p:main".into()),
            max_tokens: DEFAULT_MAX_TOKENS,
            billing_user_id: None,
            stop: None,
            segments: None,
        };
        let gateway = tokio::spawn({
            let pending = Arc::clone(&pending);
            async move {
                let mut models = Vec::new();
                for n in 0..3 {
                    let Some(AgentToGateway::LlmRequest(req)) = rx.recv().await else {
                        panic!("no request");
                    };
                    models.push(req.model.clone());
                    let reply = match n {
                        0 => LlmResponse {
                            request_id: req.request_id.clone(),
                            outcome: LlmOutcome::Error {
                                message: "unavailable".into(),
                            },
                            provider_response: Value::Null,
                            usage: None,
                            trimmed: false,
                            fallback: Some("p:spare".into()),
                        },
                        1 => LlmResponse {
                            request_id: req.request_id.clone(),
                            outcome: LlmOutcome::Ok,
                            provider_response: serde_json::json!({"stop_reason": "tool_use", "content": [
                                {"type": "tool_use", "id": "t1", "name": "no_such_tool", "input": {}}
                            ]}),
                            usage: None,
                            trimmed: false,
                            fallback: None,
                        },
                        _ => LlmResponse {
                            request_id: req.request_id.clone(),
                            outcome: LlmOutcome::Ok,
                            provider_response: serde_json::json!({"stop_reason": "end_turn", "content": [
                                {"type": "text", "text": "done"}
                            ]}),
                            usage: None,
                            trimmed: false,
                            fallback: None,
                        },
                    };
                    loop {
                        if let Some(answer) = pending.lock().await.remove(&req.request_id) {
                            let _ = answer.send(reply);
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                }
                models
            }
        });
        let trace = Arc::new(std::sync::Mutex::new(Trace::default()));
        let mut messages = vec![serde_json::json!({"role": "user", "content": "go"})];
        let prompt = crate::system_prompt::BootstrapFiles::default().system_prompt(None);
        let text = TRACE
            .scope(
                Arc::clone(&trace),
                run_typed(
                    &handle,
                    crate::tools::context::WEBCHAT,
                    &mut messages,
                    &prompt,
                ),
            )
            .await
            .expect("answered");
        assert_eq!(text, "done");
        // The fallback answered, and the turn's next call went to it.
        let models = gateway.await.expect("gateway");
        assert_eq!(models, ["p:main", "p:spare", "p:spare"]);
        assert_eq!(trace.lock().unwrap().fallback.as_deref(), Some("p:spare"));
    }

    #[tokio::test]
    async fn cancelled_llm_call_leaves_no_pending_entry() {
        let (tx, _rx) = mpsc::channel(8);
        let (pending, _) = maps();
        let call = tokio::spawn({
            let pending = Arc::clone(&pending);
            async move { request_llm(&tx, &pending, request("r1")).await }
        });
        while pending.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }

        call.abort();
        let _ = call.await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn replacements_that_are_still_too_large_are_dropped() {
        let (pending, queries) = maps();
        let big = "x".repeat(MAX_FRAME_BYTES);
        let mut wire = Vec::new();

        // The error reply echoes the id, so it is as large as the original.
        let unsendable = AgentToGateway::AgentQueryResponse(AgentQueryResponse {
            request_id: big.clone(),
            outcome: AgentQueryOutcome::Ok,
            text: String::new(),
            transcript: Value::Null,
        });
        tokio::time::timeout(
            Duration::from_secs(10),
            write_outbound(&mut wire, unsendable, &pending, &queries),
        )
        .await
        .expect("the writer does not spin")
        .expect("write");
        assert!(wire.is_empty());

        // Dropping the transcript is not enough; the error reply is.
        let answer = AgentToGateway::AgentQueryResponse(AgentQueryResponse {
            request_id: "q".into(),
            outcome: AgentQueryOutcome::Ok,
            text: big.clone(),
            transcript: serde_json::json!([big]),
        });
        write_outbound(&mut wire, answer, &pending, &queries)
            .await
            .expect("write");
        let mut reader = tokio::io::BufReader::new(wire.as_slice());
        match havn_proto::read_frame::<_, AgentToGateway>(&mut reader).await {
            Ok(Some(AgentToGateway::AgentQueryResponse(r))) => {
                assert_eq!(r.request_id, "q");
                assert!(
                    matches!(r.outcome, AgentQueryOutcome::Error { .. }),
                    "{:?}",
                    r.outcome
                );
            }
            other => panic!("unexpected {other:?}"),
        }
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
