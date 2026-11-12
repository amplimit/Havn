//! LLM proxy — gateway-side caller for upstream providers (spec §8.2 + §7.4 + §13 Phase 3).
//!
//! Phase 1 shipped Anthropic-only. Phase 3 adds OpenAI, OpenRouter, and
//! Gemini behind a [`provider::LlmProvider`] trait. The fallback policy
//! (401 → disable + try next, 429/402 → try next, 5xx → try next, all
//! exhausted → error) is defined once here and reused by every provider.
//!
//! ## Canonical shape
//!
//! The gateway's wire contract with the runtime is **Anthropic Messages**
//! (the runtime's `tool_loop.rs` parses `stop_reason == "tool_use"` and
//! `content[].type` directly). Non-Anthropic providers translate in/out
//! inside their `complete` impl so the runtime stays untouched. See
//! [`openai`] and [`gemini`] for the translators.
//!
//! ## Provider selection
//!
//! [`complete`] picks a provider via:
//! 1. Explicit hint in `LlmRequest.options.provider` (string, set by the
//!    runtime when an agent's policy pins a provider regardless of model).
//! 2. Otherwise model-name inference via [`provider::provider_for_model`].
//!
//! The chosen provider name keys both:
//! - The credential resolver (`Credential.provider` is matched as an opaque
//!   string — the resolver doesn't care which provider).
//! - The provider trait dispatch in [`dispatch_provider`].

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod provider;

use std::borrow::Cow;

use havn_core::{AgentId, BudgetExhaustAction, BudgetPolicy, UserId};
use havn_db::repo::credential_usage::{self, Caps, Reservation};
use havn_db::repo::credentials::Credential;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::{info, warn};

use crate::credential_resolver::{
    credential_caps, minute_ago_utc_rfc3339, resolve_for_user, today_start_utc_rfc3339,
};
use crate::keyring::KeyRing;
use provider::{LlmProvider, classify};

/// Smallest output allowance worth sending upstream when a token cap
/// leaves less than the request's `max_tokens`. Below it the credential
/// counts as exhausted.
const MIN_OUTPUT_TOKENS: u32 = 256;

/// Bytes of request text counted as one input token when reserving. Real
/// tokenizers average about 4 bytes per token for English and 2 to 3 for
/// CJK text, so this over-reserves; the reservation is settled to the
/// provider's reported usage once the call returns.
const INPUT_BYTES_PER_TOKEN: u64 = 2;

/// Tokens reserved per base64 image, in place of its encoded size. Claude
/// bills at most about 1,600 tokens for an image at standard resolution
/// and about 4,800 at high resolution.
const IMAGE_TOKENS: u64 = 5_000;

/// Who an LLM call is billed to and the agent budget it must fit in.
#[derive(Debug, Clone)]
pub struct Billing {
    pub user_id: UserId,
    /// `None` for gateway-direct calls (the test endpoint).
    pub agent_id: Option<AgentId>,
    /// The agent's policy budget; ignored without `agent_id`.
    pub budget: BudgetPolicy,
}

impl Billing {
    #[must_use]
    pub fn user(user_id: UserId) -> Self {
        Self {
            user_id,
            agent_id: None,
            budget: BudgetPolicy::default(),
        }
    }

    /// The agent's daily token cap as enforced by reservations: 0 (none)
    /// without an agent, for an unlimited budget, or for
    /// `on_exhaust = warn_only`.
    fn enforced_agent_tokens(&self) -> i64 {
        if self.agent_id.is_none() || self.budget.on_exhaust == BudgetExhaustAction::WarnOnly {
            return 0;
        }
        i64::try_from(self.budget.max_tokens_per_day).unwrap_or(i64::MAX)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    /// `String` for plain text or a vec of content blocks. Passed through verbatim.
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Tool definitions in Anthropic's `tools` shape: an array of
    /// `{ name, description, input_schema }` objects. Passed verbatim
    /// (or translated to OpenAI/Gemini shape inside the relevant provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicResponse {
    pub id: String,
    pub model: String,
    pub role: String,
    /// Raw content blocks. Kept as `Value` so Anthropic-shape evolution
    /// (tool_use's `id`/`name`/`input`, image source structures,
    /// citations, thinking blocks, …) round-trips losslessly through
    /// the proxy without us having to track every new content type.
    /// The runtime parses these blocks itself; the proxy only handles
    /// the envelope.
    ///
    /// Bug we hit and fixed: the previous typed `ContentBlock` only
    /// kept `type` + `text`, silently dropping the `id`/`name`/`input`
    /// of `tool_use` blocks. The next round-trip put a `tool_use`
    /// content with no `id` back into messages, and Anthropic 400'd
    /// with `messages.N.content.0.tool_use.id: Field required`.
    pub content: serde_json::Value,
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub usage: AnthropicUsage,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProxyError {
    #[error("no usable credential for provider {0:?}")]
    NoCredential(String),
    #[error("upstream provider error: {status} — {message}")]
    Upstream { status: u16, message: String },
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("database: {0}")]
    Db(#[from] havn_db::DbError),
    #[error("invalid response body: {0}")]
    InvalidResponse(String),
    #[error("unknown provider: {0:?}")]
    UnknownProvider(String),
    #[error("token budget exhausted: {0}")]
    BudgetExhausted(String),
}

/// Outcome of a single attempt against one credential. Drives the fallback loop.
pub(crate) enum Attempt {
    Ok(AnthropicResponse),
    /// Try next credential; do not disable this one.
    TryNext {
        reason: String,
    },
    /// Try next credential, and disable this one (401 unauthorized — key revoked).
    Disable {
        reason: String,
    },
    /// Hard upstream error — surface to caller, do not try other credentials.
    Fatal(ProxyError),
}

/// Run an LLM completion through the gateway proxy with credential fallback.
/// Provider is resolved from `request.model` (or callers can pre-resolve it
/// via [`complete_with_provider`]).
///
/// Iterates the user's credential chain (highest priority first); on each:
/// - token caps leave no room for the request → try next.
/// - 200 → settle usage, return canonical response.
/// - 401 → disable credential, try next.
/// - 402 / 429 → try next; do not disable (transient / quota).
/// - 5xx / network → try next.
/// - 4xx (other) → fatal — caller's request is malformed; do not iterate.
#[allow(
    dead_code,
    reason = "public API; callers without a provider hint use this entry"
)]
pub async fn complete(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    billing: &Billing,
    request: &AnthropicRequest,
    max_tokens_ceiling: Option<u32>,
) -> Result<AnthropicResponse, ProxyError> {
    let provider_name = provider::provider_for_model(&request.model);
    complete_with_provider(
        db,
        http,
        keyring,
        billing,
        provider_name,
        request,
        max_tokens_ceiling,
    )
    .await
}

/// Like [`complete`], but the caller has already decided which provider to
/// use (e.g. the runtime read it out of `LlmRequest.options.provider`).
/// `max_tokens` is first clamped to the model's known output limit and to
/// the operator's `[llm] max_tokens_ceiling`.
pub async fn complete_with_provider(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    billing: &Billing,
    provider_name: &str,
    request: &AnthropicRequest,
    max_tokens_ceiling: Option<u32>,
) -> Result<AnthropicResponse, ProxyError> {
    let provider = dispatch_provider(provider_name)?;
    let max_tokens =
        provider::clamp_max_tokens(&request.model, request.max_tokens, max_tokens_ceiling);
    let request = with_max_tokens(request, max_tokens);
    complete_through(
        db,
        http,
        keyring,
        provider,
        provider_name,
        billing,
        &request,
    )
    .await
}

fn with_max_tokens(request: &AnthropicRequest, max_tokens: u32) -> Cow<'_, AnthropicRequest> {
    if max_tokens == request.max_tokens {
        Cow::Borrowed(request)
    } else {
        Cow::Owned(AnthropicRequest {
            max_tokens,
            ..request.clone()
        })
    }
}

async fn complete_through(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    provider: &dyn LlmProvider,
    provider_name: &str,
    billing: &Billing,
    request: &AnthropicRequest,
) -> Result<AnthropicResponse, ProxyError> {
    let chain = resolve_for_user(db, billing.user_id, provider_name).await?;
    if chain.is_empty() {
        return Err(ProxyError::NoCredential(provider_name.to_string()));
    }

    // Each attempt first reserves the request's worst case (estimated
    // input plus `max_tokens`) against the credential's caps and the
    // agent's budget in one statement, so parallel requests cannot all
    // pass a check that only one of them fits in. The reservation is
    // settled to the reported usage on success and released otherwise.
    let tokens_in = input_token_estimate(request);
    let agent_tokens = billing.enforced_agent_tokens();
    let min_tokens_out = i64::from(request.max_tokens.min(MIN_OUTPUT_TOKENS));
    let mut exhausted: Option<String> = None;
    for cred in chain {
        let caps = Caps {
            agent_tokens,
            ..credential_caps(&cred)
        };
        let day_start = today_start_utc_rfc3339();
        let minute_start = minute_ago_utc_rfc3339();
        let reservation = Reservation {
            credential_id: cred.id,
            user_id: billing.user_id,
            agent_id: billing.agent_id,
            provider: provider_name,
            model: &request.model,
            tokens_in,
            max_tokens_out: i64::from(request.max_tokens),
            min_tokens_out,
            caps,
            day_start: &day_start,
            minute_start: &minute_start,
        };
        let reserved = match credential_usage::reserve(db, reservation).await {
            Ok(Some(reserved)) => reserved,
            Ok(None) => {
                let refusal = refusal(
                    db,
                    &cred,
                    billing,
                    caps,
                    tokens_in + min_tokens_out,
                    (&day_start, &minute_start),
                )
                .await;
                warn!(
                    credential_id = %cred.id,
                    user_id = %billing.user_id,
                    reason = %refusal.reason,
                    "token caps leave no room for this request; skipping credential"
                );
                if refusal.agent {
                    // The agent budget spans every credential.
                    return Err(ProxyError::BudgetExhausted(refusal.reason));
                }
                exhausted = Some(refusal.reason);
                continue;
            }
            Err(e) => {
                warn!(
                    credential_id = %cred.id,
                    error = %e,
                    "token reservation failed; skipping credential"
                );
                continue;
            }
        };
        let granted = u32::try_from(reserved.tokens_out).unwrap_or(request.max_tokens);
        let attempt_request = with_max_tokens(request, granted.min(request.max_tokens));

        match try_one(http, keyring, provider, &cred, &attempt_request).await {
            Attempt::Ok(resp) => {
                let tokens_in = i64::try_from(resp.usage.input_tokens).unwrap_or(i64::MAX);
                let tokens_out = i64::try_from(resp.usage.output_tokens).unwrap_or(i64::MAX);
                // A failed write leaves the reservation in place, which
                // over-counts rather than under-counts.
                let _ =
                    credential_usage::settle(db, &reserved.id, &resp.model, tokens_in, tokens_out)
                        .await
                        .map_err(|e| warn!(error = %e, "usage settlement failed (non-fatal)"));
                info!(
                    credential_id = %cred.id,
                    provider = provider_name,
                    model = %resp.model,
                    in = resp.usage.input_tokens,
                    out = resp.usage.output_tokens,
                    "llm proxy ok"
                );
                warn_over_agent_budget(db, &cred, billing, (&day_start, &minute_start)).await;
                return Ok(resp);
            }
            Attempt::Disable { reason } => {
                release(db, &reserved.id).await;
                warn!(credential_id = %cred.id, %reason, "disabling credential");
                let _ = havn_db::repo::credentials::set_enabled(db, cred.id, false)
                    .await
                    .map_err(|e| warn!(error = %e, "failed to disable credential"));
            }
            Attempt::TryNext { reason } => {
                release(db, &reserved.id).await;
                warn!(credential_id = %cred.id, %reason, "skipping credential, trying next");
            }
            Attempt::Fatal(e) => {
                release(db, &reserved.id).await;
                return Err(e);
            }
        }
    }

    Err(exhausted.map_or_else(
        || ProxyError::NoCredential(provider_name.to_string()),
        ProxyError::BudgetExhausted,
    ))
}

async fn release(db: &SqlitePool, reservation_id: &str) {
    if let Err(e) = credential_usage::release(db, reservation_id).await {
        warn!(error = %e, "releasing token reservation failed");
    }
}

struct Refusal {
    reason: String,
    /// The agent's budget, not the credential, refused the request.
    agent: bool,
}

/// Which cap refused a reservation needing `needed` tokens, from a fresh
/// read of the totals (not atomic with the reservation; for messages only).
async fn refusal(
    db: &SqlitePool,
    cred: &Credential,
    billing: &Billing,
    caps: Caps,
    needed: i64,
    (day_start, minute_start): (&str, &str),
) -> Refusal {
    let totals = credential_usage::totals(
        db,
        cred.id,
        billing.user_id,
        billing.agent_id,
        day_start,
        minute_start,
    )
    .await
    .unwrap_or_default();
    let refuses = |cap: i64, used: i64| cap > 0 && used.saturating_add(needed) > cap;
    let (reason, agent) = if refuses(caps.agent_tokens, totals.agent_tokens) {
        (
            format!(
                "agent daily token budget of {} has {} used today",
                caps.agent_tokens, totals.agent_tokens
            ),
            true,
        )
    } else if caps.user_requests_per_minute > 0
        && totals.user_requests >= caps.user_requests_per_minute
    {
        (
            format!(
                "per-user limit of {} requests per minute reached",
                caps.user_requests_per_minute
            ),
            false,
        )
    } else if refuses(caps.user_tokens, totals.user_tokens) {
        (
            format!(
                "per-user daily token cap of {} has {} used today",
                caps.user_tokens, totals.user_tokens
            ),
            false,
        )
    } else {
        (
            format!(
                "credential daily token cap of {} has {} used today",
                caps.credential_tokens, totals.credential_tokens
            ),
            false,
        )
    };
    Refusal { reason, agent }
}

/// Budgets with `on_exhaust = warn_only` are not reserved against; log
/// once the agent has gone over.
async fn warn_over_agent_budget(
    db: &SqlitePool,
    cred: &Credential,
    billing: &Billing,
    (day_start, minute_start): (&str, &str),
) {
    let cap = billing.budget.max_tokens_per_day;
    if billing.agent_id.is_none()
        || cap == 0
        || billing.budget.on_exhaust != BudgetExhaustAction::WarnOnly
    {
        return;
    }
    if let Ok(totals) = credential_usage::totals(
        db,
        cred.id,
        billing.user_id,
        billing.agent_id,
        day_start,
        minute_start,
    )
    .await
        && u64::try_from(totals.agent_tokens).is_ok_and(|used| used > cap)
    {
        warn!(
            agent_id = ?billing.agent_id,
            used = totals.agent_tokens,
            cap,
            "agent is over its daily token budget (on_exhaust = warn_only)"
        );
    }
}

/// Upper estimate of a request's input tokens: the bytes of its text
/// (system prompt, messages, tool definitions) at
/// [`INPUT_BYTES_PER_TOKEN`], with each base64 image counted as
/// [`IMAGE_TOKENS`] instead of its encoded size.
fn input_token_estimate(request: &AnthropicRequest) -> i64 {
    fn bytes(value: &serde_json::Value) -> u64 {
        match value {
            serde_json::Value::String(s) => s.len() as u64,
            serde_json::Value::Array(items) => items.iter().map(bytes).sum(),
            serde_json::Value::Object(map) => {
                let image = map.get("type").and_then(serde_json::Value::as_str) == Some("base64")
                    && map
                        .get("media_type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|m| m.starts_with("image/"));
                if image {
                    return IMAGE_TOKENS * INPUT_BYTES_PER_TOKEN;
                }
                map.iter().map(|(k, v)| k.len() as u64 + bytes(v)).sum()
            }
            _ => 4,
        }
    }
    let system = request.system.as_ref().map_or(0, |s| s.len() as u64);
    let messages: u64 = request
        .messages
        .iter()
        .map(|m| m.role.len() as u64 + bytes(&m.content))
        .sum();
    let tools = request.tools.as_ref().map_or(0, bytes);
    i64::try_from((system + messages + tools).div_ceil(INPUT_BYTES_PER_TOKEN)).unwrap_or(i64::MAX)
}

/// Back-compat wrapper. Phase 1 callers used `anthropic_complete`; we keep
/// the name working so external integrations (and the test endpoint) don't
/// break. Equivalent to [`complete_with_provider`] with `"anthropic"`.
pub async fn anthropic_complete(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    billing: &Billing,
    request: &AnthropicRequest,
    max_tokens_ceiling: Option<u32>,
) -> Result<AnthropicResponse, ProxyError> {
    complete_with_provider(
        db,
        http,
        keyring,
        billing,
        "anthropic",
        request,
        max_tokens_ceiling,
    )
    .await
}

fn dispatch_provider(name: &str) -> Result<&'static dyn LlmProvider, ProxyError> {
    // Static dispatch — providers are stateless or hold only `&'static`
    // config (base_url, extra headers), so each lives as a const value.
    // Adding a new provider means one new arm here plus its module.
    static ANTHROPIC: anthropic::AnthropicProvider = anthropic::AnthropicProvider;
    static GEMINI: gemini::GeminiProvider = gemini::GeminiProvider;
    static OPENAI: openai::OpenAiProvider = openai::OpenAiProvider::OPENAI;
    static OPENROUTER: openai::OpenAiProvider = openai::OpenAiProvider::OPENROUTER;

    match name {
        anthropic::AnthropicProvider::NAME => Ok(&ANTHROPIC),
        gemini::GeminiProvider::NAME => Ok(&GEMINI),
        openai::OpenAiProvider::OPENAI_NAME => Ok(&OPENAI),
        openai::OpenAiProvider::OPENROUTER_NAME => Ok(&OPENROUTER),
        other => Err(ProxyError::UnknownProvider(other.to_string())),
    }
}

async fn try_one(
    http: &reqwest::Client,
    keyring: &KeyRing,
    provider: &dyn LlmProvider,
    cred: &Credential,
    request: &AnthropicRequest,
) -> Attempt {
    // Decrypt the at-rest ciphertext to bytes, then verify UTF-8 shape
    // before sending. Decrypt errors collapse to "credential malformed
    // / wrong key" without leaking which — the operator's recourse is
    // the same either way (fix the env, restart). Disable the credential
    // so the resolver doesn't keep grinding on it.
    let plaintext = match keyring.decrypt(&cred.api_key_ciphertext) {
        Ok(p) => p,
        Err(e) => {
            return Attempt::Disable {
                reason: format!(
                    "could not decrypt api_key — wrong HAVN_AGE_KEY or corrupted row ({e})"
                ),
            };
        }
    };
    let Ok(api_key) = std::str::from_utf8(&plaintext) else {
        return Attempt::Disable {
            reason: "decrypted api_key bytes are not valid UTF-8 — credential is malformed".into(),
        };
    };

    match provider.complete(http, api_key, request).await {
        Ok(resp) => Attempt::Ok(resp),
        Err(e) => classify(e),
    }
}

/// Why a request would make the LLM provider fetch something from the
/// network on the agent's behalf: image or document blocks whose `source`
/// is a URL, server-side tools (they carry a `type` that the runtime's own
/// tool definitions never set), or a model with built-in web search. Used
/// to refuse such requests for agents whose policy denies egress. The model
/// check only catches well-known naming patterns; `allowed_models` is the
/// operator's actual control over which models an agent can use.
#[must_use]
pub fn provider_side_fetch(request: &AnthropicRequest) -> Option<&'static str> {
    fn has_url_source(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                let kind = map.get("type").and_then(serde_json::Value::as_str);
                // Tool inputs are data the model wrote, not content the
                // provider would act on.
                if kind == Some("tool_use") {
                    return false;
                }
                let url_source = matches!(kind, Some("image" | "document"))
                    && map
                        .get("source")
                        .and_then(|source| source.get("type"))
                        .and_then(serde_json::Value::as_str)
                        == Some("url");
                url_source || map.values().any(has_url_source)
            }
            serde_json::Value::Array(items) => items.iter().any(has_url_source),
            _ => false,
        }
    }
    if request.messages.iter().any(|m| has_url_source(&m.content)) {
        return Some("message content refers to a URL the provider would fetch");
    }
    let server_tool = request
        .tools
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| kind != "custom")
            })
        });
    if server_tool {
        return Some("request uses a provider-side tool");
    }
    let model = request.model.to_ascii_lowercase();
    if model.ends_with(":online")
        || model.contains("search")
        || model.starts_with("perplexity/")
        || model.contains("sonar")
    {
        return Some("model has built-in web access");
    }
    None
}

/// Whether `model` is permitted by a policy's `allowed_models`: `"*"`
/// allows any model, otherwise the name must be listed exactly (the same
/// semantics the policy uses when narrowing lists).
#[must_use]
pub fn model_allowed(allowed_models: &[String], model: &str) -> bool {
    allowed_models.iter().any(|m| m == "*" || m == model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(content: serde_json::Value, tools: Option<serde_json::Value>) -> AnthropicRequest {
        AnthropicRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 64,
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content,
            }],
            system: None,
            temperature: None,
            tools,
        }
    }

    #[test]
    fn plain_requests_fetch_nothing() {
        let tools = json!([{"name": "shell", "description": "run", "input_schema": {}}]);
        assert_eq!(
            provider_side_fetch(&request(json!("hello"), Some(tools))),
            None
        );
        let base64_image = json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}]);
        assert_eq!(provider_side_fetch(&request(base64_image, None)), None);
    }

    #[test]
    fn url_sources_are_detected_anywhere_in_the_content() {
        let direct = json!([{"type": "image", "source": {"type": "url", "url": "https://example.test/?d=x"}}]);
        assert!(provider_side_fetch(&request(direct, None)).is_some());
        let nested = json!([{"type": "tool_result", "tool_use_id": "t1", "content": [
            {"type": "document", "source": {"type": "url", "url": "https://example.test/doc.pdf"}}
        ]}]);
        assert!(provider_side_fetch(&request(nested, None)).is_some());
    }

    #[test]
    fn tool_inputs_and_other_blocks_are_not_flagged() {
        let tool_use = json!([{"type": "tool_use", "id": "t1", "name": "mcp_fetch",
            "input": {"source": {"type": "url", "url": "https://example.test/"}}}]);
        assert_eq!(provider_side_fetch(&request(tool_use, None)), None);
    }

    #[test]
    fn web_search_models_are_detected() {
        for model in [
            "openai/gpt-4o:online",
            "gpt-4o-search-preview",
            "perplexity/sonar-pro",
        ] {
            let mut req = request(json!("hi"), None);
            req.model = model.into();
            assert!(provider_side_fetch(&req).is_some(), "{model}");
        }
    }

    #[test]
    fn allowed_models_are_matched_exactly_or_by_star() {
        assert!(model_allowed(&["*".into()], "anything"));
        let list = vec!["deepseek/deepseek-v4.1-flash-1017".to_string()];
        assert!(model_allowed(&list, "deepseek/deepseek-v4.1-flash-1017"));
        assert!(!model_allowed(&list, "openai/gpt-4o:online"));
        assert!(!model_allowed(&[], "anything"));
    }

    #[test]
    fn server_side_tools_are_detected() {
        let tools = json!([{"type": "web_search_20250305", "name": "web_search"}]);
        assert!(provider_side_fetch(&request(json!("hi"), Some(tools))).is_some());
        let custom = json!([{"type": "custom", "name": "shell", "input_schema": {}}]);
        assert_eq!(
            provider_side_fetch(&request(json!("hi"), Some(custom))),
            None
        );
    }

    mod budgets {
        #![allow(clippy::expect_used, clippy::unwrap_used)]

        use std::sync::{Arc, Mutex, OnceLock};
        use std::time::Duration;

        use async_trait::async_trait;
        use havn_db::repo::agents::{NewAgent, create as create_agent};
        use havn_db::repo::credentials::{CredentialScope, NewCredential, create as create_cred};
        use havn_db::repo::users::{NewUser, create as create_user};
        use serde_json::json;

        use super::super::provider::ProviderError;
        use super::super::*;

        const PASSPHRASE: &str = "budget-tests";

        /// Encrypting calibrates scrypt to about a second; do it once.
        fn ciphertext() -> &'static [u8] {
            static CIPHERTEXT: OnceLock<Vec<u8>> = OnceLock::new();
            CIPHERTEXT.get_or_init(|| {
                KeyRing::from_passphrase(PASSPHRASE)
                    .encrypt(b"sk-test")
                    .expect("encrypt")
            })
        }

        /// Records the `max_tokens` of each call and answers with
        /// `output_tokens = max_tokens`, or fails with a 500.
        struct FakeProvider {
            fail: bool,
            delay: Duration,
            seen: Mutex<Vec<u32>>,
        }

        impl FakeProvider {
            fn new(fail: bool, delay: Duration) -> Arc<Self> {
                Arc::new(Self {
                    fail,
                    delay,
                    seen: Mutex::new(Vec::new()),
                })
            }
        }

        #[async_trait]
        impl LlmProvider for FakeProvider {
            fn name(&self) -> &'static str {
                "anthropic"
            }

            async fn complete(
                &self,
                _http: &reqwest::Client,
                api_key: &str,
                request: &AnthropicRequest,
            ) -> Result<AnthropicResponse, ProviderError> {
                assert_eq!(api_key, "sk-test");
                self.seen.lock().unwrap().push(request.max_tokens);
                tokio::time::sleep(self.delay).await;
                if self.fail {
                    return Err(ProviderError::Upstream {
                        status: 500,
                        body: "down".into(),
                    });
                }
                Ok(AnthropicResponse {
                    id: "msg_1".into(),
                    model: format!("{}-20260101", request.model),
                    role: "assistant".into(),
                    content: json!([{"type": "text", "text": "ok"}]),
                    stop_reason: Some("end_turn".into()),
                    usage: AnthropicUsage {
                        input_tokens: 1,
                        output_tokens: u64::from(request.max_tokens),
                    },
                })
            }
        }

        struct Fixture {
            db: SqlitePool,
            billing: Billing,
        }

        async fn fixture(db: SqlitePool, limits: serde_json::Value) -> Fixture {
            let user_id = create_user(&db, NewUser { display_name: "u" })
                .await
                .expect("user")
                .id;
            let agent_id = create_agent(
                &db,
                NewAgent {
                    owner_id: user_id,
                    team_id: None,
                    name: "a",
                    config: json!({}),
                },
            )
            .await
            .expect("agent")
            .id;
            create_cred(
                &db,
                NewCredential {
                    scope: CredentialScope::User,
                    scope_id: &user_id.to_string(),
                    provider: "anthropic",
                    name: None,
                    api_key_ciphertext: ciphertext(),
                    priority: 1,
                    limits,
                },
            )
            .await
            .expect("credential");
            Fixture {
                db,
                billing: Billing {
                    user_id,
                    agent_id: Some(agent_id),
                    budget: BudgetPolicy::default(),
                },
            }
        }

        fn ask(max_tokens: u32) -> AnthropicRequest {
            AnthropicRequest {
                model: "claude-sonnet-4-6".into(),
                max_tokens,
                messages: vec![AnthropicMessage {
                    role: "user".into(),
                    content: json!("hi"),
                }],
                system: None,
                temperature: None,
                tools: None,
            }
        }

        async fn call(
            f: &Fixture,
            provider: &FakeProvider,
            billing: &Billing,
            request: &AnthropicRequest,
        ) -> Result<AnthropicResponse, ProxyError> {
            complete_through(
                &f.db,
                &reqwest::Client::new(),
                &KeyRing::from_passphrase(PASSPHRASE),
                provider,
                "anthropic",
                billing,
                request,
            )
            .await
        }

        async fn rows(db: &SqlitePool) -> Vec<(Option<String>, String, i64, i64)> {
            sqlx::query_as("SELECT agent_id, model, tokens_in, tokens_out FROM credential_usages")
                .fetch_all(db)
                .await
                .expect("rows")
        }

        #[tokio::test]
        async fn settlement_records_actual_usage_and_agent() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({})).await;
            let provider = FakeProvider::new(false, Duration::ZERO);
            call(&f, &provider, &f.billing, &ask(300))
                .await
                .expect("ok");
            let agent = f.billing.agent_id.map(|a| a.to_string());
            assert_eq!(
                rows(&f.db).await,
                vec![(agent, "claude-sonnet-4-6-20260101".into(), 1, 300)]
            );
        }

        #[tokio::test]
        async fn reservation_is_released_on_upstream_error() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({"max_tokens_per_day": 10_000})).await;
            let provider = FakeProvider::new(true, Duration::ZERO);
            let err = call(&f, &provider, &f.billing, &ask(300))
                .await
                .expect_err("upstream down");
            assert!(matches!(err, ProxyError::NoCredential(_)), "{err}");
            assert_eq!(provider.seen.lock().unwrap().len(), 1);
            assert!(rows(&f.db).await.is_empty());
        }

        #[tokio::test]
        async fn max_tokens_shrinks_to_what_the_cap_leaves() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({"max_tokens_per_day": 2_000})).await;
            let provider = FakeProvider::new(false, Duration::ZERO);
            let request = ask(8_192);
            call(&f, &provider, &f.billing, &request).await.expect("ok");
            let granted = 2_000 - u32::try_from(input_token_estimate(&request)).unwrap();
            assert_eq!(*provider.seen.lock().unwrap(), vec![granted]);
            // The cap is now used up: the next call never reaches upstream.
            let err = call(&f, &provider, &f.billing, &request)
                .await
                .expect_err("exhausted");
            assert!(matches!(err, ProxyError::BudgetExhausted(_)), "{err}");
            assert_eq!(provider.seen.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn agent_budget_is_enforced_unless_warn_only() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({})).await;
            let provider = FakeProvider::new(false, Duration::ZERO);
            let mut billing = f.billing.clone();
            billing.budget.max_tokens_per_day = 100;
            billing.budget.on_exhaust = BudgetExhaustAction::Fail;
            let err = call(&f, &provider, &billing, &ask(1_000))
                .await
                .expect_err("over budget");
            assert!(
                matches!(&err, ProxyError::BudgetExhausted(r) if r.starts_with("agent")),
                "{err}"
            );
            assert!(provider.seen.lock().unwrap().is_empty());

            billing.budget.on_exhaust = BudgetExhaustAction::WarnOnly;
            call(&f, &provider, &billing, &ask(1_000))
                .await
                .expect("warn_only does not block");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn parallel_requests_cannot_overspend_the_cap() {
            let path =
                std::env::temp_dir().join(format!("havn-proxy-budget-{}.db", uuid::Uuid::now_v7()));
            let db = havn_db::connect(&path).await.expect("db");
            let f = Arc::new(fixture(db, json!({"max_tokens_per_day": 3_000})).await);
            let provider = FakeProvider::new(false, Duration::from_millis(200));
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..8 {
                let f = Arc::clone(&f);
                let provider = Arc::clone(&provider);
                tasks.spawn(async move { call(&f, &provider, &f.billing, &ask(1_000)).await });
            }
            let mut ok = 0;
            while let Some(result) = tasks.join_next().await {
                match result.expect("join") {
                    Ok(_) => ok += 1,
                    Err(ProxyError::BudgetExhausted(_)) => {}
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
            // Two full reservations of 1,000 output tokens plus the input
            // estimate, then one shrunk to what remains; the rest are refused
            // while all of them are in flight together.
            assert_eq!(ok, 3);
            let used: i64 = rows(&f.db).await.iter().map(|r| r.2 + r.3).sum();
            assert!(used <= 3_000, "used {used}");
            f.db.close().await;
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }

        #[test]
        fn input_estimate_counts_text_and_images() {
            let mut request = ask(1);
            request.system = Some("x".repeat(100));
            request.messages[0].content = json!([
                {"type": "text", "text": "y".repeat(200)},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                    "data": "A".repeat(1_000_000)}}
            ]);
            let estimate = input_token_estimate(&request);
            let image = i64::try_from(IMAGE_TOKENS).unwrap();
            assert!(
                estimate > 150 + image && estimate < 200 + image,
                "{estimate}"
            );
        }
    }
}
