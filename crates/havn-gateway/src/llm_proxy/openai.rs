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

use super::provider::{LlmProvider, ProviderError};
use super::{AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicUsage};

const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// OpenAI-compatible Chat Completions provider. The same translator backs
/// OpenAI itself and OpenRouter — the only differences are `base_url` and a
/// couple of optional headers that OpenRouter recommends for analytics.
#[derive(Debug)]
pub struct OpenAiProvider {
    /// Used by [`LlmProvider::name`] for diagnostics; dead-code analysis
    /// can't follow the trait dispatch back to this field.
    #[allow(dead_code)]
    name: &'static str,
    base_url: &'static str,
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
        extra_headers: &[],
    };

    pub const OPENROUTER: Self = Self {
        name: Self::OPENROUTER_NAME,
        base_url: OPENROUTER_URL,
        extra_headers: &[("HTTP-Referer", "https://havn.dev"), ("X-Title", "havn")],
    };
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn complete(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        request: &AnthropicRequest,
    ) -> Result<AnthropicResponse, ProviderError> {
        let body = anthropic_to_openai(request)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;

        let mut builder = http.post(self.base_url).bearer_auth(api_key).json(&body);
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
            return Err(match status.as_u16() {
                401 => ProviderError::Unauthorized { body: body_text },
                402 | 429 => ProviderError::QuotaOrRateLimit {
                    status: status.as_u16(),
                    body: body_text,
                },
                500..=599 => ProviderError::Upstream {
                    status: status.as_u16(),
                    body: body_text,
                },
                other => ProviderError::BadRequest {
                    status: other,
                    body: body_text,
                },
            });
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Translation(format!("response decode: {e}")))?;
        openai_to_anthropic(&raw)
            .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))
    }
}

/// Translate a canonical Anthropic request into an OpenAI Chat Completions
/// body. Tool uses / results inside content blocks are unwrapped into
/// OpenAI's split `tool_calls` (assistant) / `role: "tool"` (response)
/// shape.
fn anthropic_to_openai(req: &AnthropicRequest) -> Result<Value, String> {
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
        "max_tokens": req.max_tokens,
    });
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

    let usage = raw.get("usage").map_or(
        AnthropicUsage {
            input_tokens: 0,
            output_tokens: 0,
        },
        |u| AnthropicUsage {
            input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            output_tokens: u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
    );

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
    fn translates_plain_text_round_trip() {
        let r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!("hello"),
            }],
            Some("you are a calc"),
        );
        let body = anthropic_to_openai(&r).expect("translate");
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
        let body = anthropic_to_openai(&r).expect("translate");
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
        let body = anthropic_to_openai(&r).expect("translate");
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
        let body = anthropic_to_openai(&r).expect("translate");
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
