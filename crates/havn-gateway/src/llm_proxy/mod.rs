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

use havn_core::UserId;
use havn_db::repo::credential_usage::{self, NewUsage};
use havn_db::repo::credentials::Credential;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::{info, warn};

use crate::credential_resolver::{
    PerUserVerdict, daily_tokens_remaining, per_user_ceilings_remaining, resolve_for_user,
};
use crate::keyring::KeyRing;
use provider::{LlmProvider, classify};

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
/// - 200 → record usage, return canonical response.
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
    user_id: UserId,
    request: &AnthropicRequest,
) -> Result<AnthropicResponse, ProxyError> {
    let provider_name = provider::provider_for_model(&request.model);
    complete_with_provider(db, http, keyring, user_id, provider_name, request).await
}

/// Like [`complete`], but the caller has already decided which provider to
/// use (e.g. the runtime read it out of `LlmRequest.options.provider`).
pub async fn complete_with_provider(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    user_id: UserId,
    provider_name: &str,
    request: &AnthropicRequest,
) -> Result<AnthropicResponse, ProxyError> {
    let provider = dispatch_provider(provider_name)?;
    let chain = resolve_for_user(db, user_id, provider_name).await?;
    if chain.is_empty() {
        return Err(ProxyError::NoCredential(provider_name.to_string()));
    }

    for cred in chain {
        // Pre-call budget gate (spec §6.2 / §7.3). A compromised agent must
        // not be able to burn unlimited tokens on one key — if today's
        // token cap is exhausted, fall through to the next credential
        // just like a 402. Soft cap: we don't predict this request's
        // token usage, so the per-day total can overshoot by up to one
        // request's worth. Acceptable v0.6.
        match daily_tokens_remaining(db, &cred).await {
            Ok(Some(remaining)) if remaining <= 0 => {
                warn!(
                    credential_id = %cred.id,
                    cap = ?cred.limits.get("max_tokens_per_day"),
                    "daily token cap exhausted; skipping credential"
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                warn!(
                    credential_id = %cred.id,
                    error = %e,
                    "budget check failed; skipping credential"
                );
                continue;
            }
        }

        // Per-user gate (spec §10.3) — for shared team credentials, an
        // admin can clamp how much one user can spend.
        match per_user_ceilings_remaining(db, &cred, user_id).await {
            Ok(PerUserVerdict::Allowed) => {}
            Ok(PerUserVerdict::DenyTokens) => {
                warn!(
                    credential_id = %cred.id, %user_id,
                    "per-user daily token cap exhausted; skipping credential"
                );
                continue;
            }
            Ok(PerUserVerdict::DenyRequests) => {
                warn!(
                    credential_id = %cred.id, %user_id,
                    "per-user RPM cap exhausted; skipping credential"
                );
                continue;
            }
            Err(e) => {
                warn!(
                    credential_id = %cred.id,
                    error = %e,
                    "per-user budget check failed; skipping credential"
                );
                continue;
            }
        }

        match try_one(http, keyring, provider, &cred, request).await {
            Attempt::Ok(resp) => {
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "token counts < 2^31 in any realistic call"
                )]
                let _ = credential_usage::record(
                    db,
                    NewUsage {
                        credential_id: cred.id,
                        user_id,
                        agent_id: None,
                        provider: provider_name,
                        model: &resp.model,
                        tokens_in: resp.usage.input_tokens as i64,
                        tokens_out: resp.usage.output_tokens as i64,
                    },
                )
                .await
                .map_err(|e| warn!(error = %e, "usage record write failed (non-fatal)"));
                info!(
                    credential_id = %cred.id,
                    provider = provider_name,
                    model = %resp.model,
                    in = resp.usage.input_tokens,
                    out = resp.usage.output_tokens,
                    "llm proxy ok"
                );
                return Ok(resp);
            }
            Attempt::Disable { reason } => {
                warn!(credential_id = %cred.id, %reason, "disabling credential");
                let _ = havn_db::repo::credentials::set_enabled(db, cred.id, false)
                    .await
                    .map_err(|e| warn!(error = %e, "failed to disable credential"));
            }
            Attempt::TryNext { reason } => {
                warn!(credential_id = %cred.id, %reason, "skipping credential, trying next");
            }
            Attempt::Fatal(e) => return Err(e),
        }
    }

    Err(ProxyError::NoCredential(provider_name.to_string()))
}

/// Back-compat wrapper. Phase 1 callers used `anthropic_complete`; we keep
/// the name working so external integrations (and the test endpoint) don't
/// break. Equivalent to [`complete_with_provider`] with `"anthropic"`.
pub async fn anthropic_complete(
    db: &SqlitePool,
    http: &reqwest::Client,
    keyring: &KeyRing,
    user_id: UserId,
    request: &AnthropicRequest,
) -> Result<AnthropicResponse, ProxyError> {
    complete_with_provider(db, http, keyring, user_id, "anthropic", request).await
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
