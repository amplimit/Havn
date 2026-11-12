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

use havn_core::{AgentId, BudgetExhaustAction, BudgetPolicy, TeamId, UserId};
use havn_db::repo::credential_usage::{self, Caps, Reservation};
use havn_db::repo::credentials::Credential;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::{info, warn};

use crate::config::LlmConfig;
use crate::credential_resolver::{
    credential_caps, minute_ago_utc_rfc3339, resolve_for_user, today_start_utc_rfc3339,
};
use crate::keyring::KeyRing;
use provider::{LlmProvider, classify};

/// Smallest output allowance worth sending upstream when a token cap
/// leaves less than the request's `max_tokens`. Below it the credential
/// counts as exhausted.
const MIN_OUTPUT_TOKENS: u32 = 256;

/// ASCII bytes of request text counted as one input token when reserving
/// (tokenizers average about 4 for English). Other characters count as one
/// token each (CJK text runs at roughly 1 to 1.5 characters per token), and
/// characters outside the BMP as one per UTF-8 byte. This over-reserves;
/// the reservation is settled to the provider's reported usage once the
/// call returns.
const ASCII_BYTES_PER_TOKEN: u64 = 2;

/// Least tokens reserved per image or document block, whatever its source.
/// Claude bills at most about 1,600 tokens for an image at standard
/// resolution and about 4,800 at high resolution.
const IMAGE_TOKENS: u64 = 5_000;

/// Least tokens reserved for a URL-sourced document, whose size the
/// gateway cannot see: a large PDF runs to 100k-300k tokens. A base64
/// document is instead counted from its data, so a huge one can be
/// refused outright.
const URL_DOCUMENT_TOKENS: u64 = 100_000;

/// Who an LLM call is billed to and the agent budget it must fit in.
#[derive(Debug, Clone)]
pub struct Billing {
    pub user_id: UserId,
    /// `None` for gateway-direct calls (the test endpoint).
    pub agent_id: Option<AgentId>,
    /// The calling agent's team; limits the team credentials tried (see
    /// [`resolve_for_user`]). `None` for personal agents and direct calls.
    pub agent_team: Option<TeamId>,
    /// The agent's policy budget; ignored without `agent_id`.
    pub budget: BudgetPolicy,
}

impl Billing {
    #[must_use]
    pub fn user(user_id: UserId) -> Self {
        Self {
            user_id,
            agent_id: None,
            agent_team: None,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AnthropicUsage {
    /// Excludes prompt-cache writes and reads, reported apart below.
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// The provider's response carried no usage. The proxy then keeps the
    /// request's reservation as its recorded usage.
    #[serde(skip)]
    pub unreported: bool,
}

impl AnthropicUsage {
    #[must_use]
    pub fn unreported() -> Self {
        Self {
            unreported: true,
            ..Self::default()
        }
    }

    /// Every input token the call consumed, cache writes and reads included.
    #[must_use]
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
            .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
    }
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
    llm: &LlmConfig,
) -> Result<AnthropicResponse, ProxyError> {
    let provider_name = provider::provider_for_model(&request.model);
    complete_with_provider(db, http, keyring, billing, provider_name, request, llm).await
}

/// Like [`complete`], but the caller has already decided which provider to
/// use (e.g. the runtime read it out of `LlmRequest.options.provider`).
pub async fn complete_with_provider(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    billing: &Billing,
    provider_name: &str,
    request: &AnthropicRequest,
    llm: &LlmConfig,
) -> Result<AnthropicResponse, ProxyError> {
    let provider = dispatch_provider(provider_name)?;
    let route = Route {
        provider,
        name: provider_name,
        base_url: llm
            .base_url
            .get(provider_name)
            .map_or(provider.default_base_url(), |url| url.trim_end_matches('/')),
    };
    complete_through(
        db,
        http,
        keyring,
        route,
        billing,
        request,
        llm.max_tokens_ceiling,
    )
    .await
}

/// The provider implementation a call goes through, the provider name
/// its credentials are stored under, and the API base URL it calls.
#[derive(Clone, Copy)]
struct Route<'a> {
    provider: &'a dyn LlmProvider,
    name: &'a str,
    base_url: &'a str,
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

/// `max_tokens` is clamped to the model's known output limit and to
/// `max_tokens_ceiling` before anything is reserved or sent.
async fn complete_through(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    route: Route<'_>,
    billing: &Billing,
    request: &AnthropicRequest,
    max_tokens_ceiling: u32,
) -> Result<AnthropicResponse, ProxyError> {
    let max_tokens =
        provider::clamp_max_tokens(&request.model, request.max_tokens, max_tokens_ceiling);
    let request = with_max_tokens(request, max_tokens);
    let request = request.as_ref();

    let chain = resolve_for_user(db, billing.user_id, billing.agent_team, route.name).await?;
    if chain.is_empty() {
        return Err(ProxyError::NoCredential(route.name.to_string()));
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
    let mut db_failure: Option<havn_db::DbError> = None;
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
            provider: route.name,
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
                db_failure = None;
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
                db_failure = Some(e);
                continue;
            }
        };
        db_failure = None;
        let guard = ReservationGuard::new(db, reserved.id);
        let granted = u32::try_from(reserved.tokens_out).unwrap_or(request.max_tokens);
        let attempt_request = with_max_tokens(request, granted.min(request.max_tokens));

        match try_one(http, keyring, route, &cred, &attempt_request).await {
            Attempt::Ok(resp) => {
                if resp.usage.unreported {
                    warn!(
                        credential_id = %cred.id,
                        model = %resp.model,
                        "provider reported no token usage; keeping the reservation as usage"
                    );
                    guard.keep();
                } else {
                    guard.settle(&resp).await;
                }
                info!(
                    credential_id = %cred.id,
                    provider = route.name,
                    model = %resp.model,
                    in = resp.usage.total_input_tokens(),
                    out = resp.usage.output_tokens,
                    "llm proxy ok"
                );
                warn_over_agent_budget(db, &cred, billing, (&day_start, &minute_start)).await;
                return Ok(resp);
            }
            Attempt::Disable { reason } => {
                guard.release().await;
                warn!(credential_id = %cred.id, %reason, "disabling credential");
                let _ = havn_db::repo::credentials::set_enabled(db, cred.id, false)
                    .await
                    .map_err(|e| warn!(error = %e, "failed to disable credential"));
            }
            Attempt::TryNext { reason } => {
                guard.release().await;
                warn!(credential_id = %cred.id, %reason, "skipping credential, trying next");
            }
            Attempt::Fatal(e) => {
                guard.release().await;
                return Err(e);
            }
        }
    }

    if let Some(e) = db_failure {
        return Err(ProxyError::Db(e));
    }
    Err(exhausted.map_or_else(
        || ProxyError::NoCredential(route.name.to_string()),
        ProxyError::BudgetExhausted,
    ))
}

/// Owns a reservation row until it is settled, released or kept. Dropped
/// while armed (the caller's future was cancelled mid-call, e.g. an HTTP
/// client disconnected), it releases the row from a spawned task.
struct ReservationGuard {
    db: SqlitePool,
    id: Option<String>,
}

impl ReservationGuard {
    fn new(db: &SqlitePool, id: String) -> Self {
        Self {
            db: db.clone(),
            id: Some(id),
        }
    }

    async fn settle(mut self, resp: &AnthropicResponse) {
        let Some(id) = self.id.take() else { return };
        let tokens_in = i64::try_from(resp.usage.total_input_tokens()).unwrap_or(i64::MAX);
        let tokens_out = i64::try_from(resp.usage.output_tokens).unwrap_or(i64::MAX);
        // A failed write leaves the reservation in place, which
        // over-counts rather than under-counts.
        if let Err(e) =
            credential_usage::settle(&self.db, &id, &resp.model, tokens_in, tokens_out).await
        {
            warn!(error = %e, "usage settlement failed (non-fatal)");
        }
    }

    async fn release(mut self) {
        if let Some(id) = self.id.take() {
            release(&self.db, &id).await;
        }
    }

    /// Leave the reservation as the recorded usage.
    fn keep(mut self) {
        self.id = None;
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let db = self.db.clone();
                runtime.spawn(async move { release(&db, &id).await });
            }
            Err(_) => warn!(reservation_id = %id, "no runtime to release a token reservation"),
        }
    }
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

/// Upper estimate of a request's input tokens (system prompt, messages,
/// tool definitions), text counted as [`ASCII_BYTES_PER_TOKEN`] describes.
/// An image block counts as at least [`IMAGE_TOKENS`] whatever its source,
/// without its base64 data; a document block as at least [`IMAGE_TOKENS`]
/// with its data included, or [`URL_DOCUMENT_TOKENS`] when it comes from a
/// URL. Tool inputs are plain data the model wrote and are counted as text.
fn input_token_estimate(request: &AnthropicRequest) -> i64 {
    #[derive(Default)]
    struct Count {
        ascii: u64,
        other: u64,
        fixed: u64,
    }

    impl Count {
        fn text(&mut self, s: &str) {
            for c in s.chars() {
                match c.len_utf8() {
                    1 => self.ascii += 1,
                    4 => self.other += 4,
                    _ => self.other += 1,
                }
            }
        }

        fn tokens(&self) -> u64 {
            self.ascii.div_ceil(ASCII_BYTES_PER_TOKEN) + self.other + self.fixed
        }

        fn walk(&mut self, value: &serde_json::Value, blocks: bool) {
            match value {
                serde_json::Value::String(s) => self.text(s),
                serde_json::Value::Array(items) => {
                    for item in items {
                        self.walk(item, blocks);
                    }
                }
                serde_json::Value::Object(map) => {
                    let kind = map.get("type").and_then(serde_json::Value::as_str);
                    let url_source = map
                        .get("source")
                        .and_then(|source| source.get("type"))
                        .and_then(serde_json::Value::as_str)
                        == Some("url");
                    let (blocks, floor) = match kind {
                        _ if !blocks => (false, None),
                        Some("tool_use") => (false, None),
                        Some("document") if url_source => (true, Some(URL_DOCUMENT_TOKENS)),
                        Some("image" | "document") => (true, Some(IMAGE_TOKENS)),
                        _ => (true, None),
                    };
                    let mut inner = Self::default();
                    for (key, item) in map {
                        inner.text(key);
                        match (kind, key.as_str(), item) {
                            (Some("image"), "source", serde_json::Value::Object(source))
                                if floor.is_some() =>
                            {
                                for (k, v) in source.iter().filter(|(k, _)| *k != "data") {
                                    inner.text(k);
                                    inner.walk(v, false);
                                }
                            }
                            _ => inner.walk(item, blocks),
                        }
                    }
                    match floor {
                        Some(floor) => self.fixed += inner.tokens().max(floor),
                        None => {
                            self.ascii += inner.ascii;
                            self.other += inner.other;
                            self.fixed += inner.fixed;
                        }
                    }
                }
                _ => self.ascii += 4,
            }
        }
    }

    let mut count = Count::default();
    if let Some(system) = &request.system {
        count.text(system);
    }
    for message in &request.messages {
        count.text(&message.role);
        count.walk(&message.content, true);
    }
    if let Some(tools) = &request.tools {
        count.walk(tools, false);
    }
    i64::try_from(count.tokens()).unwrap_or(i64::MAX)
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
    llm: &LlmConfig,
) -> Result<AnthropicResponse, ProxyError> {
    complete_with_provider(db, http, keyring, billing, "anthropic", request, llm).await
}

/// HTTP client for provider calls. Redirects are not followed: requests
/// carry the provider's API key, which must not reach whatever host a
/// redirect names.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Whether `name` is a provider the proxy can call.
#[must_use]
pub fn is_provider(name: &str) -> bool {
    dispatch_provider(name).is_ok()
}

fn dispatch_provider(name: &str) -> Result<&'static dyn LlmProvider, ProxyError> {
    // Static dispatch — providers are stateless or hold only `&'static`
    // config (default base URL, extra headers), so each lives as a const
    // value; the operator's `[llm.base_url]` travels in the `Route`.
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
    route: Route<'_>,
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

    match route
        .provider
        .complete(http, route.base_url, api_key, request)
        .await
    {
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
        const CEILING: u32 = 128_000;

        /// Encrypting calibrates scrypt to about a second; do it once.
        fn ciphertext() -> &'static [u8] {
            static CIPHERTEXT: OnceLock<Vec<u8>> = OnceLock::new();
            CIPHERTEXT.get_or_init(|| {
                KeyRing::from_passphrase(PASSPHRASE)
                    .encrypt(b"sk-test")
                    .expect("encrypt")
            })
        }

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Reply {
            /// `output_tokens = max_tokens`, one input token.
            Ok,
            /// As `Ok`, plus 100 prompt-cache reads.
            Cached,
            /// As `Ok`, without usage.
            Unreported,
            /// HTTP 500.
            Fail,
        }

        /// Records the `max_tokens` of each call and answers per `reply`.
        struct FakeProvider {
            reply: Reply,
            delay: Duration,
            seen: Mutex<Vec<u32>>,
        }

        impl FakeProvider {
            fn new(reply: Reply, delay: Duration) -> Arc<Self> {
                Arc::new(Self {
                    reply,
                    delay,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn seen(&self) -> Vec<u32> {
                self.seen.lock().unwrap().clone()
            }
        }

        #[async_trait]
        impl LlmProvider for FakeProvider {
            fn name(&self) -> &'static str {
                "anthropic"
            }

            fn default_base_url(&self) -> &'static str {
                "http://fake.invalid"
            }

            async fn complete(
                &self,
                _http: &reqwest::Client,
                _base_url: &str,
                api_key: &str,
                request: &AnthropicRequest,
            ) -> Result<AnthropicResponse, ProviderError> {
                assert_eq!(api_key, "sk-test");
                self.seen.lock().unwrap().push(request.max_tokens);
                tokio::time::sleep(self.delay).await;
                let usage = match self.reply {
                    Reply::Fail => {
                        return Err(ProviderError::Upstream {
                            status: 500,
                            body: "down".into(),
                        });
                    }
                    Reply::Unreported => AnthropicUsage::unreported(),
                    Reply::Ok | Reply::Cached => AnthropicUsage {
                        input_tokens: 1,
                        output_tokens: u64::from(request.max_tokens),
                        cache_read_input_tokens: (self.reply == Reply::Cached).then_some(100),
                        ..AnthropicUsage::default()
                    },
                };
                Ok(AnthropicResponse {
                    id: "msg_1".into(),
                    model: format!("{}-20260101", request.model),
                    role: "assistant".into(),
                    content: json!([{"type": "text", "text": "ok"}]),
                    stop_reason: Some("end_turn".into()),
                    usage,
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
                    agent_team: None,
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

        async fn call_with(
            f: &Fixture,
            provider: &FakeProvider,
            billing: &Billing,
            request: &AnthropicRequest,
            ceiling: u32,
        ) -> Result<AnthropicResponse, ProxyError> {
            complete_through(
                &f.db,
                &reqwest::Client::new(),
                &KeyRing::from_passphrase(PASSPHRASE),
                Route {
                    provider,
                    name: "anthropic",
                    base_url: provider.default_base_url(),
                },
                billing,
                request,
                ceiling,
            )
            .await
        }

        async fn call(
            f: &Fixture,
            provider: &FakeProvider,
            request: &AnthropicRequest,
        ) -> Result<AnthropicResponse, ProxyError> {
            call_with(f, provider, &f.billing, request, CEILING).await
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
            let provider = FakeProvider::new(Reply::Cached, Duration::ZERO);
            call(&f, &provider, &ask(300)).await.expect("ok");
            let agent = f.billing.agent_id.map(|a| a.to_string());
            // One fresh input token plus 100 read from the prompt cache.
            assert_eq!(
                rows(&f.db).await,
                vec![(agent, "claude-sonnet-4-6-20260101".into(), 101, 300)]
            );
        }

        #[tokio::test]
        async fn missing_usage_keeps_the_reservation() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({})).await;
            let provider = FakeProvider::new(Reply::Unreported, Duration::ZERO);
            let request = ask(300);
            call(&f, &provider, &request).await.expect("ok");
            let row = rows(&f.db).await.pop().expect("row");
            assert_eq!((row.2, row.3), (input_token_estimate(&request), 300));
        }

        #[tokio::test]
        async fn reservation_is_released_on_upstream_error() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({"max_tokens_per_day": 10_000})).await;
            let provider = FakeProvider::new(Reply::Fail, Duration::ZERO);
            let err = call(&f, &provider, &ask(300))
                .await
                .expect_err("upstream down");
            assert!(matches!(err, ProxyError::NoCredential(_)), "{err}");
            assert_eq!(provider.seen().len(), 1);
            assert!(rows(&f.db).await.is_empty());
        }

        #[tokio::test]
        async fn reservation_is_released_when_the_call_is_dropped() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = Arc::new(fixture(db, json!({})).await);
            let provider = FakeProvider::new(Reply::Ok, Duration::from_secs(60));
            let task = {
                let (f, provider) = (Arc::clone(&f), Arc::clone(&provider));
                tokio::spawn(async move { call(&f, &provider, &ask(300)).await })
            };
            while provider.seen().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(rows(&f.db).await.len(), 1);
            task.abort();
            let _ = task.await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !rows(&f.db).await.is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("reservation released");
        }

        #[tokio::test]
        async fn database_failures_are_reported_as_such() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({})).await;
            sqlx::query("DROP TABLE credential_usages")
                .execute(&f.db)
                .await
                .expect("drop");
            let provider = FakeProvider::new(Reply::Ok, Duration::ZERO);
            let err = call(&f, &provider, &ask(300)).await.expect_err("no table");
            assert!(matches!(err, ProxyError::Db(_)), "{err}");
            assert!(provider.seen().is_empty());
        }

        #[tokio::test]
        async fn max_tokens_is_clamped_before_reserving() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({"max_tokens_per_day": 5_000})).await;
            // Without usage in the reply the reservation stays as the row.
            let provider = FakeProvider::new(Reply::Unreported, Duration::ZERO);
            let mut request = ask(8_192);
            // Output limit 4,096. Unclamped, the reservation would grant the
            // 4,997 tokens the cap leaves.
            request.model = "claude-3-haiku-20240307".into();
            call(&f, &provider, &request).await.expect("ok");
            assert_eq!(provider.seen(), vec![4_096]);
            assert_eq!(rows(&f.db).await.pop().expect("row").3, 4_096);

            let f = fixture(havn_db::connect_in_memory().await.expect("db"), json!({})).await;
            let provider = FakeProvider::new(Reply::Unreported, Duration::ZERO);
            call_with(&f, &provider, &f.billing, &ask(8_192), 500)
                .await
                .expect("ok");
            assert_eq!(provider.seen(), vec![500]);
            assert_eq!(rows(&f.db).await.pop().expect("row").3, 500);
        }

        #[tokio::test]
        async fn max_tokens_shrinks_to_what_the_cap_leaves() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({"max_tokens_per_day": 2_000})).await;
            let provider = FakeProvider::new(Reply::Ok, Duration::ZERO);
            let request = ask(8_192);
            call(&f, &provider, &request).await.expect("ok");
            let granted = 2_000 - u32::try_from(input_token_estimate(&request)).unwrap();
            assert_eq!(provider.seen(), vec![granted]);
            // The cap is now used up: the next call never reaches upstream.
            let err = call(&f, &provider, &request).await.expect_err("exhausted");
            assert!(matches!(err, ProxyError::BudgetExhausted(_)), "{err}");
            assert_eq!(provider.seen().len(), 1);
        }

        #[tokio::test]
        async fn agent_budget_is_enforced_unless_warn_only() {
            let db = havn_db::connect_in_memory().await.expect("db");
            let f = fixture(db, json!({})).await;
            let provider = FakeProvider::new(Reply::Ok, Duration::ZERO);
            let mut billing = f.billing.clone();
            billing.budget.max_tokens_per_day = 100;
            billing.budget.on_exhaust = BudgetExhaustAction::Fail;
            let err = call_with(&f, &provider, &billing, &ask(1_000), CEILING)
                .await
                .expect_err("over budget");
            assert!(
                matches!(&err, ProxyError::BudgetExhausted(r) if r.starts_with("agent")),
                "{err}"
            );
            assert!(provider.seen().is_empty());

            billing.budget.on_exhaust = BudgetExhaustAction::WarnOnly;
            call_with(&f, &provider, &billing, &ask(1_000), CEILING)
                .await
                .expect("warn_only does not block");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn parallel_requests_cannot_overspend_the_cap() {
            let dir = tempfile::tempdir().expect("tempdir");
            let db = havn_db::connect(dir.path().join("havn.db"))
                .await
                .expect("db");
            let f = Arc::new(fixture(db, json!({"max_tokens_per_day": 3_000})).await);
            let provider = FakeProvider::new(Reply::Ok, Duration::from_millis(200));
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..8 {
                let (f, provider) = (Arc::clone(&f), Arc::clone(&provider));
                tasks.spawn(async move { call(&f, &provider, &ask(1_000)).await });
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
        }

        fn estimate(content: serde_json::Value) -> i64 {
            let mut request = ask(1);
            request.messages[0].content = content;
            input_token_estimate(&request)
        }

        #[test]
        fn input_estimate_counts_text_images_and_documents() {
            let image = i64::try_from(IMAGE_TOKENS).unwrap();
            // "user" + "text" keys and value + 200 ASCII bytes, 2 bytes a token.
            assert_eq!(
                estimate(json!([{"type": "text", "text": "y".repeat(200)}])),
                108
            );
            let base64 = json!([{"type": "image", "source": {"type": "base64",
                "media_type": "image/png", "data": "A".repeat(1_000_000)}}]);
            assert_eq!(estimate(base64), 2 + image);
            let url = json!([{"type": "image", "source": {"type": "url",
                "url": "https://example.test/a.png"}}]);
            assert_eq!(estimate(url), 2 + image);
            let nested = json!([{"type": "tool_result", "tool_use_id": "t", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://example.test/a.png"}}
            ]}]);
            assert!(estimate(nested) > image);
            let pdf = json!([{"type": "document", "source": {"type": "base64",
                "media_type": "application/pdf", "data": "A".repeat(100_000)}}]);
            assert!(estimate(pdf) > 50_000);
            let url_pdf = json!([{"type": "document", "source": {"type": "url",
                "url": "https://example.test/big.pdf"}}]);
            assert_eq!(
                estimate(url_pdf),
                2 + i64::try_from(URL_DOCUMENT_TOKENS).unwrap()
            );
        }

        #[test]
        fn input_estimate_counts_non_ascii_bytes_and_tool_inputs() {
            // A CJK character is one token; one outside the BMP is four.
            assert_eq!(estimate(json!("\u{4e2d}\u{6587}".repeat(50))), 102);
            assert_eq!(estimate(json!("\u{20000}".repeat(10))), 42);
            // An "image" inside tool input is data, counted in full.
            let tool_use = json!([{"type": "tool_use", "id": "t", "name": "n", "input":
                {"type": "image", "source": {"data": "A".repeat(100_000)}}}]);
            assert!(estimate(tool_use) > 50_000);
        }
    }
}
