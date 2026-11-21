//! Anthropic Messages API provider — the canonical shape, no translation.

use async_trait::async_trait;

use super::provider::{
    LlmProvider, ProviderError, Sse, TextEvent, TextSink, cut_short, is_event_stream, pass_text,
    retry_after, status_error,
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
        let body = request_body(request)?;
        let resp = http
            .post(format!("{base_url}/v1/messages"))
            .header("x-api-key", api_key)
            .header("anthropic-version", VERSION)
            .json(&body)
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

        let wait = retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        Err(status_error(status.as_u16(), body, wait))
    }

    async fn stream(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
        text: &TextSink,
    ) -> Result<AnthropicResponse, ProviderError> {
        let mut body = request_body(request)?;
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
            let wait = retry_after(resp.headers());
            let body = resp.text().await.unwrap_or_default();
            return Err(status_error(status.as_u16(), body, wait));
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
                    retry_after: None,
                    status: 529,
                    body: event
                        .get("error")
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                });
            }
            if event
                .pointer("/delta/type")
                .and_then(serde_json::Value::as_str)
                == Some("thinking_delta")
                && let Some(piece) = event
                    .pointer("/delta/thinking")
                    .and_then(serde_json::Value::as_str)
                && !piece.is_empty()
            {
                text(TextEvent::Reasoning(piece));
            }
            if let Some(piece) = message.add(&event) {
                text(TextEvent::Text(&piece));
            }
            if message.stopped {
                break;
            }
        }
        if !message.stopped {
            return Err(cut_short());
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
                    "citations_delta" => {
                        let citation = delta.get("citation")?.clone();
                        match block.get_mut("citations").and_then(Value::as_array_mut) {
                            Some(citations) => citations.push(citation),
                            None => block["citations"] = Value::Array(vec![citation]),
                        }
                    }
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

/// The request as the Messages API takes it, asking the model to think
/// (issue #190): adaptive thinking, at an effort for a level, as current
/// models take it, or a fixed budget, as Haiku 4.5 and older models take
/// it, leaving room for the reply. With thinking on, no temperature;
/// thinking is shown summarized, since newer models leave it out unless
/// asked. Off sends no `thinking`: several current models refuse
/// `disabled`, and older ones do not think without being asked.
fn request_body(request: &AnthropicRequest) -> Result<serde_json::Value, ProviderError> {
    let mut body = serde_json::to_value(request)
        .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;
    let adaptive = || serde_json::json!({"type": "adaptive", "display": "summarized"});
    let thinking = match &request.reasoning {
        None | Some(super::Reasoning::Off) => None,
        Some(super::Reasoning::On) => Some(adaptive()),
        Some(super::Reasoning::Effort(level)) => {
            body["output_config"] = serde_json::json!({"effort": level});
            Some(adaptive())
        }
        Some(super::Reasoning::Budget(n)) => super::budget_within(*n, request.max_tokens)
            .map(|budget| serde_json::json!({"type": "enabled", "budget_tokens": budget})),
    };
    if let Some(thinking) = thinking {
        body["thinking"] = thinking;
        if let Some(obj) = body.as_object_mut() {
            obj.remove("temperature");
        }
    }
    mark_cache(&mut body, request);
    Ok(body)
}

/// Mark where Anthropic caches the prompt (issue #220), with three of the
/// four marks a request may carry:
/// - the end of the system part every turn shares (the tools before it
///   are cached with it);
/// - the end of the conversation: the turn's next round and the next turn
///   read what came before from the cache;
/// - the end of the person's message or results before that, where the
///   previous round wrote: a round of many calls can be further back than
///   Anthropic looks for an earlier write.
fn mark_cache(body: &mut serde_json::Value, request: &AnthropicRequest) {
    if !super::marks_cache(request) {
        return;
    }
    if let Some(system) = request.system.as_deref().filter(|s| !s.trim().is_empty()) {
        let (shared, rest) = super::system_parts(system, request.system_stable);
        let mut blocks = vec![serde_json::json!({
            "type": "text", "text": shared, "cache_control": super::cache_mark(),
        })];
        if let Some(rest) = rest {
            blocks.push(serde_json::json!({"type": "text", "text": rest}));
        }
        body["system"] = serde_json::Value::Array(blocks);
    }
    let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let users: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .map(|(i, _)| i)
        .rev()
        .take(2)
        .collect();
    for at in users {
        mark_message(&mut messages[at]);
    }
}

/// Mark the last block of `message` that can carry it (not thinking).
fn mark_message(message: &mut serde_json::Value) {
    if let Some(text) = message.get("content").and_then(serde_json::Value::as_str) {
        if text.trim().is_empty() {
            return;
        }
        message["content"] = serde_json::json!([{"type": "text", "text": text}]);
    }
    let Some(blocks) = message
        .get_mut("content")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    if let Some(block) = blocks.iter_mut().rev().find(|b| {
        !matches!(
            b.get("type").and_then(serde_json::Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    }) && let Some(block) = block.as_object_mut()
    {
        block.insert("cache_control".into(), super::cache_mark());
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

    #[test]
    fn an_image_a_tool_showed_stays_in_its_result() {
        let mut r = hi();
        let shown = serde_json::json!([
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "The image chart.png:"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]},
        ]);
        r.messages = vec![AnthropicMessage {
            role: "user".into(),
            content: shown.clone(),
        }];
        let body = request_body(&r).expect("body");
        let image = &body["messages"][0]["content"][0]["content"][1];
        assert_eq!(image["type"], "image");
        assert_eq!(image["source"]["data"], "AAAA");
    }

    #[test]
    fn the_shared_system_part_and_the_conversation_s_end_are_marked_for_the_cache() {
        let mut r = hi();
        r.system = Some("base prompt\n\n---\n\nthe time now".into());
        r.system_stable = "base prompt".len();
        r.messages = vec![
            AnthropicMessage {
                role: "user".into(),
                content: serde_json::json!("hi"),
            },
            AnthropicMessage {
                role: "assistant".into(),
                content: serde_json::json!([
                    {"type": "tool_use", "id": "t1", "name": "x", "input": {}},
                ]),
            },
            AnthropicMessage {
                role: "user".into(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                ]),
            },
        ];
        let body = request_body(&r).expect("body");
        let system = body["system"].as_array().expect("blocks");
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], "base prompt");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert!(
            system[1].get("cache_control").is_none(),
            "the turn's own part is not"
        );
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(
            messages[2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "where the previous round wrote"
        );
        let marks = body.to_string().matches("cache_control").count();
        assert_eq!(marks, 3, "within Anthropic's four");

        // A one-off call is not marked, nor one that marks its own.
        let mut once = r.clone();
        once.system_stable = 0;
        assert!(
            !request_body(&once)
                .expect("body")
                .to_string()
                .contains("cache_control")
        );
        let mut own = r.clone();
        own.messages[0].content = serde_json::json!([{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]);
        let marks = request_body(&own)
            .expect("body")
            .to_string()
            .matches("cache_control")
            .count();
        assert_eq!(marks, 1, "its own mark only");
    }

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
            reasoning: None,
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: serde_json::json!("hi"),
            }],
            system: None,
            temperature: None,
            tools: None,
            system_stable: 0,
        }
    }

    #[test]
    fn thinking_is_asked_for_as_the_model_takes_it() {
        use super::super::Reasoning;
        let mut r = AnthropicRequest {
            model: "claude-x".into(),
            max_tokens: 16000,
            reasoning: Some(Reasoning::Effort("high".into())),
            messages: Vec::new(),
            system: None,
            temperature: Some(0.2),
            tools: None,
            system_stable: 0,
        };
        let body = request_body(&r).expect("body");
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], serde_json::json!({"effort": "high"}));
        assert!(body.get("temperature").is_none());
        r.reasoning = Some(Reasoning::On);
        let body = request_body(&r).expect("body");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body.get("output_config").is_none());
        // A budget, for older models: the reply keeps room.
        r.reasoning = Some(Reasoning::Budget(60000));
        assert_eq!(
            request_body(&r).expect("body")["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 11904})
        );
        r.max_tokens = 2048;
        assert_eq!(
            request_body(&r).expect("body")["thinking"]["budget_tokens"],
            1024
        );
        r.max_tokens = 2047;
        assert!(
            request_body(&r).expect("body").get("thinking").is_none(),
            "no room for the smallest budget and a reply"
        );
        r.max_tokens = 16000;
        r.reasoning = Some(Reasoning::Off);
        let body = request_body(&r).expect("body");
        assert!(body.get("thinking").is_none());
        assert_eq!(body["temperature"], 0.2);
        r.reasoning = None;
        assert_eq!(request_body(&r).expect("body")["temperature"], 0.2);
    }
}
