//! `McpToolHandle` — adapter that exposes one MCP server tool as a
//! havn `Tool` (spec §13 Phase 3, decision #11).
//!
//! Tool name in the registry: `mcp__<server>__<tool>` — same convention
//! as Claude Desktop. Disambiguates two MCP servers that both expose a
//! tool called e.g. `search`, and lets PreToolUse hooks (§14b) match
//! `^mcp__github__` to allow one server's tools but not another.
//!
//! Schema: the MCP server reports a JSON Schema for each tool's input
//! (the `inputSchema` field). We forward that verbatim to Anthropic in
//! the tool definition — no coercion, no mapping. If the server author
//! and we disagree about what `additionalProperties` means, the LLM's
//! tool call goes to the server unchanged and the server decides.
//!
//! Result shape: rmcp's `CallToolResult` carries one or more content
//! blocks (text / image / resource). v1 only handles text and the
//! is_error flag; non-text content is rendered as a brief placeholder
//! so the LLM at least sees that something came back. Image / resource
//! handling is future work paired with a concrete user need.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::{McpCallError, McpClient};
use crate::tools::{Tool, ToolCtx, ToolResult};

/// Per-tool registry entry that knows which `McpClient` to call.
pub struct McpToolHandle {
    /// Registry-visible name: `mcp__<server>__<tool>`.
    name: String,
    /// One-line summary copied from the MCP `tool.description` field
    /// (or a synthesised default if the server omitted it).
    description: String,
    /// JSON Schema for the tool's input — the raw `inputSchema` from
    /// the MCP server's `tools/list` response.
    input_schema: Value,
    /// Live handle to the server. Cloned per-tool so a server with
    /// many tools shares one connection.
    client: Arc<McpClient>,
    /// Original (un-prefixed) tool name to forward in `tools/call`.
    upstream_name: String,
}

impl McpToolHandle {
    /// Build the registry name for a given (server, tool) pair.
    pub fn registry_name(server: &str, tool: &str) -> String {
        format!("mcp__{server}__{tool}")
    }

    pub fn new(client: Arc<McpClient>, tool: rmcp::model::Tool) -> Self {
        let upstream_name = tool.name.to_string();
        let registry_name = Self::registry_name(&client.server_name, &upstream_name);
        // rmcp's Tool.description is Option<Cow<'static, str>>; fall
        // back to a generic when the server omits it. Anthropic's tool
        // definition treats description as optional too, but a useful
        // tool description meaningfully helps the model decide when
        // to invoke — so we'd rather show something than nothing.
        let description = tool.description.as_deref().map_or_else(
            || {
                format!(
                    "MCP tool exposed by server {:?} (no description provided).",
                    client.server_name
                )
            },
            str::to_owned,
        );
        let input_schema = serde_json::to_value(&*tool.input_schema)
            .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
        Self {
            name: registry_name,
            description,
            input_schema,
            client,
            upstream_name,
        }
    }
}

#[async_trait]
impl Tool for McpToolHandle {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, _ctx: &ToolCtx, input: Value) -> ToolResult {
        let raw = match self.client.call_tool(&self.upstream_name, input).await {
            Ok(r) => r,
            Err(McpCallError::Timeout { server, seconds }) => {
                return ToolResult::Error(format!(
                    "MCP server {server:?} timed out after {seconds}s; \
                     consider raising `timeout_seconds` in policy or \
                     splitting the work."
                ));
            }
            Err(e) => {
                return ToolResult::Error(format!("MCP call failed: {e}"));
            }
        };
        let text = render_content_blocks(&raw);
        if raw.is_error.unwrap_or(false) {
            ToolResult::Error(text)
        } else {
            ToolResult::Ok(text)
        }
    }
}

/// Flatten a CallToolResult's content blocks into the runtime's
/// text-only `ToolResult`. Images / resources / audio are rendered
/// as a short placeholder — present so the LLM sees something came
/// back, and tagged so a future vertical can handle them properly.
fn render_content_blocks(result: &rmcp::model::CallToolResult) -> String {
    use rmcp::model::RawContent;
    let mut parts: Vec<String> = Vec::with_capacity(result.content.len());
    for block in &result.content {
        match &block.raw {
            RawContent::Text(t) => parts.push(t.text.clone()),
            RawContent::Image(_) => parts.push("[mcp image content omitted in v1]".into()),
            RawContent::Audio(_) => parts.push("[mcp audio content omitted in v1]".into()),
            RawContent::Resource(_) => parts.push("[mcp resource content omitted in v1]".into()),
            RawContent::ResourceLink(_) => {
                parts.push("[mcp resource link omitted in v1]".into());
            }
        }
    }
    if parts.is_empty() {
        // Empty result is legal per MCP spec but unhelpful as a tool
        // output — the LLM can't see "nothing happened" cleanly. A
        // short marker is friendlier.
        "(mcp tool returned no content)".into()
    } else {
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn registry_name_uses_double_underscore() {
        assert_eq!(
            McpToolHandle::registry_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(
            McpToolHandle::registry_name("file system", "read"),
            "mcp__file system__read",
            "spaces and other tokens passed through verbatim — caller \
             validates server names earlier"
        );
    }
}
