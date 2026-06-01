//! Wire protocol for the gateway↔agent runtime socket.
//!
//! Frames are JSON, newline-terminated, over a Unix socket (single-host)
//! or a mutually-authenticated TCP stream (multi-node). Both directions
//! use the same encoding; only the message type set differs.
//!
//! See spec §8.2 (LLM proxy) and §9.1 (core loop).

pub mod channel;
pub mod codec;

use havn_core::{InboundMessage, OutboundMessage, Policy};
use serde::{Deserialize, Serialize};

pub use codec::{FrameError, read_frame, write_frame};

/// Current wire-protocol version. Mismatch on `Hello` is a hard error.
pub const PROTOCOL_VERSION: &str = "0.1.0";

/// Frames sent from the agent runtime to the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentToGateway {
    /// Sent immediately after socket connection. Identifies the agent and
    /// asserts a protocol version. Gateway closes the socket on mismatch.
    Hello { agent_id: String, version: String },
    /// Outbound message to be routed via the appropriate channel adapter.
    OutboundMessage(OutboundMessage),
    /// Request an LLM completion via the gateway proxy.
    LlmRequest(LlmRequest),
    /// Cron job finished — gateway records a `CronRun` row, writes the output
    /// to disk, and optionally broadcasts via `deliver` (spec §8.5).
    CronResult(CronResult),
    /// Periodic liveness ping. Gateway may consider the agent dead if
    /// these stop arriving for a configurable timeout.
    LivenessPing,
    /// Reply to a [`GatewayToAgent::MemoryForgetRequest`]. Carries the
    /// matching `request_id` so the gateway can resolve the pending
    /// future. Spec Phase-2 §13: lets the dashboard's read-only memory
    /// page get a delete button without violating the "agent is the
    /// only writer to agent.db" invariant (spec §5.2).
    MemoryForgetResponse(MemoryForgetResponse),
    /// Reply to a [`GatewayToAgent::SkillPinRequest`]. Same correlation
    /// shape as `MemoryForgetResponse`.
    SkillPinResponse(SkillPinResponse),
    /// Caller-side initiation of a cross-agent query (spec §4.4 v0.7).
    /// The gateway routes this to `target_agent_id`, which must be owned
    /// by the same user. Replies arrive as
    /// [`GatewayToAgent::AgentQueryResult`] with the matching
    /// `request_id`.
    AgentQuery(AgentQuery),
    /// Callee-side reply to a [`GatewayToAgent::IncomingQuery`]. The
    /// gateway pairs this with the in-flight query by `request_id` and
    /// forwards a [`GatewayToAgent::AgentQueryResult`] back to the
    /// caller.
    AgentQueryResponse(AgentQueryResponse),
}

/// Frames sent from the gateway to the agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
#[allow(clippy::large_enum_variant)]
pub enum GatewayToAgent {
    /// Hello accepted. Sent in response to a successful `Hello`.
    ///
    /// `policy` carries the runtime-enforcement policy (spec §6) the gateway
    /// resolved for this agent (per-agent override, then team role, then
    /// system default). The runtime builds its tool registry from this snapshot
    /// instead of `Policy::default()` — that's what turns "policy is data" from
    /// a slogan into actual enforcement.
    ///
    /// Defaulted on deserialize so an older gateway that doesn't yet send
    /// the field still parses (the runtime falls back to `Policy::default()`
    /// with a warning in that case).
    Welcome {
        session_token: String,
        #[serde(default)]
        policy: Policy,
        /// Model the runtime should use for this session's LLM calls.
        /// Resolved gateway-side from `agent.config.model`; `None`
        /// when the agent's config doesn't pin one (runtime falls back
        /// to its compiled-in default). Defaulted on deserialize so an
        /// older gateway can interop.
        #[serde(default)]
        model: Option<String>,
        /// Embedding-provider configuration shipped from the gateway
        /// (spec §9.4 v0.7 hybrid retrieval). The runtime instantiates
        /// the right `EmbeddingProvider` impl from this and uses it
        /// for `memory_remember` writes + `memory_search` queries.
        /// Defaults to `serde_json::Value::Null` so older gateways
        /// (pre-v0.7) still parse — runtime treats absent or
        /// `provider: "disabled"` identically: no embeddings written,
        /// FTS5-only search.
        #[serde(default)]
        embedding: serde_json::Value,
        /// Operator-declared extra bind-mounts (spec §4.1 v0.7) the
        /// runtime should add to its Landlock allowlist. Populated from
        /// the gateway's `[[extra_mounts]]` config; the spawner has
        /// already done the actual `mount --bind` before the runtime
        /// runs, so this field is purely for LSM accounting. Empty
        /// when no packs are wired up — the runtime treats absent and
        /// empty identically. Defaulted on deserialize so older
        /// gateways still interop.
        #[serde(default)]
        extra_mounts: Vec<ExtraMountWire>,
        /// Operator-declared in-namespace tmpfs mounts (spec §16.2).
        /// Same purpose as `extra_mounts` from the runtime's view: the
        /// actual `mount tmpfs` already happened in havn-init; this
        /// field is for the runtime's Landlock rw-allowlist. Defaulted
        /// for older-gateway interop.
        #[serde(default)]
        tmpfs_mounts: Vec<TmpfsMountWire>,
        /// Operator-declared seccomp allowances (spec §16.2). The
        /// runtime validates names against `libc::SYS_*` at startup
        /// and removes matching entries from the syscall blocklist.
        /// Defaulted for older-gateway interop.
        #[serde(default)]
        seccomp_allow_extra: Vec<String>,
    },
    /// Inbound message from a channel adapter.
    InboundMessage(InboundMessage),
    /// LLM call has completed (success or error). Phase 1 is non-streaming;
    /// streaming chunks land in a follow-up vertical (separate frame variant).
    LlmResponse(LlmResponse),
    /// Heartbeat-tick injection — fired per agent's HEARTBEAT.md schedule (§9.6).
    /// Runs in the live session context.
    HeartbeatTick,
    /// Cron-job tick — runs in a fresh context (`skip_memory: true`,
    /// `cron_system_prompt`, no history). Spec §8.5.
    CronTick(CronTick),
    /// Dashboard-initiated memory delete. The runtime calls
    /// `memory::forget(key)` on its agent.db, replies with a
    /// [`AgentToGateway::MemoryForgetResponse`] carrying the same
    /// `request_id`. The gateway awaits the response so the HTTP
    /// caller gets a synchronous outcome.
    ///
    /// Why a frame instead of having the gateway write directly?
    /// Spec §5.2: agent.db has a single writer (the agent runtime).
    /// Gateway-side writes would race with the runtime's typed-memory
    /// invariants (supersedes chain, key-suffixing). Routing through
    /// the runtime preserves the invariant and lets the runtime echo
    /// the same audit trail (`@forgotten:<ts>` suffix).
    MemoryForgetRequest(MemoryForgetRequest),
    /// Dashboard-initiated skill pin / unpin. Same rationale as
    /// `MemoryForgetRequest` — pins live in `skills_index` which the
    /// runtime owns.
    SkillPinRequest(SkillPinRequest),
    /// Graceful shutdown request — agent should close cleanly within 5s,
    /// after which the spawner sends SIGKILL.
    Shutdown,
    /// Inbound cross-agent query addressed to this agent (spec §4.4
    /// v0.7). Caller is identified by `caller_agent_id`. Token costs of
    /// any LLM calls during processing must be billed against
    /// `billing_user_id` (the caller's owner) — propagate it into every
    /// [`LlmRequest::billing_user_id`]. Reply with
    /// [`AgentToGateway::AgentQueryResponse`] carrying the same
    /// `request_id`.
    IncomingQuery(IncomingQuery),
    /// Reply for an [`AgentToGateway::AgentQuery`] this agent issued.
    /// Routed back from the target agent (or synthesised by the gateway
    /// on rejection / timeout / not-running).
    AgentQueryResult(AgentQueryResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryForgetRequest {
    pub request_id: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryForgetResponse {
    pub request_id: String,
    pub outcome: AdminOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPinRequest {
    pub request_id: String,
    pub name: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPinResponse {
    pub request_id: String,
    pub outcome: AdminOutcome,
}

/// Caller-side: "ask `target_agent_id` this question, return the answer".
/// Both agents must share the same owner; the gateway rejects mismatches
/// before routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentQuery {
    /// Correlation ID. The runtime matches the
    /// [`GatewayToAgent::AgentQueryResult`] back via this.
    pub request_id: String,
    /// Agent to ask. UUID-as-string per the rest of the proto.
    pub target_agent_id: String,
    /// User turn injected into the target agent's temporary context.
    pub prompt: String,
    /// Wall-clock budget for the target's reply, in seconds. Gateway
    /// clamps to a hard-coded ceiling (300s) and cancels the in-flight
    /// IncomingQuery on expiry, returning an `Error` outcome to the
    /// caller. Default 60.
    #[serde(default = "default_query_timeout")]
    pub timeout_seconds: u32,
    /// When true, [`AgentQueryResult::transcript`] is populated with the
    /// target's full content blocks (text + tool_use + tool_result).
    /// Default false: only the final assistant text is returned.
    #[serde(default)]
    pub include_transcript: bool,
}

fn default_query_timeout() -> u32 {
    60
}

/// Gateway → callee. Identifies the caller and carries the billing
/// principal. The runtime spins up a temporary, non-persistent tool loop
/// with `prompt` as the only user turn and the agent's frozen system
/// prompt + typed memory in scope. **Crucial**: every
/// [`LlmRequest`] the runtime emits during this loop MUST carry
/// `billing_user_id` set to [`Self::billing_user_id`] so token spend
/// hits the caller's credentials, not the callee's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingQuery {
    /// Correlation ID — round-tripped back in
    /// [`AgentToGateway::AgentQueryResponse`].
    pub request_id: String,
    /// Caller agent's id, for audit logging and same-owner verification
    /// echoes. The gateway has already validated owner equality before
    /// dispatching this frame.
    pub caller_agent_id: String,
    /// User who must pay for the LLM token spend during this query.
    /// Identical to the caller's owner in v1 (same-owner only); kept
    /// as a separate field so cross-owner extensions don't reshape the
    /// proto.
    pub billing_user_id: String,
    /// User turn — what the caller asked.
    pub prompt: String,
    /// True when the caller asked for the full transcript. The runtime
    /// captures every assistant content block during the temp loop and
    /// returns them in
    /// [`AgentQueryResponse::transcript`].
    #[serde(default)]
    pub include_transcript: bool,
}

/// Callee → gateway. Carries the assistant's final text plus, when
/// requested, the full transcript of content blocks (text + tool_use +
/// tool_result) that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentQueryResponse {
    pub request_id: String,
    pub outcome: AgentQueryOutcome,
    /// Final assistant text. Empty string when `outcome` is `Error`.
    #[serde(default)]
    pub text: String,
    /// Full content-block transcript when the caller passed
    /// `include_transcript: true`; otherwise empty. Same shape as
    /// Anthropic content blocks (`{type, text}`, `{type:"tool_use", ...}`,
    /// `{type:"tool_result", ...}`) so the caller's tool_loop can fold
    /// them into its own context if it cares to.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub transcript: serde_json::Value,
}

/// Gateway → caller. Same payload shape as
/// [`AgentQueryResponse`], rebadged so the agent's frame parser keeps
/// caller and callee paths distinct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentQueryResult {
    pub request_id: String,
    pub outcome: AgentQueryOutcome,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub transcript: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentQueryOutcome {
    /// Target replied successfully.
    Ok,
    /// Target rejected (different owner, target offline, recursion cap)
    /// or the gateway aborted before reaching the target. `message` is
    /// short, operator-readable, surfaced verbatim to the LLM as the
    /// tool_result error body.
    Error { message: String },
    /// `timeout_seconds` elapsed before the target replied. Distinct
    /// from `Error` so the LLM can decide to retry vs. give up.
    Timeout,
}

/// Common shape for dashboard-initiated admin RPCs. Mirrors `LlmOutcome`
/// in spirit — the runtime always replies, but the reply may be a
/// soft "no row matched" not an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdminOutcome {
    /// Operation succeeded — `details` carries any small payload (e.g.
    /// the new pinned state for confirmation).
    Ok { details: serde_json::Value },
    /// The target row didn't exist (e.g. forgetting a key that was
    /// never set, pinning a skill that was uninstalled). The dashboard
    /// surfaces this as "already done — refreshing list".
    NotFound,
    /// Runtime hit an error executing the operation. Caller surfaces
    /// `message` to the user.
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Correlation ID; the agent matches the [`LlmResponse`] back via this.
    pub request_id: String,
    pub model: String,
    /// Provider-shaped messages array, passed through to the LLM provider.
    pub messages: serde_json::Value,
    /// Provider-specific options (`temperature`, `top_p`, …) passed through.
    #[serde(default)]
    pub options: serde_json::Value,
    /// Billing override (spec §4.4 v0.7 cross-agent query). When set, the
    /// gateway resolves credentials and records usage against this user
    /// instead of the executing agent's owner. Used during
    /// [`GatewayToAgent::IncomingQuery`] processing so the *caller* pays
    /// for token spend even though the *callee* is doing the work. None
    /// for ordinary LLM calls — gateway falls back to agent owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_user_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub request_id: String,
    pub outcome: LlmOutcome,
    /// Raw response from the LLM provider. Phase 1 ships only Anthropic, so
    /// this is the JSON of the Anthropic Messages API response. When other
    /// providers land, the shape stays provider-specific — agents already
    /// route on `model` so they know how to interpret it.
    #[serde(default)]
    pub provider_response: serde_json::Value,
    pub usage: Option<LlmUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmOutcome {
    Ok,
    Error { message: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    // (no `estimated_usd` — havn doesn't maintain a model pricing table.
    //  See spec §7.3 v0.6.)
}

/// Wire form of an operator-declared extra bind-mount (spec §4.1 v0.7).
/// Mirrors `havn_spawner::ExtraMount` but lives here so the proto crate
/// doesn't take a dep on havn-spawner (and its libc / linux-only
/// modules). The runtime translates these into Landlock RW/RO entries
/// — the actual `mount --bind` is the spawner's job, already done
/// before the runtime starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraMountWire {
    pub path: String,
    /// `"ro"` or `"rw"`, matching `havn_spawner::MountMode`'s wire form.
    /// Defaulted to `"ro"` on deserialize so older gateways that just
    /// emit `{path: "..."}` still parse.
    #[serde(default = "default_extra_mount_mode")]
    pub mode: String,
}

fn default_extra_mount_mode() -> String {
    "ro".into()
}

/// Wire form of an operator-declared in-namespace tmpfs mount
/// (spec §16.2). Mirrors `havn_spawner::TmpfsMount` in proto-friendly
/// types. The runtime translates these into Landlock rw entries; the
/// `mount tmpfs` itself happened in havn-init before the runtime ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmpfsMountWire {
    pub path: String,
    #[serde(default = "default_tmpfs_size_mb")]
    pub size_mb: u64,
}

fn default_tmpfs_size_mb() -> u64 {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTick {
    pub job_id: String,
    /// Free-form prompt to inject as the first (and only) user turn.
    pub prompt: String,
    /// Subset of tool names to expose for this run (empty = the agent's
    /// default policy-allowed set; spec §6.2 `context_toolsets.cron`).
    #[serde(default)]
    pub enabled_toolsets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronResult {
    pub job_id: String,
    pub outcome: CronOutcome,
    /// Final assistant text. May start with the literal sentinel `[SILENT]`
    /// to suppress delivery while still recording the run for audit.
    pub output: String,
    /// Convenience flag — `true` when `output.trim_start().starts_with("[SILENT]")`.
    /// Computed by the runtime so the gateway doesn't have to re-derive it.
    pub silent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronOutcome {
    Success,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn agent_hello_round_trips_through_json() {
        let frame = AgentToGateway::Hello {
            agent_id: "agt-test".into(),
            version: PROTOCOL_VERSION.into(),
        };
        let line = serde_json::to_string(&frame).expect("serialize");
        let parsed: AgentToGateway = serde_json::from_str(&line).expect("parse");
        match parsed {
            AgentToGateway::Hello { agent_id, version } => {
                assert_eq!(agent_id, "agt-test");
                assert_eq!(version, PROTOCOL_VERSION);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn gateway_welcome_serializes_with_snake_case_tag() {
        let frame = GatewayToAgent::Welcome {
            session_token: "tok".into(),
            policy: havn_core::Policy::default(),
            model: None,
            embedding: serde_json::Value::Null,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
        };
        let line = serde_json::to_string(&frame).expect("serialize");
        assert!(line.contains("\"type\":\"welcome\""));
    }

    #[test]
    fn welcome_round_trips_with_custom_policy() {
        // The runtime uses the policy snapshot the gateway sends to build its
        // tool registry; if this round-trip ever silently loses a field, the
        // agent will run with the wrong privileges. Lock it.
        let mut policy = havn_core::Policy::default();
        policy.permissions.can_use_shell = false;
        policy.permissions.can_access_network = false;
        policy.context_toolsets.0.insert(
            "cron".into(),
            havn_core::ContextToolsetEntry {
                disabled: vec!["shell".into(), "web_fetch".into()],
            },
        );

        let frame = GatewayToAgent::Welcome {
            session_token: "tok".into(),
            policy: policy.clone(),
            model: Some("claude-sonnet-4-6".into()),
            embedding: serde_json::Value::Null,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
        };
        let line = serde_json::to_string(&frame).expect("serialize");
        let parsed: GatewayToAgent = serde_json::from_str(&line).expect("parse");
        let GatewayToAgent::Welcome {
            policy: parsed_policy,
            ..
        } = parsed
        else {
            panic!("wrong variant");
        };
        assert!(!parsed_policy.permissions.can_use_shell);
        assert!(!parsed_policy.permissions.can_access_network);
        assert_eq!(
            parsed_policy
                .context_toolsets
                .0
                .get("cron")
                .map(|e| e.disabled.len()),
            Some(2)
        );
    }

    #[test]
    fn welcome_without_policy_field_defaults() {
        // Back-compat: an older gateway that doesn't yet emit `policy` must
        // still produce a parseable Welcome — the runtime then falls back to
        // Policy::default() with a warn log.
        let line = r#"{"type":"welcome","session_token":"tok"}"#;
        let parsed: GatewayToAgent = serde_json::from_str(line).expect("parse");
        let GatewayToAgent::Welcome { policy, .. } = parsed else {
            panic!("wrong variant");
        };
        // Default policy mirrors havn-core's defaults.
        assert!(policy.permissions.can_use_shell);
    }
}
