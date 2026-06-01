//! `LlmProvider` trait and per-status error mapping (spec §7.4 / §13 Phase 3).
//!
//! The gateway's canonical request/response shape is **Anthropic Messages**
//! (see [`super::AnthropicRequest`] / [`super::AnthropicResponse`]). The
//! runtime's `tool_loop.rs` parses Anthropic-shaped responses; keeping the
//! gateway's outbound contract Anthropic-shaped means non-Anthropic providers
//! translate **inside** the provider impl and the runtime stays untouched.
//!
//! Each provider implementation:
//! 1. Receives a canonical [`super::AnthropicRequest`].
//! 2. Translates to its native shape (no-op for Anthropic itself).
//! 3. Calls upstream.
//! 4. Translates the response back to canonical [`super::AnthropicResponse`].
//! 5. Maps HTTP status onto [`ProviderError`] so the orchestrator's fallback
//!    loop can decide disable / try-next / fatal exactly the same way it
//!    always did for Anthropic.
//!
//! Provider selection is data-driven by the credential's `provider` string
//! ([`super::AnthropicProvider::NAME`] etc). Model-name inference exists for
//! callers that don't carry an explicit hint — see [`provider_for_model`].

use async_trait::async_trait;

use super::{AnthropicRequest, AnthropicResponse};

/// Outcome of one attempt against one provider+credential pair. Mirrors the
/// HTTP status taxonomy the resolver acts on. Translation failures (provider
/// returned a body we couldn't map to canonical shape) collapse into
/// [`ProviderError::Translation`], which is fatal — fallback won't fix a
/// shape bug.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProviderError {
    /// HTTP 401 — key revoked. Disable the credential and try next.
    Unauthorized { body: String },
    /// HTTP 402 / 429 — quota exhausted or rate-limited. Try next; do NOT
    /// disable (the key may be fine again tomorrow / next minute).
    QuotaOrRateLimit { status: u16, body: String },
    /// HTTP 5xx — upstream is broken. Try next.
    Upstream { status: u16, body: String },
    /// HTTP 4xx other than 401/402/429 — caller's request is malformed.
    /// Fatal; do not iterate.
    BadRequest { status: u16, body: String },
    /// Network / TLS / DNS failure. Try next (the next credential may target
    /// a different base_url and succeed).
    Transport(String),
    /// Provider returned a body we couldn't translate into canonical shape.
    /// Fatal — iterating won't fix a shape bug.
    Translation(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized { body } => write!(f, "401 unauthorized: {body}"),
            Self::QuotaOrRateLimit { status, body } => write!(f, "{status}: {body}"),
            Self::Upstream { status, body } => write!(f, "upstream {status}: {body}"),
            Self::BadRequest { status, body } => write!(f, "{status}: {body}"),
            Self::Transport(s) => write!(f, "transport: {s}"),
            Self::Translation(s) => write!(f, "translation: {s}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// One LLM provider. Implementors are stateless and `Send + Sync` — the
/// orchestrator holds a `&dyn LlmProvider` per request.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable identifier matching the `Credential.provider` column. Carried
    /// for diagnostics and parity with the resolver's provider string.
    #[allow(
        dead_code,
        reason = "trait contract; consumed by per-provider tests + future logging"
    )]
    fn name(&self) -> &'static str;

    /// Make one call against `api_key` for the given canonical request.
    /// Translation in/out happens inside this call.
    async fn complete(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        request: &AnthropicRequest,
    ) -> Result<AnthropicResponse, ProviderError>;
}

/// Best-effort provider inference from the model string when the caller
/// didn't pass an explicit provider hint. Used as a fallback in
/// [`super::complete`].
///
/// Returns the **bare** provider name (`"anthropic"`, `"openai"`, …)
/// — the LLM proxy's internal dispatcher (which picks the upstream
/// HTTP client to call) keys off this. v0.2 credential rows live
/// under namespaced provider strings (`llm:anthropic`); the
/// credential resolver does the namespacing transparently
/// (`credential_resolver::list_active` tries both forms).
///
/// The mapping is intentionally conservative — when in doubt we
/// return `"anthropic"` rather than guessing a new provider, so
/// unrecognised models keep the v0.6 behaviour.
#[must_use]
pub fn provider_for_model(model: &str) -> &'static str {
    if model.starts_with("claude-") {
        "anthropic"
    } else if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("chatgpt-")
    {
        "openai"
    } else if model.starts_with("gemini-") {
        "gemini"
    } else if model.contains('/') {
        // Vendor-prefixed model strings ("anthropic/claude-3.5-sonnet",
        // "meta-llama/llama-3.1-70b") are OpenRouter's convention.
        "openrouter"
    } else {
        "anthropic"
    }
}

/// Map [`ProviderError`] onto the orchestrator's [`super::Attempt`] verdict.
/// Lives here because the rules are part of the provider contract — every
/// provider's HTTP semantics fold to the same fallback decisions.
pub(super) fn classify(err: ProviderError) -> super::Attempt {
    match err {
        ProviderError::Unauthorized { body } => super::Attempt::Disable {
            reason: format!("401 unauthorized: {body}"),
        },
        ProviderError::QuotaOrRateLimit { status, body } => super::Attempt::TryNext {
            reason: format!("{status}: {body}"),
        },
        ProviderError::Upstream { status, body } => super::Attempt::TryNext {
            reason: format!("upstream {status}: {body}"),
        },
        ProviderError::Transport(s) => super::Attempt::TryNext {
            reason: format!("transport: {s}"),
        },
        ProviderError::BadRequest { status, body } => {
            super::Attempt::Fatal(super::ProxyError::Upstream {
                status,
                message: body,
            })
        }
        ProviderError::Translation(s) => {
            super::Attempt::Fatal(super::ProxyError::InvalidResponse(s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_for_model_routes_known_prefixes() {
        assert_eq!(provider_for_model("claude-opus-4-7"), "anthropic");
        assert_eq!(provider_for_model("claude-sonnet-4-6"), "anthropic");
        assert_eq!(provider_for_model("gpt-4o"), "openai");
        assert_eq!(provider_for_model("gpt-5"), "openai");
        assert_eq!(provider_for_model("o1-preview"), "openai");
        assert_eq!(provider_for_model("o3-mini"), "openai");
        assert_eq!(provider_for_model("chatgpt-4o-latest"), "openai");
        assert_eq!(provider_for_model("gemini-2.5-pro"), "gemini");
        assert_eq!(provider_for_model("gemini-1.5-flash"), "gemini");
        assert_eq!(
            provider_for_model("anthropic/claude-3.5-sonnet"),
            "openrouter"
        );
        assert_eq!(provider_for_model("meta-llama/llama-3.1-70b"), "openrouter");
    }

    #[test]
    fn provider_for_model_falls_back_to_anthropic_on_unknown() {
        // Unknown bare model string with no slash → anthropic (preserves
        // v0.6 default behaviour for callers passing odd model names).
        assert_eq!(provider_for_model("mystery-model-9000"), "anthropic");
        assert_eq!(provider_for_model(""), "anthropic");
    }
}
