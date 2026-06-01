//! MCP (Model Context Protocol) client integration — spec §13 Phase 3.
//!
//! Operator-installed MCP server binaries under
//! `/usr/share/havn/mcp-servers/<name>` are spawned as agent-runtime
//! child processes (decision #4: inherit namespace + landlock + seccomp
//! + cgroup). Each spawn does the standard MCP `initialize` handshake,
//!   lists the server's tools, and registers each as
//!   `mcp__<server>__<tool>` in the runtime's `ToolRegistry` (when
//!   `policy.permissions.can_use_mcp` is true and the server is in the
//!   policy whitelist).
//!
//! v1 scope (decisions locked above the implementation):
//! - **Tools only.** Resources / Prompts deferred — Resources fight
//!   with the frozen-system-prompt invariant (§9.4) and Prompts
//!   overlap with `SKILL.md`.
//! - **stdio transport only.** SSE / streamable-HTTP cut to keep the
//!   "default no network" invariant intact and avoid OAuth surface.
//! - **No agent-side install.** Binaries are operator-managed; agents
//!   can only invoke whitelisted ones. Same posture as "no
//!   marketplace, no skill registry."
//!
//! Failure mode is fail-soft: a server that doesn't start, doesn't
//! handshake, or doesn't list tools just doesn't register — the
//! agent boots without it and logs the reason. We do NOT crash the
//! agent for one broken MCP server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use havn_core::{McpServerConfig, Policy};
use rmcp::RoleClient;
use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool as McpTool};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::child_process::TokioChildProcess;
use tokio::process::Command;
use tracing::{info, warn};

pub mod tool;

/// On-host directory where operators place MCP server binaries.
/// Inside the agent namespace this path resolves through the existing
/// `/usr` bind mount (spawner ns_setup), so no extra mount logic is
/// needed for v1 — operators put binaries at
/// `/usr/share/havn/mcp-servers/<binary>` on the host and they're
/// automatically visible (and Landlock-allowed via the `/usr` rule)
/// inside the agent.
pub const MCP_SERVERS_DIR: &str = "/usr/share/havn/mcp-servers";

/// Live handles to every successfully-spawned MCP server for this
/// agent session. Read by `McpToolHandle::execute` to forward
/// `tools/call`. Held in an `Arc` so the registry can clone cheaply
/// across many `McpToolHandle` instances (one per server tool).
pub struct McpServers {
    clients: HashMap<String, Arc<McpClient>>,
}

#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for McpServers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServers")
            .field("server_count", &self.clients.len())
            .field("server_names", &self.clients.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One live MCP server connection.
pub struct McpClient {
    /// Server name as it appears in `policy.mcp_servers` — used to
    /// build the `mcp__<server>__<tool>` registry name.
    pub server_name: String,
    /// Tools the server reported via `tools/list` at handshake time.
    pub tools: Vec<McpTool>,
    /// rmcp service handle. Kept behind a mutex-free Arc since rmcp's
    /// `RunningService` methods take `&self`.
    inner: Arc<RunningService<RoleClient, ()>>,
    /// Per-server timeout for `tools/call`.
    pub timeout: Duration,
}

#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server_name", &self.server_name)
            .field("tool_count", &self.tools.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl McpClient {
    /// Forward a `tools/call` to the server. Returns the raw
    /// `CallToolResult` so the caller (`tool::McpToolHandle`) can map
    /// it into the runtime's tool-result shape. Subject to the
    /// per-server timeout: a server that hangs doesn't pin the agent's
    /// turn forever.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpCallError> {
        let arg_object = match arguments {
            serde_json::Value::Object(map) if !map.is_empty() => Some(map),
            // Some MCP servers reject `arguments: {}` (treating empty
            // as missing). Prefer `None` for empty + Null both.
            serde_json::Value::Object(_) | serde_json::Value::Null => None,
            other => {
                return Err(McpCallError::BadArguments(format!(
                    "expected JSON object, got {other:?}"
                )));
            }
        };
        // `CallToolRequestParams` is `#[non_exhaustive]` — must use
        // the `new(name)` + `with_arguments(...)` builder pattern.
        // `with_arguments` takes the inner `serde_json::Map`, not
        // `Value::Object(map)`.
        let mut req = CallToolRequestParams::new(tool_name.to_string());
        if let Some(args) = arg_object {
            req = req.with_arguments(args);
        }
        let fut = self.inner.call_tool(req);
        let result = tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| McpCallError::Timeout {
                server: self.server_name.clone(),
                seconds: self.timeout.as_secs(),
            })?
            .map_err(McpCallError::Service)?;
        Ok(result)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpCallError {
    #[error("mcp server {server:?} timed out after {seconds}s")]
    Timeout { server: String, seconds: u64 },
    #[error("mcp service error: {0}")]
    Service(#[from] ServiceError),
    #[error("bad arguments: {0}")]
    BadArguments(String),
}

impl McpServers {
    /// Read-only access to the underlying server map — the registry
    /// builder iterates this to register one `McpToolHandle` per
    /// tool-per-server.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<McpClient>)> {
        self.clients.iter()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    pub fn empty() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Spawn every whitelisted server in `policy.mcp_servers`, perform
    /// the MCP handshake + `tools/list`, return the live handles.
    /// Servers that fail at any step are skipped with a warn log; the
    /// agent boots regardless. Sequential spawn (not parallel) keeps
    /// log lines coherent and limits transient resource pressure on
    /// underpowered hosts.
    pub async fn start(policy: &Policy) -> Self {
        if !policy.permissions.can_use_mcp {
            return Self::empty();
        }
        if policy.mcp_servers.is_empty() {
            return Self::empty();
        }
        let mut clients: HashMap<String, Arc<McpClient>> = HashMap::new();
        // Sort by server name so log lines are deterministic.
        let mut entries: Vec<(&String, &McpServerConfig)> = policy.mcp_servers.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, cfg) in entries {
            if !cfg.enabled {
                info!(server = %name, "MCP server disabled in policy; skipping");
                continue;
            }
            if let Err(e) = validate_binary_name(&cfg.binary) {
                warn!(
                    server = %name,
                    binary = %cfg.binary,
                    error = e,
                    "MCP server binary name rejected"
                );
                continue;
            }
            match spawn_one(name, cfg).await {
                Ok(client) => {
                    info!(
                        server = %name,
                        binary = %cfg.binary,
                        tool_count = client.tools.len(),
                        "MCP server up"
                    );
                    clients.insert(name.clone(), Arc::new(client));
                }
                Err(e) => {
                    warn!(
                        server = %name,
                        binary = %cfg.binary,
                        error = %e,
                        "MCP server failed to start; agent will boot without it"
                    );
                }
            }
        }
        Self { clients }
    }
}

/// Spawn one MCP server, do the handshake, list its tools.
async fn spawn_one(name: &str, cfg: &McpServerConfig) -> anyhow::Result<McpClient> {
    let bin_path = format!("{MCP_SERVERS_DIR}/{}", cfg.binary);
    let mut command = Command::new(&bin_path);
    for arg in &cfg.args {
        command.arg(arg);
    }
    for (k, v) in &cfg.env {
        command.env(k, v);
    }
    // child stderr inherits the runtime's stderr by default (rmcp
    // configures stdin/stdout for the JSON-RPC pipe), which means
    // server log output ends up in the agent's tracing pipeline —
    // useful for debugging without separate plumbing.
    let transport = TokioChildProcess::new(command)?;
    // `().serve(t)` performs the MCP `initialize` handshake; we get
    // back a handle that exposes `list_all_tools` and `call_tool`.
    let svc = ().serve(transport).await?;
    let tools = svc.list_all_tools().await?;
    Ok(McpClient {
        server_name: name.to_string(),
        inner: Arc::new(svc),
        tools,
        timeout: Duration::from_secs(cfg.timeout_seconds.max(1)),
    })
}

/// Reject binary names that could escape the MCP-server install
/// directory or contain unsafe characters. The runtime never
/// concatenates a user-supplied path; the binary is always looked up
/// at `<MCP_SERVERS_DIR>/<binary>`. Still, we defend against:
/// - empty (would resolve to the directory itself, a TypeError)
/// - `/` (path traversal beyond the install dir)
/// - `..` segment (backreference)
/// - control bytes (most filesystems accept them but they're a sign
///   the policy was tampered with)
fn validate_binary_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty binary name");
    }
    if name.contains('/') {
        return Err("binary must be a file name, not a path");
    }
    if name == "." || name == ".." || name.contains("..") {
        return Err("binary name must not contain '..'");
    }
    if name.bytes().any(|b| b.is_ascii_control()) {
        return Err("binary name contains control bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn rejects_path_traversal_in_binary_name() {
        for bad in [
            "",
            "/etc/passwd",
            "../etc/passwd",
            "..",
            ".",
            "good/../bad",
            "good\nname",
        ] {
            assert!(
                validate_binary_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_normal_binary_names() {
        for ok in [
            "mcp-server-filesystem",
            "git_mcp",
            "weather.exe",
            "tool-1.2.3",
        ] {
            assert!(
                validate_binary_name(ok).is_ok(),
                "expected {ok:?} to be accepted"
            );
        }
    }

    #[tokio::test]
    async fn start_returns_empty_when_can_use_mcp_is_false() {
        let mut p = Policy::default();
        p.permissions.can_use_mcp = false;
        // even if a server is configured, the master gate kills it
        p.mcp_servers.insert(
            "fs".into(),
            McpServerConfig {
                binary: "anything".into(),
                ..Default::default()
            },
        );
        let s = McpServers::start(&p).await;
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn start_skips_disabled_entries_without_spawning() {
        // If we tried to spawn a missing binary the test would error;
        // disabled = pass-through means we never reach the spawn path.
        let mut p = Policy::default();
        p.permissions.can_use_mcp = true;
        p.mcp_servers.insert(
            "fs".into(),
            McpServerConfig {
                binary: "definitely-not-installed-anywhere".into(),
                enabled: false,
                ..Default::default()
            },
        );
        let s = McpServers::start(&p).await;
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn start_warns_and_continues_when_binary_missing() {
        // Real-world failure mode — operator typoed the binary name
        // or forgot to install it. The agent must still boot.
        let mut p = Policy::default();
        p.permissions.can_use_mcp = true;
        p.mcp_servers.insert(
            "fs".into(),
            McpServerConfig {
                binary: "havn-mcp-this-binary-does-not-exist".into(),
                ..Default::default()
            },
        );
        let s = McpServers::start(&p).await;
        // Spawn failed → the server is not in the map. No panic.
        assert!(s.is_empty());
    }
}
