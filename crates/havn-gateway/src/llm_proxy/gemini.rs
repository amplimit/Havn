//! Google Gemini provider via the Generative Language API.
//!
//! URL pattern: `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={api_key}`
//! (the API key rides on the query string, not a header — Gemini's choice,
//! not ours.)
//!
//! Translation differences vs Anthropic:
//!
//! * Roles — Gemini uses `user` and `model`; we map `assistant → model`.
//! * System prompt — separate `systemInstruction: {parts: [{text}]}` field,
//!   not a leading message.
//! * Content — every message has `parts: [{text} | {functionCall} |
//!   {functionResponse}]`. Anthropic content blocks map onto these
//!   one-for-one.
//! * Tools — `tools: [{functionDeclarations: [{name, description, parameters}]}]`.
//! * Response — `candidates[0].content.parts[]` and
//!   `candidates[0].finishReason`. Token usage in `usageMetadata.promptTokenCount`
//!   / `candidatesTokenCount`.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::provider::{LlmProvider, ProviderError};
use super::{AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicUsage};

const BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

#[derive(Debug, Default)]
pub struct GeminiProvider;

impl GeminiProvider {
    pub const NAME: &'static str = "gemini";
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    async fn complete(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        request: &AnthropicRequest,
    ) -> Result<AnthropicResponse, ProviderError> {
        let body = anthropic_to_gemini(request)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;

        let url = format!("{BASE}/{}:generateContent", request.model);
        // Gemini accepts `?key=...` query param OR `x-goog-api-key` header;
        // header keeps the key out of URL logs and works the same way.
        let resp = http
            .post(url)
            .header("x-goog-api-key", api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 => ProviderError::Unauthorized { body: body_text },
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
        gemini_to_anthropic(&raw, &request.model)
            .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))
    }
}

fn anthropic_to_gemini(req: &AnthropicRequest) -> Result<Value, String> {
    let mut contents: Vec<Value> = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        contents.push(translate_message_to_gemini(m)?);
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": req.max_tokens,
        }
    });
    if let Some(t) = req.temperature {
        body["generationConfig"]["temperature"] = json!(t);
    }
    if let Some(sys) = req.system.as_ref().filter(|s| !s.is_empty()) {
        body["systemInstruction"] = json!({"parts": [{"text": sys}]});
    }
    if let Some(tools) = req.tools.as_ref() {
        body["tools"] = translate_tools_to_gemini(tools)?;
    }
    Ok(body)
}

fn translate_message_to_gemini(m: &AnthropicMessage) -> Result<Value, String> {
    let role = match m.role.as_str() {
        "assistant" => "model",
        other => other, // user/system passthrough; system messages shouldn't appear here
    };

    if let Some(s) = m.content.as_str() {
        return Ok(json!({"role": role, "parts": [{"text": s}]}));
    }

    let blocks = m
        .content
        .as_array()
        .ok_or_else(|| format!("message.content must be string or array, got {}", m.content))?;

    let mut parts: Vec<Value> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("content block missing 'type': {block}"))?;
        match kind {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(json!({"text": t}));
                }
            }
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("tool_use block missing 'name'")?;
                let input = block.get("input").cloned().unwrap_or(json!({}));
                parts.push(json!({
                    "functionCall": {"name": name, "args": input}
                }));
            }
            "tool_result" => {
                // Gemini's functionResponse wants name + response.content.
                // We don't carry name on tool_result blocks (Anthropic
                // identifies by tool_use_id), but Gemini matches by name +
                // order. Use "tool" as a stable placeholder when missing —
                // round-trip from the openai/anthropic frontends always
                // sets name on tool_use, so this fallback only fires for
                // odd direct callers.
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let content = match block.get("content") {
                    Some(Value::String(s)) => json!({"result": s}),
                    Some(Value::Array(a)) => {
                        let combined: String = a
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        json!({"result": combined})
                    }
                    Some(other) => json!({"result": other.to_string()}),
                    None => json!({"result": ""}),
                };
                parts.push(json!({
                    "functionResponse": {"name": name, "response": content}
                }));
            }
            other => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(json!({"text": t}));
                } else {
                    parts.push(json!({"text": format!("(unsupported block: {other})")}));
                }
            }
        }
    }

    Ok(json!({"role": role, "parts": parts}))
}

fn translate_tools_to_gemini(tools: &Value) -> Result<Value, String> {
    let arr = tools
        .as_array()
        .ok_or_else(|| format!("tools must be an array, got {tools}"))?;
    let mut decls = Vec::with_capacity(arr.len());
    for t in arr {
        let name = t
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tool entry missing 'name'")?;
        let description = t.get("description").and_then(Value::as_str).unwrap_or("");
        let mut parameters = t.get("input_schema").cloned().unwrap_or(json!({
            "type": "object",
            "properties": {}
        }));
        sanitize_schema_for_gemini(&mut parameters);
        decls.push(json!({
            "name": name,
            "description": description,
            "parameters": parameters,
        }));
    }
    Ok(json!([{"functionDeclarations": decls}]))
}

/// Strip JSON-Schema fields that Gemini's OpenAPI-3.0 subset rejects.
///
/// Most tool schemas come from `schemars`-derived definitions (OpenAI's
/// permissive JSON Schema) — they routinely include `additionalProperties`,
/// `$schema`, `$ref`, and the union combinators `allOf` / `anyOf` /
/// `oneOf` / `not`. Gemini's `generateContent` rejects any of these with a
/// 400. We descend the schema tree once and drop them. The mutation is
/// in-place so we don't reallocate giant nested objects.
///
/// Real failure that prompted this: `additionalProperties` inside one of
/// the runtime tool schemas tripped Gemini with
/// `Invalid JSON payload received. Unknown name "additionalProperties"`.
fn sanitize_schema_for_gemini(v: &mut Value) {
    const STRIP: &[&str] = &[
        "additionalProperties",
        "$schema",
        "$ref",
        "definitions",
        "$defs",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "examples",
        "title",
        // Gemini supports `format` only on string types and only for a
        // small whitelist (date-time, etc.). It quietly tolerates unknown
        // formats but we still scrub the noisy ones — leaving `format` in
        // place is fine since the API is lenient about that field.
    ];

    match v {
        Value::Object(map) => {
            for k in STRIP {
                map.remove(*k);
            }
            for (_, child) in map.iter_mut() {
                sanitize_schema_for_gemini(child);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                sanitize_schema_for_gemini(child);
            }
        }
        _ => {}
    }
}

fn gemini_to_anthropic(raw: &Value, model: &str) -> Result<AnthropicResponse, String> {
    let candidate = raw
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or("response.candidates[0] missing")?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut content_blocks: Vec<Value> = Vec::with_capacity(parts.len());
    let mut tool_call_counter: u64 = 0;
    for part in &parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                content_blocks.push(json!({"type": "text", "text": text}));
            }
        } else if let Some(fc) = part.get("functionCall") {
            tool_call_counter += 1;
            let name = fc
                .get("name")
                .and_then(Value::as_str)
                .ok_or("functionCall missing 'name'")?;
            let input = fc.get("args").cloned().unwrap_or(json!({}));
            // Synthesise an id since Gemini doesn't return one. The runtime
            // uses the id only to pair tool_use with tool_result on the
            // next user turn; a per-response counter is sufficient.
            content_blocks.push(json!({
                "type": "tool_use",
                "id": format!("gemini_call_{tool_call_counter}"),
                "name": name,
                "input": input,
            }));
        }
    }

    let stop_reason = match candidate.get("finishReason").and_then(Value::as_str) {
        Some("STOP") => Some("end_turn".to_string()),
        Some("MAX_TOKENS") => Some("max_tokens".to_string()),
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
            Some("stop_sequence".to_string())
        }
        Some(other) if !other.is_empty() => Some(other.to_lowercase()),
        _ => None,
    };
    // If we emitted any tool_use blocks, override stop_reason to tool_use so
    // the runtime's tool_loop iterates. Gemini sometimes returns finishReason
    // STOP even when functionCall parts are present.
    let stop_reason = if content_blocks
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        Some("tool_use".to_string())
    } else {
        stop_reason
    };

    let usage = raw.get("usageMetadata").map_or(
        AnthropicUsage {
            input_tokens: 0,
            output_tokens: 0,
        },
        |u| AnthropicUsage {
            input_tokens: u
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: u
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
    );

    Ok(AnthropicResponse {
        id: raw
            .get("responseId")
            .and_then(Value::as_str)
            .unwrap_or("gemini-no-id")
            .to_string(),
        model: model.to_string(),
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
            model: "gemini-2.5-flash".into(),
            max_tokens: 256,
            messages,
            system: system.map(String::from),
            temperature: None,
            tools: None,
        }
    }

    #[test]
    fn translates_system_to_system_instruction() {
        let r = req(
            vec![AnthropicMessage {
                role: "user".into(),
                content: json!("hi"),
            }],
            Some("be brief"),
        );
        let body = anthropic_to_gemini(&r).expect("translate");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        let contents = body["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hi");
    }

    #[test]
    fn translates_assistant_to_model_role() {
        let r = req(
            vec![AnthropicMessage {
                role: "assistant".into(),
                content: json!("ok"),
            }],
            None,
        );
        let body = anthropic_to_gemini(&r).expect("translate");
        assert_eq!(body["contents"][0]["role"], "model");
    }

    #[test]
    fn translates_tool_use_block_to_function_call_part() {
        let r = req(
            vec![AnthropicMessage {
                role: "assistant".into(),
                content: json!([
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "tu_1", "name": "shell", "input": {"command": "ls"}}
                ]),
            }],
            None,
        );
        let body = anthropic_to_gemini(&r).expect("translate");
        let parts = body["contents"][0]["parts"].as_array().expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "checking");
        assert_eq!(parts[1]["functionCall"]["name"], "shell");
        assert_eq!(parts[1]["functionCall"]["args"]["command"], "ls");
    }

    #[test]
    fn sanitizer_strips_additional_properties_and_combinators() {
        let mut s = json!({
            "type": "object",
            "additionalProperties": false,
            "$schema": "http://json-schema.org/draft-07/schema#",
            "properties": {
                "input": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "additionalProperties": true
                }
            },
            "definitions": {"Foo": {"type": "string"}}
        });
        sanitize_schema_for_gemini(&mut s);
        let obj = s.as_object().expect("object");
        assert!(!obj.contains_key("additionalProperties"));
        assert!(!obj.contains_key("$schema"));
        assert!(!obj.contains_key("definitions"));
        let inner = obj["properties"]["input"].as_object().expect("inner");
        assert!(!inner.contains_key("anyOf"));
        assert!(!inner.contains_key("additionalProperties"));
        // `properties` and `type` survive.
        assert_eq!(obj["type"], "object");
    }

    #[test]
    fn translate_tools_drops_additional_properties() {
        let tools = json!([{
            "name": "shell",
            "description": "run a command",
            "input_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }
        }]);
        let out = translate_tools_to_gemini(&tools).expect("translate");
        let decl = &out[0]["functionDeclarations"][0];
        let params = decl["parameters"].as_object().expect("parameters");
        assert!(!params.contains_key("additionalProperties"));
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["command"]["type"], "string");
    }

    #[test]
    fn parses_gemini_text_response() {
        let raw = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello!"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2}
        });
        let canon = gemini_to_anthropic(&raw, "gemini-2.5-flash").expect("parse");
        assert_eq!(canon.stop_reason.as_deref(), Some("end_turn"));
        let blocks = canon.content.as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello!");
        assert_eq!(canon.usage.input_tokens, 5);
        assert_eq!(canon.usage.output_tokens, 2);
    }

    #[test]
    fn parses_gemini_function_call_to_tool_use_with_synthetic_id() {
        let raw = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "shell", "args": {"command": "pwd"}}}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 30, "candidatesTokenCount": 10}
        });
        let canon = gemini_to_anthropic(&raw, "gemini-2.5-pro").expect("parse");
        // Even when Gemini reports finishReason=STOP, presence of a
        // functionCall flips stop_reason to tool_use so the runtime loop
        // executes the tool.
        assert_eq!(canon.stop_reason.as_deref(), Some("tool_use"));
        let blocks = canon.content.as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["name"], "shell");
        assert_eq!(blocks[0]["input"]["command"], "pwd");
        // Synthetic id is non-empty and stable per response.
        assert!(
            blocks[0]["id"]
                .as_str()
                .unwrap()
                .starts_with("gemini_call_")
        );
    }
}
