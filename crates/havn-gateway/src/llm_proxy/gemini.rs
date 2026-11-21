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

use super::provider::{
    LlmProvider, ProviderError, Sse, TextEvent, TextSink, cut_short, is_event_stream, pass_text,
    status_error,
};
use super::{AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicUsage};

const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

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
        // The model name becomes part of the URL path, so it must not be able
        // to add path segments, a query string or a fragment.
        if !is_plain_model_name(&request.model) {
            return Err(ProviderError::BadRequest {
                status: 400,
                body: format!("invalid Gemini model name {:?}", request.model),
            });
        }
        let body = anthropic_to_gemini(request)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;

        let url = format!("{base_url}/models/{}:generateContent", request.model);
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
            let wait = super::provider::retry_after(resp.headers());
            let body_text = resp.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 => ProviderError::Unauthorized { body: body_text },
                402 | 429 => ProviderError::QuotaOrRateLimit {
                    status: status.as_u16(),
                    body: body_text,
                    retry_after: wait,
                },
                500..=599 => ProviderError::Upstream {
                    status: status.as_u16(),
                    body: body_text,
                    retry_after: wait,
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

    async fn stream(
        &self,
        http: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        request: &AnthropicRequest,
        text: &TextSink,
    ) -> Result<AnthropicResponse, ProviderError> {
        if !is_plain_model_name(&request.model) {
            return Err(ProviderError::BadRequest {
                status: 400,
                body: format!("invalid Gemini model name {:?}", request.model),
            });
        }
        let body = anthropic_to_gemini(request)
            .map_err(|e| ProviderError::Translation(format!("request build: {e}")))?;
        let url = format!(
            "{base_url}/models/{}:streamGenerateContent?alt=sse",
            request.model
        );
        let resp = http
            .post(url)
            .header("x-goog-api-key", api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let wait = super::provider::retry_after(resp.headers());
            let body_text = resp.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                403 => ProviderError::Unauthorized { body: body_text },
                code => status_error(code, body_text, wait),
            });
        }
        if !is_event_stream(&resp) {
            let raw: Value = resp
                .json()
                .await
                .map_err(|e| ProviderError::Translation(format!("response decode: {e}")))?;
            let response = gemini_to_anthropic(&raw, &request.model)
                .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))?;
            pass_text(&response, text);
            return Ok(response);
        }
        let mut events = Sse::new(resp);
        let mut reply = StreamedReply::default();
        while let Some(data) = events.next().await? {
            let chunk: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Translation(format!("stream chunk decode: {e}")))?;
            if let Some(error) = chunk.get("error") {
                return Err(ProviderError::Upstream {
                    retry_after: None,
                    status: 502,
                    body: error.to_string(),
                });
            }
            for part in chunk
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part
                    .get("thought")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    && let Some(piece) = part.get("text").and_then(Value::as_str)
                    && !piece.is_empty()
                {
                    text(TextEvent::Reasoning(piece));
                }
            }
            for piece in reply.add(&chunk) {
                text(TextEvent::Text(&piece));
            }
        }
        // A blocked prompt ends with its feedback and no candidate.
        if reply.finish.is_none() && reply.last.pointer("/promptFeedback/blockReason").is_none() {
            return Err(cut_short());
        }
        gemini_to_anthropic(&reply.whole(), &request.model)
            .map_err(|e| ProviderError::Translation(format!("response translate: {e}")))
    }
}

fn is_plain_model_name(model: &str) -> bool {
    let mut chars = model.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
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
    // How much to think (issue #190), with the thoughts returned.
    if let Some(r) = &req.reasoning {
        let config = match r {
            super::Reasoning::Off => Some(json!({"thinkingBudget": 0})),
            super::Reasoning::On => Some(json!({"thinkingBudget": -1, "includeThoughts": true})),
            super::Reasoning::Budget(n) => super::budget_within(*n, req.max_tokens)
                .map(|n| json!({"thinkingBudget": n, "includeThoughts": true})),
            super::Reasoning::Effort(level) => {
                Some(json!({"thinkingLevel": level, "includeThoughts": true}))
            }
        };
        if let Some(config) = config {
            body["generationConfig"]["thinkingConfig"] = config;
        }
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
                // An image a tool showed (issue #227) follows its response.
                let mut shown = Vec::new();
                let content = match block.get("content") {
                    Some(Value::String(s)) => json!({"result": s}),
                    Some(Value::Array(a)) => {
                        shown.extend(
                            a.iter()
                                .filter(|p| super::openai::is_base64_image(p))
                                .map(inline_data),
                        );
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
                parts.append(&mut shown);
            }
            "image" | "audio"
                if block.pointer("/source/type").and_then(Value::as_str) == Some("base64") =>
            {
                parts.push(inline_data(block));
            }
            // A model's reasoning is not sent back to it.
            "thinking" | "redacted_thinking" => {}
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

/// Base64 block `block` (an image, audio) as an `inline_data` part.
fn inline_data(block: &Value) -> Value {
    json!({
        "inline_data": {
            "mime_type": block.pointer("/source/media_type").and_then(Value::as_str).unwrap_or("image/png"),
            "data": block.pointer("/source/data").and_then(Value::as_str).unwrap_or_default(),
        }
    })
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

/// A streamed `generateContent` response, put back together: the parts of
/// every chunk, a text part added to the text part before it.
#[derive(Default)]
struct StreamedReply {
    parts: Vec<Value>,
    finish: Option<Value>,
    last: Value,
}

impl StreamedReply {
    /// Add one chunk; the pieces of reply text it adds (not thoughts).
    fn add(&mut self, chunk: &Value) -> Vec<String> {
        let mut pieces = Vec::new();
        let parts = chunk
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for part in parts {
            let thought = part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(piece) = part.get("text").and_then(Value::as_str) {
                if !thought && !piece.is_empty() {
                    pieces.push(piece.to_string());
                }
                let joins = self.parts.last().is_some_and(|last| {
                    last.get("text").is_some()
                        && last
                            .get("thought")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                            == thought
                });
                if joins && let Some(last) = self.parts.last_mut() {
                    let whole = format!("{}{piece}", last["text"].as_str().unwrap_or(""));
                    last["text"] = Value::String(whole);
                    if let Some(signature) = part.get("thoughtSignature") {
                        last["thoughtSignature"] = signature.clone();
                    }
                    continue;
                }
            }
            self.parts.push(part);
        }
        if let Some(finish) = chunk.pointer("/candidates/0/finishReason") {
            self.finish = Some(finish.clone());
        }
        self.last = chunk.clone();
        pieces
    }

    /// The whole response, as the API answers a call that does not stream:
    /// the last chunk's finish reason, usage and feedback, with every part.
    fn whole(&self) -> Value {
        // Nothing came but the last chunk (a blocked prompt's feedback):
        // it is the response.
        if self.parts.is_empty() && self.finish.is_none() {
            return self.last.clone();
        }
        let mut whole = self.last.clone();
        if !whole.is_object() {
            whole = json!({});
        }
        let mut candidate = json!({ "content": { "role": "model", "parts": self.parts } });
        if let Some(finish) = &self.finish {
            candidate["finishReason"] = finish.clone();
        }
        whole["candidates"] = json!([candidate]);
        whole
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
            // A thought (asked for with `includeThoughts`) is reasoning, not
            // the reply.
            if part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                if !text.is_empty() {
                    content_blocks.push(json!({"type": "thinking", "thinking": text}));
                }
            } else if !text.is_empty() {
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

    // Thinking tokens are billed as output but reported apart from the
    // candidates. The JSON mapping omits zero counts, so a missing output
    // count is 0; a missing prompt count (never 0) means no usage.
    let usage = raw
        .get("usageMetadata")
        .and_then(|u| {
            let count = |key: &str| u.get(key).and_then(Value::as_u64).unwrap_or(0);
            Some(AnthropicUsage {
                input_tokens: u.get("promptTokenCount")?.as_u64()?,
                output_tokens: count("candidatesTokenCount") + count("thoughtsTokenCount"),
                ..AnthropicUsage::default()
            })
        })
        .unwrap_or_else(AnthropicUsage::unreported);

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

    #[test]
    fn a_streamed_reply_is_put_back_together() {
        let chunks = [
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "plan", "thought": true}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "lo", "thoughtSignature": "sig"}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"functionCall": {"name": "shell", "args": {"command": "ls"}}}]}, "finishReason": "STOP"}]}),
            json!({"usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 4}}),
        ];
        let mut reply = StreamedReply::default();
        let pieces: Vec<String> = chunks.iter().flat_map(|c| reply.add(c)).collect();
        assert_eq!(pieces, ["Hel", "lo"]);
        let whole = reply.whole();
        assert_eq!(whole["candidates"][0]["finishReason"], "STOP");
        let parts = whole["candidates"][0]["content"]["parts"]
            .as_array()
            .expect("parts");
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[1]["text"], "Hello");
        assert_eq!(parts[1]["thoughtSignature"], "sig");
        assert_eq!(whole["usageMetadata"]["promptTokenCount"], 9);
    }

    #[test]
    fn model_names_cannot_change_the_url() {
        for ok in ["gemini-2.5-flash", "gemini-2.0-flash-001", "gemini_exp.1"] {
            assert!(is_plain_model_name(ok), "{ok}");
        }
        for bad in [
            "",
            "../cachedContents",
            "gemini-2.5-flash:countTokens?x=",
            "a/b",
            "a#b",
            ".hidden",
            "gemini 2.5",
        ] {
            assert!(!is_plain_model_name(bad), "{bad}");
        }
    }

    fn req(messages: Vec<AnthropicMessage>, system: Option<&str>) -> AnthropicRequest {
        AnthropicRequest {
            model: "gemini-2.5-flash".into(),
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
    fn an_image_a_tool_showed_follows_its_response() {
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
                    }
                ]),
            }],
            None,
        );
        let body = anthropic_to_gemini(&r).expect("translate");
        let parts = body["contents"][0]["parts"].as_array().expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["functionResponse"]["response"]["result"],
            "The image chart.png:"
        );
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
        assert_eq!(parts[1]["inline_data"]["data"], "AAAA");
    }

    #[test]
    fn thinking_is_asked_for_and_thoughts_kept_apart() {
        use super::super::Reasoning;
        let ask = |reasoning: Option<Reasoning>| {
            let mut r = req(
                vec![AnthropicMessage {
                    role: "user".into(),
                    content: json!("hi"),
                }],
                None,
            );
            r.reasoning = reasoning;
            r.max_tokens = 8192;
            anthropic_to_gemini(&r).expect("translate")["generationConfig"]["thinkingConfig"]
                .clone()
        };
        assert_eq!(ask(Some(Reasoning::Off)), json!({"thinkingBudget": 0}));
        assert_eq!(
            ask(Some(Reasoning::Budget(2048))),
            json!({"thinkingBudget": 2048, "includeThoughts": true})
        );
        assert_eq!(
            ask(Some(Reasoning::Effort("low".into()))),
            json!({"thinkingLevel": "low", "includeThoughts": true})
        );
        assert_eq!(ask(None), Value::Null);
        let raw = json!({"candidates": [{"content": {"role": "model", "parts": [
            {"text": "first the sum", "thought": true},
            {"text": "42"},
        ]}, "finishReason": "STOP"}]});
        let resp = gemini_to_anthropic(&raw, "gemini-x").expect("translate");
        assert_eq!(
            resp.content[0],
            json!({"type": "thinking", "thinking": "first the sum"})
        );
        assert_eq!(resp.content[1], json!({"type": "text", "text": "42"}));
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
    fn thinking_tokens_count_as_output() {
        let raw = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hi"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "thoughtsTokenCount": 40}
        });
        let canon = gemini_to_anthropic(&raw, "gemini-2.5-pro").expect("parse");
        assert_eq!(canon.usage.output_tokens, 42);
        assert!(!canon.usage.unreported);
        let mut partial = raw.clone();
        partial["usageMetadata"] = json!({"candidatesTokenCount": 2});
        let canon = gemini_to_anthropic(&partial, "gemini-2.5-pro").expect("parse");
        assert!(canon.usage.unreported);
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
