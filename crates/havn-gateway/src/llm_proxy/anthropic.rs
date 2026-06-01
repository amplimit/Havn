//! Anthropic Messages API provider — the canonical shape, no translation.

use async_trait::async_trait;

use super::provider::{LlmProvider, ProviderError};
use super::{AnthropicRequest, AnthropicResponse};

const URL: &str = "https://api.anthropic.com/v1/messages";
const VERSION: &str = "2023-06-01";

#[derive(Debug, Default)]
pub struct AnthropicProvider;

impl AnthropicProvider {
    pub const NAME: &'static str = "anthropic";
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    async fn complete(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        request: &AnthropicRequest,
    ) -> Result<AnthropicResponse, ProviderError> {
        let resp = http
            .post(URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", VERSION)
            .json(request)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<AnthropicResponse>()
                .await
                .map_err(|e| ProviderError::Translation(e.to_string()));
        }

        let body = resp.text().await.unwrap_or_default();
        Err(match status.as_u16() {
            401 => ProviderError::Unauthorized { body },
            402 | 429 => ProviderError::QuotaOrRateLimit {
                status: status.as_u16(),
                body,
            },
            500..=599 => ProviderError::Upstream {
                status: status.as_u16(),
                body,
            },
            other => ProviderError::BadRequest {
                status: other,
                body,
            },
        })
    }
}
