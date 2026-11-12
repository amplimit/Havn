//! Anthropic Messages API provider — the canonical shape, no translation.

use async_trait::async_trait;

use super::provider::{LlmProvider, ProviderError};
use super::{AnthropicRequest, AnthropicResponse};

const BASE_URL: &str = "https://api.anthropic.com";
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

    fn default_base_url(&self) -> &'static str {
        BASE_URL
    }

    async fn complete(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
    ) -> Result<AnthropicResponse, ProviderError> {
        let resp = http
            .post(format!("{base_url}/v1/messages"))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::llm_proxy::AnthropicMessage;
    use axum::http::{HeaderMap, StatusCode, header};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn calls_the_messages_endpoint_under_the_given_base_url() {
        async fn messages(headers: HeaderMap) -> axum::Json<serde_json::Value> {
            let key = headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({
                "id": "msg_1",
                "model": "claude-test",
                "role": "assistant",
                "content": [{ "type": "text", "text": key }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            }))
        }
        let app = axum::Router::new().route("/prefix/v1/messages", axum::routing::post(messages));
        let (base, server) = serve(app).await;

        let resp = AnthropicProvider
            .complete(
                &crate::llm_proxy::http_client().expect("client"),
                &format!("{base}/prefix"),
                "sk-local",
                &hi(),
            )
            .await
            .expect("reply from the configured base URL");
        server.abort();

        assert_eq!(resp.content[0]["text"], "sk-local");
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let followed = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&followed);
        let app = axum::Router::new()
            .route(
                "/v1/messages",
                axum::routing::post(|| async {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, "/elsewhere/v1/messages")],
                    )
                }),
            )
            .route(
                "/elsewhere/v1/messages",
                axum::routing::post(move || {
                    seen.store(true, Ordering::SeqCst);
                    async { StatusCode::OK }
                }),
            );
        let (base, server) = serve(app).await;

        let err = AnthropicProvider
            .complete(
                &crate::llm_proxy::http_client().expect("client"),
                &base,
                "sk-local",
                &hi(),
            )
            .await
            .expect_err("a redirect is not a reply");
        server.abort();

        assert!(
            matches!(err, ProviderError::BadRequest { status: 307, .. }),
            "{err}"
        );
        assert!(
            !followed.load(Ordering::SeqCst),
            "the request, and its API key, went to the redirect target"
        );
    }

    async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<std::io::Result<()>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}"), server)
    }

    fn hi() -> AnthropicRequest {
        AnthropicRequest {
            model: "claude-test".into(),
            max_tokens: 16,
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: serde_json::json!("hi"),
            }],
            system: None,
            temperature: None,
            tools: None,
        }
    }
}
