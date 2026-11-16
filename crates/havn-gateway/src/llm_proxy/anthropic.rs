//! Anthropic Messages API provider — the canonical shape, no translation.

use async_trait::async_trait;

use super::provider::{
    LlmProvider, ProviderError, Sse, TextEvent, TextSink, is_event_stream, pass_text, status_error,
};
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
        Err(status_error(status.as_u16(), body))
    }

    async fn stream(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
        text: &TextSink,
    ) -> Result<AnthropicResponse, ProviderError> {
        let mut body = serde_json::to_value(request)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;
        body["stream"] = serde_json::Value::Bool(true);
        let resp = http
            .post(format!("{base_url}/v1/messages"))
            .header("x-api-key", api_key)
            .header("anthropic-version", VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(status_error(status.as_u16(), body));
        }
        if !is_event_stream(&resp) {
            let response = resp
                .json::<AnthropicResponse>()
                .await
                .map_err(|e| ProviderError::Translation(e.to_string()))?;
            pass_text(&response, text);
            return Ok(response);
        }
        let mut events = Sse::new(resp);
        let mut message = StreamedMessage::default();
        while let Some(data) = events.next().await? {
            let event: serde_json::Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Translation(format!("stream event decode: {e}")))?;
            if event.get("type").and_then(serde_json::Value::as_str) == Some("error") {
                return Err(ProviderError::Upstream {
                    status: 529,
                    body: event
                        .get("error")
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                });
            }
            if let Some(piece) = message.add(&event) {
                text(TextEvent::Text(&piece));
            }
            if message.stopped {
                break;
            }
        }
        serde_json::from_value(message.whole())
            .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))
    }
}

/// A streamed Messages response, put back together.
#[derive(Default)]
struct StreamedMessage {
    message: serde_json::Value,
    blocks: Vec<serde_json::Value>,
    /// A tool call's input, as the JSON text it arrives in.
    inputs: Vec<String>,
    stopped: bool,
}

impl StreamedMessage {
    /// Add one event; the text it adds, if any.
    fn add(&mut self, event: &serde_json::Value) -> Option<String> {
        use serde_json::Value;
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| *i < 4096);
        match event.get("type").and_then(Value::as_str)? {
            "message_start" => self.message = event.get("message")?.clone(),
            "content_block_start" => {
                let i = index?;
                if self.blocks.len() <= i {
                    self.blocks.resize(i + 1, Value::Null);
                    self.inputs.resize(i + 1, String::new());
                }
                self.blocks[i] = event.get("content_block")?.clone();
            }
            "content_block_delta" => {
                let i = index.filter(|i| *i < self.blocks.len())?;
                let delta = event.get("delta")?;
                let block = &mut self.blocks[i];
                let append = |block: &mut Value, key: &str, piece: &str| {
                    let mut whole = block
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    whole.push_str(piece);
                    block[key] = Value::String(whole);
                };
                match delta.get("type").and_then(Value::as_str)? {
                    "text_delta" => {
                        let piece = delta.get("text").and_then(Value::as_str)?;
                        append(block, "text", piece);
                        return (!piece.is_empty()).then(|| piece.to_string());
                    }
                    "input_json_delta" => {
                        self.inputs[i].push_str(delta.get("partial_json").and_then(Value::as_str)?);
                    }
                    "thinking_delta" => append(
                        block,
                        "thinking",
                        delta.get("thinking").and_then(Value::as_str)?,
                    ),
                    "signature_delta" => append(
                        block,
                        "signature",
                        delta.get("signature").and_then(Value::as_str)?,
                    ),
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = index.filter(|i| *i < self.blocks.len())?;
                if !self.inputs[i].is_empty() {
                    self.blocks[i]["input"] = serde_json::from_str(&self.inputs[i])
                        .unwrap_or_else(|_| serde_json::json!({}));
                }
            }
            "message_delta" => {
                if let Some(delta) = event.get("delta") {
                    for key in ["stop_reason", "stop_sequence"] {
                        if let Some(v) = delta.get(key) {
                            self.message[key] = v.clone();
                        }
                    }
                }
                if let Some(usage) = event.get("usage").and_then(Value::as_object) {
                    for (k, v) in usage {
                        if !v.is_null() {
                            self.message["usage"][k] = v.clone();
                        }
                    }
                }
            }
            "message_stop" => self.stopped = true,
            _ => {}
        }
        None
    }

    /// The whole response, as the API answers a call that does not stream.
    fn whole(&self) -> serde_json::Value {
        let mut message = self.message.clone();
        message["content"] = serde_json::Value::Array(
            self.blocks
                .iter()
                .filter(|b| !b.is_null())
                .cloned()
                .collect(),
        );
        message
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

    #[tokio::test]
    async fn a_server_that_does_not_stream_still_answers_a_streaming_call() {
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "id": "msg_1",
                    "model": "claude-test",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "whole" }],
                    "stop_reason": "end_turn",
                    "usage": { "input_tokens": 1, "output_tokens": 1 }
                }))
            }),
        );
        let (base, server) = serve(app).await;
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let pieces = Arc::clone(&seen);
        let sink = move |event: TextEvent<'_>| {
            if let TextEvent::Text(t) = event {
                pieces.lock().expect("lock").push_str(t);
            }
        };
        let resp = AnthropicProvider
            .stream(
                &crate::llm_proxy::http_client().expect("client"),
                &base,
                "sk-local",
                &hi(),
                &sink,
            )
            .await
            .expect("the whole reply");
        server.abort();
        assert_eq!(resp.content[0]["text"], "whole");
        assert_eq!(*seen.lock().expect("lock"), "whole");
    }

    #[test]
    fn a_streamed_message_is_put_back_together() {
        let events = [
            serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-x", "content": [], "stop_reason": null, "usage": {"input_tokens": 20, "output_tokens": 1}}}),
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Looking "}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "it up."}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "web_search", "input": {}}}),
            serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"ru"}}),
            serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "st\"}"}}),
            serde_json::json!({"type": "content_block_stop", "index": 1}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 30}}),
            serde_json::json!({"type": "message_stop"}),
        ];
        let mut message = StreamedMessage::default();
        let pieces: Vec<String> = events.iter().filter_map(|e| message.add(e)).collect();
        assert_eq!(pieces, ["Looking ", "it up."]);
        assert!(message.stopped);
        let response: AnthropicResponse =
            serde_json::from_value(message.whole()).expect("a whole response");
        assert_eq!(response.content[0]["text"], "Looking it up.");
        assert_eq!(response.content[1]["input"]["query"], "rust");
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(
            (response.usage.input_tokens, response.usage.output_tokens),
            (20, 30)
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
