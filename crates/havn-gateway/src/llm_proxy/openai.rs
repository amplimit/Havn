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

use super::provider::{
    LlmProvider, ProviderError, Sse, TextEvent, TextSink, cut_short, is_event_stream, pass_text,
    retry_after, status_error,
};
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

    /// An `[llm.endpoints]` server of the OpenAI API (issue #232), always
    /// at its own base URL: Ollama, llama.cpp, vLLM and LM Studio take
    /// `max_tokens`, and thinking goes as OpenAI's `reasoning_effort`.
    pub const COMPATIBLE: Self = Self {
        name: Self::OPENAI_NAME,
        base_url: OPENAI_URL,
        max_tokens_field: "max_tokens",
        extra_headers: &[],
    };

    /// A POST of `body` to the Chat Completions endpoint under `base_url`,
    /// with the key, unless there is none, and the provider's headers.
    fn post(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        let mut builder = http.post(format!("{base_url}/chat/completions")).json(body);
        if !api_key.is_empty() {
            builder = builder.bearer_auth(api_key);
        }
        for (k, v) in self.extra_headers {
            builder = builder.header(*k, *v);
        }
        builder
    }
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
        let body = anthropic_to_openai(request, self.max_tokens_field, self.name)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;

        let resp = self
            .post(http, base_url, api_key, &body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let wait = retry_after(resp.headers());
            let body_text = resp.text().await.unwrap_or_default();
            return Err(status_error(status.as_u16(), body_text, wait));
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
        text: &TextSink<'_>,
    ) -> Result<AnthropicResponse, ProviderError> {
        let mut body = anthropic_to_openai(request, self.max_tokens_field, self.name)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;
        body["stream"] = Value::Bool(true);
        body["stream_options"] = json!({ "include_usage": true });

        let resp = self
            .post(http, base_url, api_key, &body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let wait = retry_after(resp.headers());
            let body_text = resp.text().await.unwrap_or_default();
            return Err(status_error(status.as_u16(), body_text, wait));
        }
        if !is_event_stream(&resp) {
            let raw: Value = resp
                .json()
                .await
                .map_err(|e| ProviderError::Translation(format!("response decode: {e}")))?;
            let response = openai_to_anthropic(&raw)
                .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))?;
            pass_text(&response, text);
            return Ok(response);
        }

        let mut events = Sse::new(resp);
        let mut reply = StreamedReply::default();
        let mut done = false;
        while let Some(data) = events.next().await? {
            if data.trim() == "[DONE]" {
                done = true;
                break;
            }
            let chunk: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Translation(format!("stream chunk decode: {e}")))?;
            if let Some(error) = chunk.get("error") {
                return Err(ProviderError::Upstream {
                    retry_after: None,
                    status: 502,
                    body: error.to_string(),
                });
            }
            if let Some(piece) = reasoning_piece(&chunk) {
                text(TextEvent::Reasoning(piece));
            }
            if let Some(piece) = reply.add(&chunk) {
                text(TextEvent::Text(&piece));
            }
        }
        if !done && reply.finish_reason.is_none() {
            return Err(cut_short());
        }
        openai_to_anthropic(&reply.whole())
            .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))
    }
}

/// Translate a canonical Anthropic request into an OpenAI Chat Completions
/// body. Tool uses / results inside content blocks are unwrapped into
/// OpenAI's split `tool_calls` (assistant) / `role: "tool"` (response)
/// shape.
fn anthropic_to_openai(
    req: &AnthropicRequest,
    max_tokens_field: &str,
    provider: &str,
) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
    // With thinking on, the turn's reasoning goes back on the assistant
    // message that called a tool, where DeepSeek and OpenRouter want it.
    let reasoning_field = match &req.reasoning {
        Some(r) if *r != super::Reasoning::Off => {
            Some(if provider == OpenAiProvider::OPENROUTER_NAME {
                "reasoning"
            } else {
                "reasoning_content"
            })
        }
        _ => None,
    };

    // OpenRouter caches an Anthropic model's prompt where it is marked
    // (issue #220); other models cache on their own, or not at all.
    let marks = provider == OpenAiProvider::OPENROUTER_NAME
        && req.model.starts_with("anthropic/")
        && super::marks_cache(req);
    if let Some(sys) = req.system.as_ref().filter(|s| !s.is_empty()) {
        if marks {
            let (shared, rest) = super::system_parts(sys, req.system_stable);
            let mut parts = vec![json!({
                "type": "text", "text": shared, "cache_control": super::cache_mark(),
            })];
            if let Some(rest) = rest {
                parts.push(json!({"type": "text", "text": rest}));
            }
            messages.push(json!({"role": "system", "content": parts}));
        } else {
            messages.push(json!({"role": "system", "content": sys}));
        }
    }

    for m in &req.messages {
        translate_message_to_openai(m, &mut messages, reasoning_field)?;
    }
    if marks {
        mark_last(&mut messages);
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
    });
    body[max_tokens_field] = json!(req.max_tokens);
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    // How much to think (issue #190): OpenRouter's normalized `reasoning`,
    // or `reasoning_effort` for an OpenAI-compatible endpoint, which takes
    // no budget.
    match (&req.reasoning, provider) {
        (None, _) => {}
        (Some(r), OpenAiProvider::OPENROUTER_NAME) => {
            let reasoning = match r {
                super::Reasoning::Off => Some(json!({"enabled": false})),
                super::Reasoning::On => Some(json!({"enabled": true})),
                super::Reasoning::Effort(effort) => Some(json!({"effort": effort})),
                // Within max_tokens, as models behind it count it.
                super::Reasoning::Budget(n) => {
                    super::budget_within(*n, req.max_tokens).map(|n| json!({"max_tokens": n}))
                }
            };
            if let Some(reasoning) = reasoning {
                body["reasoning"] = reasoning;
            }
        }
        (Some(r), _) => match r {
            super::Reasoning::Off | super::Reasoning::Budget(_) => {}
            super::Reasoning::On => body["reasoning_effort"] = json!("medium"),
            super::Reasoning::Effort(effort) => body["reasoning_effort"] = json!(effort),
        },
    }
    if let Some(tools) = req.tools.as_ref() {
        body["tools"] = translate_tools_to_openai(tools)?;
    }
    Ok(body)
}

/// An OpenAI-shaped `usage`, as Anthropic counts it: `completion_tokens`
/// includes reasoning tokens, and `prompt_tokens` includes the prompt read
/// from the cache (`prompt_tokens_details.cached_tokens`) and, on
/// OpenRouter, written to it (`cache_write_tokens`), which are counted
/// apart (issue #220).
fn usage_of(u: &Value) -> Option<AnthropicUsage> {
    let prompt = u.get("prompt_tokens")?.as_u64()?;
    let details = u.get("prompt_tokens_details");
    let detail = |key: &str| {
        details
            .and_then(|d| d.get(key))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
    };
    // DeepSeek reports its cache hits on their own.
    let read = detail("cached_tokens").or_else(|| {
        u.get("prompt_cache_hit_tokens")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
    });
    let written = detail("cache_write_tokens");
    Some(AnthropicUsage {
        input_tokens: prompt
            .saturating_sub(read.unwrap_or(0))
            .saturating_sub(written.unwrap_or(0)),
        output_tokens: u.get("completion_tokens")?.as_u64()?,
        cache_read_input_tokens: read,
        cache_creation_input_tokens: written,
        unreported: false,
    })
}

/// Mark the end of the conversation for the cache: the last tool result or
/// user message, whose content becomes parts to carry it. The conversation
/// up to it is read from the cache by the turn's later rounds and the next
/// turn.
fn mark_last(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    if !matches!(
        last.get("role").and_then(Value::as_str),
        Some("user" | "tool")
    ) {
        return;
    }
    if let Some(text) = last.get("content").and_then(Value::as_str) {
        if text.trim().is_empty() {
            return;
        }
        last["content"] =
            json!([{"type": "text", "text": text, "cache_control": super::cache_mark()}]);
        return;
    }
    if let Some(part) = last
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|parts| parts.last_mut())
        && let Some(part) = part.as_object_mut()
    {
        part.insert("cache_control".into(), super::cache_mark());
    }
}

fn translate_message_to_openai(
    m: &AnthropicMessage,
    out: &mut Vec<Value>,
    reasoning_field: Option<&str>,
) -> Result<(), String> {
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
    // tool_result blocks each emit a separate `role: "tool"` message,
    // before any text content: they must follow the tool_calls they answer.
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut thinking = String::new();
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
                // Flatten to a single string for OpenAI. A tool message
                // carries no image (issue #227): one a tool showed goes in
                // the user message that follows the results, named so.
                let content = match block.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => {
                        for part in parts.iter().filter(|p| is_base64_image(p)) {
                            images.push(json!({
                                "type": "text",
                                "text": format!("(The image the tool call {tool_use_id} showed:)"),
                            }));
                            images.push(image_url(part));
                        }
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
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
                images.push(image_url(block));
            }
            // Audio (issue #164, a transcription), as an `input_audio` part.
            "audio"
                if m.role == "user"
                    && block.pointer("/source/type").and_then(Value::as_str) == Some("base64") =>
            {
                let format = match block
                    .pointer("/source/media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("audio/ogg")
                {
                    "audio/mpeg" => "mp3",
                    "audio/mp4" => "m4a",
                    "audio/wav" => "wav",
                    "audio/webm" => "webm",
                    "audio/flac" => "flac",
                    _ => "ogg",
                };
                let data = block
                    .pointer("/source/data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                images.push(json!({
                    "type": "input_audio",
                    "input_audio": {"data": data, "format": format},
                }));
            }
            // A model's reasoning goes back only where it asks for it.
            "thinking" => {
                if reasoning_field.is_some()
                    && let Some(t) = block.get("thinking").and_then(Value::as_str)
                {
                    thinking.push_str(t);
                }
            }
            "redacted_thinking" => {}
            other => {
                // Unknown block types (image, document, …) — fold
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

    // Tool results first, as their own messages, right after the assistant
    // tool_calls they answer (OpenAI pairs them by id, and refuses anything
    // in between); the turn's text, if any, follows them.
    out.append(&mut tool_results);

    // Emit the primary message for this turn.
    if m.role == "assistant" && !tool_calls.is_empty() {
        let mut msg = json!({"role": "assistant"});
        if text_parts.is_empty() {
            msg["content"] = Value::Null;
        } else {
            msg["content"] = json!(text_parts.join("\n"));
        }
        msg["tool_calls"] = Value::Array(tool_calls);
        if let Some(field) = reasoning_field
            && !thinking.is_empty()
        {
            msg[field] = json!(thinking);
        }
        out.push(msg);
    } else if !images.is_empty() {
        // No empty text part: some servers refuse one.
        let mut parts = Vec::new();
        if !text_parts.is_empty() {
            parts.push(json!({"type": "text", "text": text_parts.join("\n")}));
        }
        parts.extend(images);
        out.push(json!({"role": &m.role, "content": parts}));
    } else if !text_parts.is_empty() {
        out.push(json!({"role": &m.role, "content": text_parts.join("\n")}));
    }

    Ok(())
}

/// Whether `block` is an image given as base64.
pub(super) fn is_base64_image(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("image")
        && block.pointer("/source/type").and_then(Value::as_str) == Some("base64")
}

/// Base64 image block `block` as an `image_url` part.
fn image_url(block: &Value) -> Value {
    let media = block
        .pointer("/source/media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    let data = block
        .pointer("/source/data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{media};base64,{data}")},
    })
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
/// The reasoning a streamed chunk adds: DeepSeek's `reasoning_content`, or
/// OpenRouter's `reasoning`.
fn reasoning_piece(chunk: &Value) -> Option<&str> {
    let delta = chunk.pointer("/choices/0/delta")?;
    ["reasoning_content", "reasoning"]
        .iter()
        .find_map(|key| delta.get(*key).and_then(Value::as_str))
        .filter(|piece| !piece.is_empty())
}

/// A streamed chat completion, put back together.
#[derive(Default)]
struct StreamedReply {
    id: Option<String>,
    model: Option<String>,
    text: String,
    /// The model's reasoning before its reply, when it shows it.
    reasoning: String,
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
        if let Some(piece) = reasoning_piece(chunk) {
            self.reasoning.push_str(piece);
        }
        let delta = choice.get("delta")?;
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            // Without an index, a new id starts a call; the rest continue
            // the last one.
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| match self.tool_calls.iter().next_back() {
                    Some((last, (last_id, _, _))) => match id {
                        Some(id) if !last_id.is_empty() && last_id != id => last + 1,
                        _ => *last,
                    },
                    None => 0,
                });
            let entry = self.tool_calls.entry(index).or_default();
            if let Some(id) = id {
                entry.0 = id.to_string();
            }
            // Some servers repeat the name in every fragment.
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str)
                && entry.1.is_empty()
            {
                entry.1 = name.to_string();
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
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(self.reasoning.clone());
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

    // The model's reasoning, as Anthropic's thinking block: the runtime
    // keeps it for the dashboard and never sends it back.
    if let Some(reasoning) = ["reasoning_content", "reasoning"]
        .iter()
        .find_map(|key| message.get(*key).and_then(Value::as_str))
        .filter(|r| !r.trim().is_empty())
    {
        content_blocks.push(json!({"type": "thinking", "thinking": reasoning}));
    }

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

    let usage = raw
        .get("usage")
        .and_then(usage_of)
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
            reasoning: None,
            messages,
            system: system.map(String::from),
            temperature: None,
            tools: None,
            system_stable: 0,
        }
    }

    #[test]
    fn audio_becomes_an_input_audio_part() {
        let r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!([
                    {"type": "text", "text": "transcribe"},
                    {"type": "audio", "source": {"type": "base64", "media_type": "audio/mpeg", "data": "AAAA"}},
                ]),
            }],
            None,
        );
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
        let parts = body["messages"][0]["content"].as_array().expect("parts");
        assert_eq!(parts[1]["type"], "input_audio");
        assert_eq!(parts[1]["input_audio"]["format"], "mp3");
        assert_eq!(parts[1]["input_audio"]["data"], "AAAA");
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
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
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
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
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
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
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
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
        let messages = body["messages"].as_array().expect("messages");
        // No primary user message (no text), just the tool message.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "tu_1");
        assert_eq!(messages[0]["content"], "file1\nfile2");
    }

    #[test]
    fn an_image_a_tool_showed_follows_its_result_as_a_user_message() {
        let r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!([
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu_1",
                        "content": [
                            {"type": "text", "text": "The image chart.png:"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                        ]
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu_2",
                        "content": [
                            {"type": "text", "text": "The image shot.png:"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "BBBB"}}
                        ]
                    }
                ]),
            }],
            None,
        );
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["content"], "The image chart.png:");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[2]["role"], "user");
        let parts = messages[2]["content"].as_array().expect("parts");
        let shape: Vec<String> = parts
            .iter()
            .map(|p| match p["image_url"]["url"].as_str() {
                Some(url) => url.to_string(),
                None => p["text"].as_str().unwrap_or_default().to_string(),
            })
            .collect();
        assert_eq!(shape.len(), 4, "{shape:?}");
        assert!(
            shape[0].contains("tu_1") && shape[2].contains("tu_2"),
            "{shape:?}"
        );
        assert_eq!(shape[1], "data:image/png;base64,AAAA");
        assert_eq!(shape[3], "data:image/png;base64,BBBB");
    }

    #[test]
    fn tool_results_come_right_after_their_calls_and_text_after_them() {
        let r = req(
            vec![
                AnthropicMessage {
                    role: "assistant".into(),
                    content: json!([{"type": "tool_use", "id": "tu_1", "name": "shell", "input": {}}]),
                },
                AnthropicMessage {
                    role: "user".into(),
                    content: json!([
                        {"type": "tool_result", "tool_use_id": "tu_1", "content": "done"},
                        {"type": "text", "text": "now sum up"}
                    ]),
                },
            ],
            None,
        );
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter_map(|m| m["role"].as_str())
            .collect();
        assert_eq!(roles, ["assistant", "tool", "user"]);
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
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
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
    fn openrouter_s_anthropic_models_get_cache_marks_and_others_do_not() {
        let mut r = req(
            vec![
                AnthropicMessage {
                    role: "user".into(),
                    content: json!("first"),
                },
                AnthropicMessage {
                    role: "assistant".into(),
                    content: json!("reply"),
                },
                AnthropicMessage {
                    role: "user".into(),
                    content: json!([{"type": "text", "text": "second"}]),
                },
            ],
            Some("base\n\nturn"),
        );
        r.system_stable = "base".len();
        r.model = "anthropic/claude-sonnet-4.5".into();
        let body =
            anthropic_to_openai(&r, "max_tokens", OpenAiProvider::OPENROUTER_NAME).expect("body");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["content"][0]["text"], "base");
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(messages[0]["content"][1].get("cache_control").is_none());
        let marked = messages.last().expect("last");
        assert_eq!(marked["content"][0]["text"], "second");
        assert_eq!(marked["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            messages[1]["content"], "first",
            "only the end of the conversation"
        );

        // A tool result ends a round: it carries the mark.
        let mut round = r.clone();
        round.messages.push(AnthropicMessage {
            role: "assistant".into(),
            content: json!([{"type": "tool_use", "id": "t1", "name": "x", "input": {}}]),
        });
        round.messages.push(AnthropicMessage {
            role: "user".into(),
            content: json!([{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]),
        });
        let body = anthropic_to_openai(&round, "max_tokens", OpenAiProvider::OPENROUTER_NAME)
            .expect("body");
        let last = body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .expect("last");
        assert_eq!(last["role"], "tool");
        assert_eq!(last["content"][0]["cache_control"]["type"], "ephemeral");

        // A one-off call is not marked.
        let mut once = r.clone();
        once.system_stable = 0;
        let body = anthropic_to_openai(&once, "max_tokens", OpenAiProvider::OPENROUTER_NAME)
            .expect("body");
        assert!(!body.to_string().contains("cache_control"));

        r.model = "deepseek/deepseek-v4".into();
        let other =
            anthropic_to_openai(&r, "max_tokens", OpenAiProvider::OPENROUTER_NAME).expect("body");
        assert!(!other.to_string().contains("cache_control"));
        r.model = "anthropic/claude-sonnet-4.5".into();
        let openai = anthropic_to_openai(&r, "max_tokens", "openai").expect("body");
        assert!(
            !openai.to_string().contains("cache_control"),
            "only behind OpenRouter"
        );
    }

    #[test]
    fn cached_prompt_tokens_are_counted_apart() {
        let usage = usage_of(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 700, "cache_write_tokens": 200},
        }))
        .expect("usage");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_read_input_tokens, Some(700));
        assert_eq!(usage.cache_creation_input_tokens, Some(200));
        assert_eq!(usage.total_input_tokens(), 1000);
        let deepseek = usage_of(&json!({
            "prompt_tokens": 500, "completion_tokens": 3,
            "prompt_cache_hit_tokens": 400, "prompt_cache_miss_tokens": 100,
        }))
        .expect("usage");
        assert_eq!(
            (deepseek.input_tokens, deepseek.cache_read_input_tokens),
            (100, Some(400))
        );
        let plain = usage_of(&json!({"prompt_tokens": 12, "completion_tokens": 4})).expect("usage");
        assert_eq!(
            (plain.input_tokens, plain.cache_read_input_tokens),
            (12, None)
        );
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
        let openai = anthropic_to_openai(&r, OpenAiProvider::OPENAI.max_tokens_field, "openai")
            .expect("body");
        assert_eq!(openai["max_completion_tokens"], 256);
        assert!(openai.get("max_tokens").is_none());
        let openrouter = anthropic_to_openai(
            &r,
            OpenAiProvider::OPENROUTER.max_tokens_field,
            "openrouter",
        )
        .expect("body");
        assert_eq!(openrouter["max_tokens"], 256);
    }

    #[tokio::test]
    async fn a_stream_cut_short_is_not_a_reply() {
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Half a\"}}]}\n\n",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let err = OpenAiProvider::OPENROUTER
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
                &|_: TextEvent<'_>| {},
            )
            .await
            .expect_err("no end, no reply");
        server.abort();
        assert!(matches!(err, ProviderError::Transport(_)), "{err}");
    }

    #[test]
    fn tool_calls_without_an_index_or_with_repeated_names_stay_apart() {
        let chunks = [
            json!({"choices": [{"delta": {"tool_calls": [{"id": "a", "function": {"name": "shell", "arguments": "{\"command\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"function": {"name": "shell", "arguments": "\"ls\"}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"id": "b", "function": {"name": "file_read", "arguments": "{\"path\":\"x\"}"}}]}, "finish_reason": "tool_calls"}]}),
        ];
        let mut reply = StreamedReply::default();
        for c in &chunks {
            reply.add(c);
        }
        let response = openai_to_anthropic(&reply.whole()).expect("translate");
        assert_eq!(response.content[0]["name"], "shell");
        assert_eq!(response.content[0]["input"]["command"], "ls");
        assert_eq!(response.content[1]["name"], "file_read");
        assert_eq!(response.content[1]["input"]["path"], "x");
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
    fn thinking_is_asked_for_as_each_endpoint_takes_it() {
        let ask = |reasoning: super::super::Reasoning, provider: &str| {
            let mut r = req(
                vec![AnthropicMessage {
                    role: "user".into(),
                    content: json!("hi"),
                }],
                None,
            );
            r.reasoning = Some(reasoning);
            r.max_tokens = 8192;
            anthropic_to_openai(&r, "max_tokens", provider).expect("translate")
        };
        use super::super::Reasoning;
        let or = OpenAiProvider::OPENROUTER_NAME;
        assert_eq!(
            ask(Reasoning::Effort("low".into()), or)["reasoning"],
            json!({"effort": "low"})
        );
        assert_eq!(
            ask(Reasoning::Budget(2048), or)["reasoning"],
            json!({"max_tokens": 2048})
        );
        assert_eq!(
            ask(Reasoning::Budget(60000), or)["reasoning"],
            json!({"max_tokens": 4096}),
            "a budget leaves the reply room"
        );
        assert_eq!(
            ask(Reasoning::Off, or)["reasoning"],
            json!({"enabled": false})
        );
        assert_eq!(
            ask(Reasoning::On, or)["reasoning"],
            json!({"enabled": true})
        );
        assert_eq!(
            ask(Reasoning::Effort("high".into()), "openai")["reasoning_effort"],
            "high"
        );
        assert!(
            ask(Reasoning::Off, "openai")
                .get("reasoning_effort")
                .is_none()
        );
        assert!(ask(Reasoning::Off, "openai").get("reasoning").is_none());
    }

    #[test]
    fn with_thinking_on_the_reasoning_goes_back_with_its_tool_call() {
        let tool_turn = |reasoning: Option<super::super::Reasoning>, provider: &str| {
            let mut r = req(
                vec![AnthropicMessage {
                    role: "assistant".into(),
                    content: json!([
                        {"type": "thinking", "thinking": "need the date"},
                        {"type": "tool_use", "id": "t1", "name": "shell", "input": {"command": "date"}},
                    ]),
                }],
                None,
            );
            r.reasoning = reasoning;
            anthropic_to_openai(&r, "max_tokens", provider).expect("translate")
        };
        use super::super::Reasoning;
        let on = tool_turn(Some(Reasoning::Effort("high".into())), "openai");
        assert_eq!(on["messages"][0]["reasoning_content"], "need the date");
        let or = tool_turn(Some(Reasoning::On), OpenAiProvider::OPENROUTER_NAME);
        assert_eq!(or["messages"][0]["reasoning"], "need the date");
        for off in [None, Some(Reasoning::Off)] {
            let body = tool_turn(off, "openai");
            assert!(!body.to_string().contains("need the date"));
        }
    }

    #[test]
    fn reasoning_is_kept_apart_and_not_sent_back() {
        // DeepSeek streams `reasoning_content`, OpenRouter `reasoning`.
        let chunks = [
            json!({"choices": [{"delta": {"reasoning_content": "The user wants "}}]}),
            json!({"choices": [{"delta": {"reasoning": "a number."}}]}),
            json!({"choices": [{"delta": {"content": "42"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ];
        let reasoning: Vec<&str> = chunks.iter().filter_map(reasoning_piece).collect();
        assert_eq!(reasoning, ["The user wants ", "a number."]);
        let mut reply = StreamedReply::default();
        let pieces: Vec<String> = chunks.iter().filter_map(|c| reply.add(c)).collect();
        assert_eq!(pieces, ["42"], "reasoning is not reply text");
        let response = openai_to_anthropic(&reply.whole()).expect("translate");
        assert_eq!(response.content[0]["type"], "thinking");
        assert_eq!(response.content[0]["thinking"], "The user wants a number.");
        assert_eq!(response.content[1]["text"], "42");

        // A call that does not stream gives it in the message.
        let raw = json!({"choices": [{"message": {"role": "assistant", "content": "ok", "reasoning_content": "think"}, "finish_reason": "stop"}]});
        let response = openai_to_anthropic(&raw).expect("translate");
        assert_eq!(response.content[0]["thinking"], "think");

        // Sent back in a later call, a thinking block is left out.
        let r = req(
            vec![AnthropicMessage {
                role: "assistant".into(),
                content: json!([
                    {"type": "thinking", "thinking": "secret"},
                    {"type": "text", "text": "42"},
                ]),
            }],
            None,
        );
        let body = anthropic_to_openai(&r, "max_tokens", "openai").expect("translate");
        assert_eq!(body["messages"][0]["content"], "42");
        assert!(!body.to_string().contains("secret"));
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
