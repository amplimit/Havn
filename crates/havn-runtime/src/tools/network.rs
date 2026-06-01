//! `web_fetch` tool — HTTP GET / POST via the runtime's reqwest client.
//!
//! Spec policy gate: `permissions.can_access_network`. Phase 1 single-user
//! mode runs all agents under the same network namespace as the gateway, so
//! once this tool is exposed the agent can reach any host the runtime can.
//! `NamespaceSpawner` (next vertical) restricts egress at the kernel level
//! per spec §4.1 / §6.2 `network_policy`.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::time::Duration;

use super::{Tool, ToolCtx, ToolResult};

const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a URL over HTTP/HTTPS. Method defaults to GET. Returns the \
         response body as text (up to 256 KB) along with status code and \
         response headers. 30-second timeout."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url":     { "type": "string", "description": "Absolute http(s) URL." },
                "method":  {
                    "type": "string",
                    "enum": ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"],
                    "default": "GET"
                },
                "headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Request headers as a flat string→string map."
                },
                "body": { "type": "string", "description": "Request body (use only for POST/PUT/PATCH)." }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(url) = input.get("url").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: url".into());
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolResult::Error("url must be http:// or https://".into());
        }
        let method = input.get("method").and_then(Value::as_str).unwrap_or("GET");

        let mut req = match method.to_ascii_uppercase().as_str() {
            "GET" => ctx.http.get(url),
            "POST" => ctx.http.post(url),
            "PUT" => ctx.http.put(url),
            "DELETE" => ctx.http.delete(url),
            "PATCH" => ctx.http.patch(url),
            "HEAD" => ctx.http.head(url),
            other => return ToolResult::Error(format!("unsupported method: {other}")),
        };
        req = req.timeout(REQUEST_TIMEOUT);

        if let Some(headers) = input.get("headers").and_then(Value::as_object) {
            for (k, v) in headers {
                if let Some(s) = v.as_str() {
                    req = req.header(k, s);
                }
            }
        }
        if let Some(body) = input.get("body").and_then(Value::as_str) {
            req = req.body(body.to_string());
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::Error(format!("request failed: {e}")),
        };

        let status = resp.status();
        // Materialise headers before consuming the response body.
        let mut headers_dump = String::new();
        for (name, value) in resp.headers() {
            if let Ok(s) = value.to_str() {
                writeln!(headers_dump, "{name}: {s}").ok();
            }
        }

        let body_bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return ToolResult::Error(format!("body read failed: {e}")),
        };
        let truncated = body_bytes.len() > MAX_RESPONSE_BYTES;
        let body_slice = &body_bytes[..body_bytes.len().min(MAX_RESPONSE_BYTES)];
        let body_text = String::from_utf8_lossy(body_slice).into_owned();

        let mut out = String::new();
        writeln!(
            out,
            "HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        )
        .ok();
        out.push_str(&headers_dump);
        out.push('\n');
        out.push_str(&body_text);
        if truncated {
            write!(
                out,
                "\n\n[response truncated at {MAX_RESPONSE_BYTES} bytes; full size {} bytes]",
                body_bytes.len()
            )
            .ok();
        }
        ToolResult::Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use std::sync::Arc;

    async fn ctx() -> ToolCtx {
        let pool = havn_db::agent::connect_in_memory().await.expect("db");
        let workspace = std::env::temp_dir().join(format!("havn-net-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&workspace).await.expect("ws");
        ToolCtx {
            workspace_dir: workspace,
            agent_db: pool,
            http: reqwest::Client::new(),
            policy: Arc::new(Policy::default()),
            embedder: None,
        }
    }

    #[tokio::test]
    async fn rejects_missing_url() {
        let r = WebFetchTool.execute(&ctx().await, json!({})).await;
        assert!(r.is_error());
        assert!(r.text().contains("url"));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let r = WebFetchTool
            .execute(&ctx().await, json!({"url": "file:///etc/passwd"}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("http"));
    }

    #[tokio::test]
    async fn rejects_unsupported_method() {
        let r = WebFetchTool
            .execute(
                &ctx().await,
                json!({"url": "https://example.com", "method": "TRACE"}),
            )
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("unsupported"));
    }
}
