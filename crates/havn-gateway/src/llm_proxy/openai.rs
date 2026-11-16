//! OpenAI Chat Completions provider, with OpenRouter as a parameterised
//! variant (same wire shape, different base URL).
//!
//! Translation strategy:
//!
//! * **Inbound** — convert canonical [`AnthropicRequest`] to OpenAI's
//!   `/v1/chat/completions` shape. Differences handled here:
//!   - Anthropic's `system: String` field becomes a `{"role": "system", ...}`
//!     message at the front.
//!   - Content blocks of type `tool_use` (assistant) and `tool_result`
//!     (user) become OpenAI `tool_calls` (assistant) and `role: "tool"`
//!     messages respectively.
//!   - Tool definitions translate from
//!     `{name, description, input_schema}` to
//!     `{type: "function", function: {name, description, parameters}}`.
//!
//! * **Outbound** — convert OpenAI's `choices[0].message` back into an
//!   Anthropic-shaped `AnthropicResponse`. The `tool_calls` array becomes
//!   `tool_use` content blocks; `finish_reason` maps to `stop_reason`.
//!
//! The runtime's `tool_loop.rs` watches `stop_reason == "tool_use"` and
//! parses `content[].type` — keeping the response Anthropic-shaped means
//! the runtime is provider-agnostic.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::provider::{LlmProvider, ProviderError, Sse, TextEvent, TextSink, status_error};
use super::{AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicUsage};

const OPENAI_URL: &str = "https://api.openai.com/v1";
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";

/// OpenAI-compatible Chat Completions provider. The same translator backs
/// OpenAI itself and OpenRouter — the only differences are the base URL and a
/// couple of optional headers that OpenRouter recommends for analytics.
#[derive(Debug)]
pub struct OpenAiProvider {
    /// Used by [`LlmProvider::name`] for diagnostics; dead-code analysis
    /// can't follow the trait dispatch back to this field.
    #[allow(dead_code)]
    name: &'static str,
    base_url: &'static str,
    /// Body field carrying `max_tokens`. OpenAI deprecated `max_tokens`
    /// in favour of `max_completion_tokens`, and its reasoning models
    /// (o-series, gpt-5) reject the old name; OpenRouter takes `max_tokens`.
    max_tokens_field: &'static str,
    /// HTTP-Referer / X-Title — OpenRouter wants these for traffic
    /// attribution. Empty for plain OpenAI.
    extra_headers: &'static [(&'static str, &'static str)],
}

impl OpenAiProvider {
    pub const OPENAI_NAME: &'static str = "openai";
    pub const OPENROUTER_NAME: &'static str = "openrouter";

    pub const OPENAI: Self = Self {
        name: Self::OPENAI_NAME,
        base_url: OPENAI_URL,
        max_tokens_field: "max_completion_tokens",
        extra_headers: &[],
    };

    pub const OPENROUTER: Self = Self {
        name: Self::OPENROUTER_NAME,
        base_url: OPENROUTER_URL,
        max_tokens_field: "max_tokens",
        extra_headers: &[("HTTP-Referer", "https://havn.dev"), ("X-Title", "havn")],
    };
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn default_base_url(&self) -> &'static str {
        self.base_url
    }

    async fn complete(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
    ) -> Result<AnthropicResponse, ProviderError> {
        let body = anthropic_to_openai(request, self.max_tokens_field)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;

        let mut builder = http
            .post(format!("{base_url}/chat/completions"))
            .bearer_auth(api_key)
            .json(&body);
        for (k, v) in self.extra_headers {
            builder = builder.header(*k, *v);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(status_error(status.as_u16(), body_text));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Translation(format!("response decode: {e}")))?;
        openai_to_anthropic(&raw)
            .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))
    }

    async fn stream(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
        text: &TextSink,
    ) -> Result<AnthropicResponse, ProviderError> {
        let mut body = anthropic_to_openai(request, self.max_tokens_field)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;
        body["stream"] = Value::Bool(true);
        body["stream_options"] = json!({ "include_usage": true });

        let mut builder = http
            .post(format!("{base_url}/chat/completions"))
            .bearer_auth(api_key)
            .json(&body);
        for (k, v) in self.extra_headers {
            builder = builder.header(*k, *v);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(status_error(status.as_u16(), body_text));
        }

        let mut events = Sse::new(resp);
        let mut reply = StreamedReply::default();
        while let Some(data) = events.next().await? {
            if data.trim() == "[DONE]" {
                break;
            }
            let chunk: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Translation(format!("stream chunk decode: {e}")))?;
            if let Some(error) = chunk.get("error") {
                return Err(ProviderError::Upstream {
                    status: 502,
                    body: error.to_string(),
                });
            }
            if let Some(piece) = reply.add(&chunk) {
                text(TextEvent::Text(&piece));
            }
        }
        openai_to_anthropic(&reply.whole())
            .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))
    }
}

/// Translate a canonical Anthropic request into an OpenAI Chat Completions
/// body. Tool uses / results inside content blocks are unwrapped into
/// OpenAI's split `tool_calls` (assistant) / `role: "tool"` (response)
/// shape.
fn anthropic_to_openai(req: &AnthropicRequest, max_tokens_field: &str) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);

    if let Some(sys) = req.system.as_ref().filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": sys}));
    }

    for m in &req.messages {
        translate_message_to_openai(m, &mut messages)?;
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
    });
    body[max_tokens_field] = json!(req.max_tokens);
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(tools) = req.tools.as_ref() {
        body["tools"] = translate_tools_to_openai(tools)?;
    }
    Ok(body)
}

fn translate_message_to_openai(m: &AnthropicMessage, out: &mut Vec<Value>) -> Result<(), String> {
    // Plain string content — same shape on both sides.
    if let Some(s) = m.content.as_str() {
        out.push(json!({"role": &m.role, "content": s}));
        return Ok(());
    }

    let blocks = m
        .content
        .as_array()
        .ok_or_else(|| format!("message.content must be string or array, got {}", m.content))?;

    // For assistant turns, tool_use blocks fold into a single message with
    // tool_calls; text blocks concatenate into `content`. For user turns,
    // tool_result blocks each emit a separate `role: "tool"` message after
    // any text content.
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    // Inline images (base64), as `image_url` parts of a user message.
    let mut images: Vec<Value> = Vec::new();

    for block in blocks {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("content block missing 'type': {block}"))?;
        match kind {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(t.to_string());
                }
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("tool_use block missing 'id'")?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("tool_use block missing 'name'")?;
                let input = block.get("input").cloned().unwrap_or(json!({}));
                let arguments =
                    serde_json::to_string(&input).map_err(|e| format!("tool_use.input: {e}"))?;
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }));
            }
            "tool_result" => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .ok_or("tool_result block missing 'tool_use_id'")?;
                // Anthropic allows tool_result.content to be either a
                // string or an array of {type:"text", text: ...} blocks.
                // Flatten to a single string for OpenAI.
                let content = match block.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
            "image"
                if m.role == "user"
                    && block.pointer("/source/type").and_then(Value::as_str) == Some("base64") =>
            {
                let media = block
                    .pointer("/source/media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png");
                let data = block
                    .pointer("/source/data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                images.push(json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{media};base64,{data}")},
                }));
            }
            other => {
                // Unknown block types (image, document, thinking, …) — fold
                // their text representation in if any, otherwise drop with a
                // marker rather than failing the whole call.
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(t.to_string());
                } else {
                    text_parts.push(format!("(unsupported block: {other})"));
                }
            }
        }
    }

    // Emit the primary message for this turn.
    if m.role == "assistant" && !tool_calls.is_empty() {
        let mut msg = json!({"role": "assistant"});
        if text_parts.is_empty() {
            msg["content"] = Value::Null;
        } else {
            msg["content"] = json!(text_parts.join("\n"));
        }
        msg["tool_calls"] = Value::Array(tool_calls);
        out.push(msg);
    } else if !images.is_empty() {
        let mut parts = vec![json!({"type": "text", "text": text_parts.join("\n")})];
        parts.extend(images);
        out.push(json!({"role": &m.role, "content": parts}));
    } else if !text_parts.is_empty() {
        out.push(json!({"role": &m.role, "content": text_parts.join("\n")}));
    }

    // Tool results follow as their own messages so OpenAI can pair them
    // with the prior assistant tool_calls by id.
    for tr in tool_results {
        out.push(tr);
    }

    Ok(())
}

fn translate_tools_to_openai(tools: &Value) -> Result<Value, String> {
    let arr = tools
        .as_array()
        .ok_or_else(|| format!("tools must be an array, got {tools}"))?;
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        let name = t
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tool entry missing 'name'")?;
        let description = t.get("description").and_then(Value::as_str).unwrap_or("");
        let parameters = t.get("input_schema").cloned().unwrap_or(json!({
            "type": "object",
            "properties": {}
        }));
        out.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": parameters,
            }
        }));
    }
    Ok(Value::Array(out))
}

/// Translate a successful OpenAI Chat Completions response back to the
/// canonical Anthropic shape the runtime expects.
/// A streamed chat completion, put back together.
#[derive(Default)]
struct StreamedReply {
    id: Option<String>,
    model: Option<String>,
    text: String,
    /// Tool calls by their index: id, name, arguments.
    tool_calls: std::collections::BTreeMap<u64, (String, String, String)>,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

impl StreamedReply {
    /// Add one chunk; the text it adds, if any.
    fn add(&mut self, chunk: &Value) -> Option<String> {
        let field = |key: &str| chunk.get(key).and_then(Value::as_str).map(str::to_string);
        self.id = self.id.take().or_else(|| field("id"));
        self.model = self.model.take().or_else(|| field("model"));
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage.clone());
        }
        let choice = chunk.get("choices")?.as_array()?.first()?;
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = choice.get("delta")?;
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let entry = self.tool_calls.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                entry.0 = id.to_string();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                entry.1.push_str(name);
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                entry.2.push_str(arguments);
            }
        }
        let piece = delta.get("content").and_then(Value::as_str)?;
        if piece.is_empty() {
            return None;
        }
        self.text.push_str(piece);
        Some(piece.to_string())
    }

    /// The whole response, as the API answers a call that does not stream.
    fn whole(&self) -> Value {
        let tool_calls: Vec<Value> = self
            .tool_calls
            .iter()
            .map(|(index, (id, name, arguments))| {
                let id = if id.is_empty() {
                    format!("call_{index}")
                } else {
                    id.clone()
                };
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                })
            })
            .collect();
        let mut message = json!({
            "role": "assistant",
            "content": if self.text.is_empty() { Value::Null } else { Value::String(self.text.clone()) },
        });
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }
        let mut whole = json!({
            "id": self.id.clone().unwrap_or_default(),
            "model": self.model.clone().unwrap_or_default(),
            "choices": [{ "message": message, "finish_reason": self.finish_reason }],
        });
        if let Some(usage) = &self.usage {
            whole["usage"] = usage.clone();
        }
        whole
    }
}

fn openai_to_anthropic(raw: &Value) -> Result<AnthropicResponse, String> {
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("openai-no-id")
        .to_string();
    let model = raw
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let choice = raw
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or("response.choices[0] missing")?;
    let message = choice
        .get("message")
        .ok_or("response.choices[0].message missing")?;

    let mut content_blocks: Vec<Value> = Vec::new();

    // Text content (may be null when the assistant only made tool calls).
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content_blocks.push(json!({"type": "text", "text": text}));
        }
    }

    // tool_calls → tool_use blocks. Argument string is JSON; parse so
    // downstream sees a structured object as Anthropic does.
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .ok_or("tool_call missing 'id'")?;
            let f = tc.get("function").ok_or("tool_call missing 'function'")?;
            let name = f
                .get("name")
                .and_then(Value::as_str)
                .ok_or("tool_call.function missing 'name'")?;
            let args_str = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            content_blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => Some("end_turn".to_string()),
        Some("length") => Some("max_tokens".to_string()),
        Some("tool_calls") => Some("tool_use".to_string()),
        Some("content_filter") => Some("stop_sequence".to_string()),
        Some(other) => Some(other.to_string()),
        None => None,
    };

    // `completion_tokens` includes reasoning tokens and `prompt_tokens`
    // includes cached ones.
    let usage = raw
        .get("usage")
        .and_then(|u| {
            Some(AnthropicUsage {
                input_tokens: u.get("prompt_tokens")?.as_u64()?,
                output_tokens: u.get("completion_tokens")?.as_u64()?,
                ..AnthropicUsage::default()
            })
        })
        .unwrap_or_else(AnthropicUsage::unreported);

    Ok(AnthropicResponse {
        id,
        model,
        role: "assistant".to_string(),
        content: Value::Array(content_blocks),
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn a_streamed_call_passes_its_text_on_and_returns_the_whole_reply() {
        // CRLF line ends, a comment, an event split across chunks.
        let body = concat!(
            ": OPENROUTER PROCESSING\r\n\r\n",
            "data: {\"id\":\"g\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move |axum::Json(sent): axum::Json<Value>| async move {
                assert_eq!(sent["stream"], true);
                assert_eq!(sent["stream_options"]["include_usage"], true);
                let pieces: Vec<Result<String, std::convert::Infallible>> = body
                    .as_bytes()
                    .chunks(7)
                    .map(|c| Ok(String::from_utf8_lossy(c).into_owned()))
                    .collect();
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(futures_util::stream::iter(pieces)),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let pieces = std::sync::Arc::clone(&seen);
        let sink = move |event: TextEvent<'_>| {
            if let TextEvent::Text(t) = event {
                pieces.lock().unwrap().push(t.to_string());
            }
        };
        let response = OpenAiProvider::OPENROUTER
            .stream(
                &crate::llm_proxy::http_client().expect("client"),
                &base,
                "sk-local",
                &req(
                    vec![AnthropicMessage {
                        role: "user".into(),
                        content: json!("hi"),
                    }],
                    None,
                ),
                &sink,
            )
            .await
            .expect("a streamed reply");
        server.abort();
        assert_eq!(seen.lock().unwrap().concat(), "Hello");
        assert_eq!(response.content[0]["text"], "Hello");
        assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(
            (response.usage.input_tokens, response.usage.output_tokens),
            (3, 2)
        );
    }

    fn req(messages: Vec<AnthropicMessage>, system: Option<&str>) -> AnthropicRequest {
        AnthropicRequest {
            model: "gpt-4o".into(),
            max_tokens: 256,
            messages,
            system: system.map(String::from),
            temperature: None,
            tools: None,
        }
    }

    #[test]
    fn an_inline_image_becomes_an_image_url_part() {
        let r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!([
                    {"type": "text", "text": "what is this?"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ]),
            }],
            None,
        );
        let body = anthropic_to_openai(&r, "max_tokens").expect("translate");
        let parts = body["messages"][0]["content"].as_array().expect("parts");
        assert_eq!(parts[0], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn translates_plain_text_round_trip() {
        let r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!("hello"),
            }],
            Some("you are a calc"),
        );
        let body = anthropic_to_openai(&r, "max_tokens").expect("translate");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "you are a calc");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");
    }

    #[test]
    fn translates_assistant_tool_use_to_openai_tool_calls() {
        let r = req(
            vec![AnthropicMessage {
                role: "assistant".into(),
                content: json!([
                    {"type": "text", "text": "let me check"},
                    {
                        "type": "tool_use",
                        "id": "tu_1",
                        "name": "shell",
                        "input": {"command": "ls"}
                    }
                ]),
            }],
            None,
        );
        let body = anthropic_to_openai(&r, "max_tokens").expect("translate");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert_eq!(m["role"], "assistant");
        assert_eq!(m["content"], "let me check");
        let tcs = m["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "tu_1");
        assert_eq!(tcs[0]["function"]["name"], "shell");
        let args: Value = serde_json::from_str(tcs[0]["function"]["arguments"].as_str().unwrap())
            .expect("args parse");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn translates_tool_result_to_role_tool_message() {
        let r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!([
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu_1",
                        "content": "file1\nfile2"
                    }
                ]),
            }],
            None,
        );
        let body = anthropic_to_openai(&r, "max_tokens").expect("translate");
        let messages = body["messages"].as_array().expect("messages");
        // No primary user message (no text), just the tool message.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "tu_1");
        assert_eq!(messages[0]["content"], "file1\nfile2");
    }

    #[test]
    fn translates_tools_definitions() {
        let mut r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!("hi"),
            }],
            None,
        );
        r.tools = Some(json!([
            {
                "name": "shell",
                "description": "run a command",
                "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}
            }
        ]));
        let body = anthropic_to_openai(&r, "max_tokens").expect("translate");
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "shell");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn parses_openai_text_response() {
        let raw = json!({
            "id": "chatcmpl-x",
            "model": "gpt-4o",
            "choices": [{
                "message": {"role": "assistant", "content": "hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4}
        });
        let canon = openai_to_anthropic(&raw).expect("parse");
        assert_eq!(canon.id, "chatcmpl-x");
        assert_eq!(canon.model, "gpt-4o");
        assert_eq!(canon.stop_reason.as_deref(), Some("end_turn"));
        let blocks = canon.content.as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello!");
        assert_eq!(canon.usage.input_tokens, 12);
        assert_eq!(canon.usage.output_tokens, 4);
        assert!(!canon.usage.unreported);
    }

    #[test]
    fn missing_usage_is_flagged_unreported() {
        let raw = json!({
            "id": "gen-1",
            "model": "vendor/model",
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]
        });
        assert!(openai_to_anthropic(&raw).expect("parse").usage.unreported);
        let mut partial = raw.clone();
        partial["usage"] = json!({"prompt_tokens": 12});
        assert!(
            openai_to_anthropic(&partial)
                .expect("parse")
                .usage
                .unreported
        );
    }

    #[test]
    fn max_tokens_field_follows_the_provider() {
        let r = req(vec![], None);
        let openai =
            anthropic_to_openai(&r, OpenAiProvider::OPENAI.max_tokens_field).expect("body");
        assert_eq!(openai["max_completion_tokens"], 256);
        assert!(openai.get("max_tokens").is_none());
        let openrouter =
            anthropic_to_openai(&r, OpenAiProvider::OPENROUTER.max_tokens_field).expect("body");
        assert_eq!(openrouter["max_tokens"], 256);
    }

    #[test]
    fn a_streamed_reply_is_put_back_together() {
        let chunks = [
            json!({"id": "gen-1", "model": "deepseek/x", "choices": [{"delta": {"role": "assistant", "content": ""}}]}),
            json!({"choices": [{"delta": {"content": "Let me "}}]}),
            json!({"choices": [{"delta": {"content": "check."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "web_search", "arguments": "{\"query\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"rust\"}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 7}}),
        ];
        let mut reply = StreamedReply::default();
        let pieces: Vec<String> = chunks.iter().filter_map(|c| reply.add(c)).collect();
        assert_eq!(pieces, ["Let me ", "check."]);
        let response = openai_to_anthropic(&reply.whole()).expect("translate");
        assert_eq!(response.id, "gen-1");
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(response.content[0]["text"], "Let me check.");
        assert_eq!(response.content[1]["name"], "web_search");
        assert_eq!(response.content[1]["input"]["query"], "rust");
        assert_eq!(
            (response.usage.input_tokens, response.usage.output_tokens),
            (12, 7)
        );
        assert!(!response.usage.unreported);
    }

    #[test]
    fn parses_openai_tool_call_response_to_anthropic_tool_use() {
        let raw = json!({
            "id": "chatcmpl-x",
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_42",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"command\":\"pwd\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 30, "completion_tokens": 10}
        });
        let canon = openai_to_anthropic(&raw).expect("parse");
        assert_eq!(canon.stop_reason.as_deref(), Some("tool_use"));
        let blocks = canon.content.as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_42");
        assert_eq!(blocks[0]["name"], "shell");
        assert_eq!(blocks[0]["input"]["command"], "pwd");
    }
}
