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
//! ([`super::anthropic::AnthropicProvider::NAME`] etc), named by the
//! `provider:` half of the model id in `[[llm.models]]`.

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
    /// disable (the key may be fine again tomorrow / next minute). A 429 is
    /// tried again first, after `retry_after` when the provider said
    /// (issue #216).
    QuotaOrRateLimit {
        status: u16,
        body: String,
        retry_after: Option<std::time::Duration>,
    },
    /// HTTP 5xx — upstream is broken. Tried again, then the next.
    Upstream {
        status: u16,
        body: String,
        retry_after: Option<std::time::Duration>,
    },
    /// HTTP 4xx other than 401/402/429 — caller's request is malformed.
    /// Fatal; do not iterate.
    BadRequest { status: u16, body: String },
    /// Network / TLS / DNS failure. Try next (the next credential may be
    /// served by a healthier upstream path).
    Transport(String),
    /// Provider returned a body we couldn't translate into canonical shape.
    /// Fatal — iterating won't fix a shape bug.
    Translation(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized { body } => write!(f, "401 unauthorized: {body}"),
            Self::QuotaOrRateLimit { status, body, .. } => write!(f, "{status}: {body}"),
            Self::Upstream { status, body, .. } => write!(f, "upstream {status}: {body}"),
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

    /// The provider's public API base URL, used unless `[llm.base_url]`
    /// names another one for this provider.
    fn default_base_url(&self) -> &'static str;

    /// Make one call against `api_key` at `base_url` (no trailing slash) for
    /// the given canonical request. Translation in/out happens inside this
    /// call.
    async fn complete(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
    ) -> Result<AnthropicResponse, ProviderError>;

    /// Like [`Self::complete`], with the reply's text passed to `text` as
    /// it arrives (issue #162). A provider that cannot stream passes it
    /// whole once the call is done.
    async fn stream(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
        text: &TextSink<'_>,
    ) -> Result<AnthropicResponse, ProviderError> {
        let response = self.complete(http, base_url, api_key, request).await?;
        pass_text(&response, text);
        Ok(response)
    }
}

/// Pass the text of a whole `response` to `text` at once.
pub fn pass_text(response: &AnthropicResponse, text: &TextSink<'_>) {
    for block in response.content.as_array().into_iter().flatten() {
        if block.get("type").and_then(serde_json::Value::as_str) == Some("text")
            && let Some(t) = block.get("text").and_then(serde_json::Value::as_str)
        {
            text(TextEvent::Text(t));
        }
    }
}

/// Whether `response` is an event stream; a server that does not stream
/// answers a streaming request with the whole response instead.
#[must_use]
pub fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
}

/// What a streamed call passes on while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEvent<'a> {
    /// The next piece of the reply's text.
    Text(&'a str),
    /// The next piece of the model's reasoning before it (issue #178):
    /// shown in the web chat, never part of the reply.
    Reasoning(&'a str),
    /// The next piece of a tool call's input: not shown, only counted
    /// for a call cut short (issue #305).
    ToolInput(&'a str),
    /// The call starts over on another credential: what it passed so far
    /// is not the reply.
    Restart,
}

/// Where a streamed call's text goes.
pub type TextSink<'a> = dyn Fn(TextEvent<'_>) + Send + Sync + 'a;

/// The error an unsuccessful HTTP status stands for.
#[must_use]
pub fn status_error(
    status: u16,
    body: String,
    retry_after: Option<std::time::Duration>,
) -> ProviderError {
    match status {
        401 => ProviderError::Unauthorized { body },
        402 | 429 => ProviderError::QuotaOrRateLimit {
            status,
            body,
            retry_after,
        },
        500..=599 => ProviderError::Upstream {
            status,
            body,
            retry_after,
        },
        other => ProviderError::BadRequest {
            status: other,
            body,
        },
    }
}

/// How long a response's `Retry-After` header asks to wait, in seconds
/// (the form providers use; an HTTP date is not read). A value no wait can
/// hold (negative, not a number, too large) is none.
#[must_use]
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .and_then(|secs| std::time::Duration::try_from_secs_f64(secs).ok())
}

/// A stream that stopped before the provider said the reply was done:
/// a connection cut, retried on the next credential.
#[must_use]
pub fn cut_short() -> ProviderError {
    ProviderError::Transport("the response stream ended before the reply did".into())
}

/// Bytes a streamed response may hold, all told.
const MAX_STREAM_BYTES: usize = 32 * 1024 * 1024;

/// The events of a server-sent event stream: each one's `data` lines,
/// joined. Comments and other fields are skipped.
pub struct Sse {
    response: reqwest::Response,
    buffer: Vec<u8>,
    read: usize,
    done: bool,
}

impl Sse {
    #[must_use]
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            read: 0,
            done: false,
        }
    }

    /// The next event's data, `None` at the end of the stream.
    pub async fn next(&mut self) -> Result<Option<String>, ProviderError> {
        loop {
            if let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
                let event: Vec<u8> = self.buffer.drain(..end + 2).collect();
                if let Some(data) = event_data(&event) {
                    return Ok(Some(data));
                }
                continue;
            }
            if self.done {
                let rest = std::mem::take(&mut self.buffer);
                return Ok(event_data(&rest));
            }
            match self
                .response
                .chunk()
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))?
            {
                Some(bytes) => {
                    self.read += bytes.len();
                    if self.read > MAX_STREAM_BYTES {
                        return Err(ProviderError::Translation(format!(
                            "the streamed response is larger than {} MB",
                            MAX_STREAM_BYTES / (1024 * 1024)
                        )));
                    }
                    // Lines may end in CRLF; JSON data never holds a raw CR.
                    self.buffer.extend(bytes.iter().filter(|b| **b != b'\r'));
                }
                None => self.done = true,
            }
        }
    }
}

/// The `data` of one event, `None` when it has none.
fn event_data(event: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(event);
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Maximum output tokens of known models, keyed by model-name prefix in
/// the normalized form [`model_output_limit`] matches against. The longest
/// matching prefix wins; models not listed are not clamped.
const OUTPUT_LIMITS: &[(&str, u32)] = &[
    // Anthropic
    ("claude-3-haiku", 4_096),
    ("claude-3-sonnet", 4_096),
    ("claude-3-opus", 4_096),
    ("claude-3-5-haiku", 8_192),
    ("claude-3-5-sonnet", 8_192),
    ("claude-3-7-sonnet", 64_000),
    ("claude-sonnet-4-0", 64_000),
    ("claude-sonnet-4-20250514", 64_000),
    ("claude-opus-4-0", 32_000),
    ("claude-opus-4-20250514", 32_000),
    ("claude-opus-4-1", 32_000),
    ("claude-sonnet-4-5", 64_000),
    ("claude-haiku-4-5", 64_000),
    ("claude-opus-4-5", 64_000),
    ("claude-opus-4-6", 128_000),
    ("claude-sonnet-4-6", 128_000),
    ("claude-opus-4-7", 128_000),
    ("claude-opus-4-8", 128_000),
    ("claude-opus-5", 128_000),
    ("claude-sonnet-5", 128_000),
    ("claude-haiku-5", 128_000),
    ("claude-fable-5", 128_000),
    ("claude-mythos-5", 128_000),
    // OpenAI
    ("gpt-3-5-turbo", 4_096),
    ("gpt-4-turbo", 4_096),
    ("gpt-4o", 16_384),
    ("gpt-4o-2024-05-13", 4_096),
    ("gpt-4-1", 32_768),
    ("gpt-5", 128_000),
    ("gpt-5-chat", 16_384),
    ("o1", 100_000),
    ("o1-mini", 65_536),
    ("o1-preview", 32_768),
    ("o3", 100_000),
    ("o3-mini", 100_000),
    ("o4-mini", 100_000),
    // Google
    ("gemini-1-5-flash", 8_192),
    ("gemini-1-5-pro", 8_192),
    ("gemini-2-0-flash", 8_192),
    ("gemini-2-5-flash", 65_536),
    ("gemini-2-5-flash-image", 32_768),
    ("gemini-2-5-pro", 65_536),
];

/// Known output-token limit of `model`, or `None` for an unknown model.
/// An OpenRouter-style vendor prefix (`anthropic/`) is dropped and dots
/// become dashes (`claude-3.5-sonnet` is `claude-3-5-sonnet`). A prefix
/// matches only up to a `-`, `:` or `@` boundary, so `o1` does not match
/// `o10`.
#[must_use]
pub fn model_output_limit(model: &str) -> Option<u32> {
    let bare = model.rsplit('/').next().unwrap_or(model);
    let name = bare.to_ascii_lowercase().replace('.', "-");
    OUTPUT_LIMITS
        .iter()
        .filter(|(prefix, _)| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', ':', '@']))
        })
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|&(_, limit)| limit)
}

#[must_use]
pub fn clamp_max_tokens(model: &str, limit: Option<u32>, requested: u32, ceiling: u32) -> u32 {
    limit
        .or_else(|| model_output_limit(model))
        .map_or(requested, |limit| requested.min(limit))
        .min(ceiling)
}

/// Map [`ProviderError`] onto the orchestrator's [`super::Attempt`] verdict.
/// Lives here because the rules are part of the provider contract — every
/// provider's HTTP semantics fold to the same fallback decisions.
pub(super) fn classify(err: ProviderError) -> super::Attempt {
    match err {
        ProviderError::Unauthorized { body } => super::Attempt::Disable {
            reason: format!("401 unauthorized: {body}"),
        },
        // A rate limit or an overload passes: tried again (issue #216). A
        // quota (402) does not.
        ProviderError::QuotaOrRateLimit {
            status,
            body,
            retry_after,
        } => super::Attempt::TryNext {
            reason: format!("{status}: {body}"),
            again: (status == 429).then(|| retry_after.unwrap_or_default()),
            transient: status == 429,
        },
        ProviderError::Upstream {
            status,
            body,
            retry_after,
        } => super::Attempt::TryNext {
            reason: format!("upstream {status}: {body}"),
            again: Some(retry_after.unwrap_or_default()),
            transient: true,
        },
        ProviderError::Transport(s) => super::Attempt::TryNext {
            reason: format!("transport: {s}"),
            again: Some(std::time::Duration::ZERO),
            transient: true,
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
    fn a_retry_after_no_wait_can_hold_is_none() {
        let header = |v: &str| -> reqwest::header::HeaderMap {
            [(reqwest::header::RETRY_AFTER, v.parse().expect("value"))]
                .into_iter()
                .collect()
        };
        assert_eq!(
            retry_after(&header("2.5")),
            Some(std::time::Duration::from_millis(2500))
        );
        for odd in [
            "1e20",
            "99999999999999999999",
            "-1",
            "NaN",
            "inf",
            "Wed, 21 Oct 2026 07:28:00 GMT",
        ] {
            assert_eq!(retry_after(&header(odd)), None, "{odd}");
        }
    }

    #[test]
    fn output_limits_match_by_longest_prefix() {
        assert_eq!(model_output_limit("claude-3-haiku-20240307"), Some(4_096));
        assert_eq!(
            model_output_limit("claude-3-5-sonnet-20241022"),
            Some(8_192)
        );
        assert_eq!(
            model_output_limit("anthropic/claude-3.5-sonnet"),
            Some(8_192)
        );
        assert_eq!(model_output_limit("claude-opus-4-20250514"), Some(32_000));
        assert_eq!(model_output_limit("claude-opus-4-5-20251101"), Some(64_000));
        assert_eq!(model_output_limit("claude-opus-5-5"), Some(128_000));
        assert_eq!(model_output_limit("gpt-4o-mini"), Some(16_384));
        assert_eq!(model_output_limit("gpt-4o-2024-05-13"), Some(4_096));
        assert_eq!(model_output_limit("openai/gpt-4.1-mini"), Some(32_768));
        assert_eq!(model_output_limit("o1-mini"), Some(65_536));
        assert_eq!(model_output_limit("gemini-2.5-pro"), Some(65_536));
        assert_eq!(
            model_output_limit("google/gemini-2.0-flash-001"),
            Some(8_192)
        );
    }

    #[test]
    fn unknown_models_are_not_limited() {
        assert_eq!(
            model_output_limit("deepseek/deepseek-v4.1-flash-1017"),
            None
        );
        assert_eq!(model_output_limit("o10"), None);
        assert_eq!(model_output_limit("claude-opus-4-9"), None);
        assert_eq!(model_output_limit("mystery-model-9000"), None);
    }

    #[test]
    fn max_tokens_is_clamped_to_model_limit_and_ceiling() {
        const NONE: u32 = u32::MAX;
        assert_eq!(
            clamp_max_tokens("claude-3-haiku-20240307", None, 8_192, NONE),
            4_096
        );
        assert_eq!(
            clamp_max_tokens("claude-opus-5-5", None, 8_192, NONE),
            8_192
        );
        assert_eq!(
            clamp_max_tokens("claude-opus-5-5", None, 200_000, NONE),
            128_000
        );
        assert_eq!(
            clamp_max_tokens("claude-opus-5-5", None, 8_192, 2_000),
            2_000
        );
        assert_eq!(
            clamp_max_tokens("claude-3-haiku", None, 8_192, 6_000),
            4_096
        );
        assert_eq!(
            clamp_max_tokens("mystery-model", None, 1_000_000, NONE),
            1_000_000
        );
        assert_eq!(
            clamp_max_tokens("mystery-model", None, 1_000_000, 128_000),
            128_000
        );
        assert_eq!(model_output_limit("gemini-2.5-flash-image"), Some(32_768));
        assert_eq!(
            clamp_max_tokens("claude-opus-5-5", Some(16_000), 200_000, NONE),
            16_000
        );
        assert_eq!(
            clamp_max_tokens("mystery-model", Some(16_000), 200_000, 8_000),
            8_000
        );
    }
}
