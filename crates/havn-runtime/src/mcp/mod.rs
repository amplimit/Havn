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
//! - **stdio, or remote.** A server with a `url` is remote (issue #234):
//!   Streamable HTTP or HTTP+SSE through the agent's network egress
//!   ([`remote`]); its OAuth is the gateway's.
//! - **No agent-side install.** Binaries are operator-managed; agents
//!   can only invoke whitelisted ones. Same posture as "no
//!   marketplace, no skill registry."
//!
//! Failure mode is fail-soft: a server that doesn't start, doesn't
//! handshake, or doesn't list tools just doesn't register — the
//! agent boots without it and logs the reason. We do NOT crash the
//! agent for one broken MCP server. How each started goes to the
//! gateway ([`havn_proto::McpStatus`]) for the agent's settings page.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use havn_core::{McpServerConfig, Policy};
use havn_proto::{AgentToGateway, McpServerStatus, McpStatus};
use rmcp::RoleClient;
use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool as McpTool};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::child_process::TokioChildProcess;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub mod remote;
pub mod tool;

/// How the agent's MCP servers are, as last sent to the gateway.
static STATUS: Mutex<Vec<McpServerStatus>> = Mutex::new(Vec::new());
/// Where [`STATUS`] goes.
static REPORT: OnceLock<mpsc::Sender<AgentToGateway>> = OnceLock::new();

/// Send how the servers started to the gateway through `writer`, and later
/// changes too.
pub fn report_to(writer: mpsc::Sender<AgentToGateway>) {
    let _ = REPORT.set(writer);
    send_status();
}

fn send_status() {
    let Some(writer) = REPORT.get() else {
        return;
    };
    let servers = STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if writer
        .try_send(AgentToGateway::McpStatus(McpStatus { servers }))
        .is_err()
    {
        warn!("the MCP servers' status could not be sent to the gateway");
    }
}

/// `error` as a status keeps it: the agent's secrets redacted, cut.
fn status_error(error: &str) -> String {
    let redacted = crate::secrets::redact(error);
    if redacted.chars().count() <= havn_proto::MAX_MCP_ERROR_CHARS {
        return redacted;
    }
    let cut: String = redacted
        .chars()
        .take(havn_proto::MAX_MCP_ERROR_CHARS)
        .collect();
    format!("{cut}…")
}

/// A call to `server` failed on its connection: its status says so.
fn call_failed(server: &str, error: &str) {
    {
        let mut status = STATUS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = status.iter_mut().find(|s| s.name == server) else {
            return;
        };
        let error = status_error(error);
        if entry.error.as_deref() == Some(error.as_str()) {
            return;
        }
        entry.error = Some(error);
    }
    send_status();
}

/// On-host directory where operators place MCP server binaries.
/// Inside the agent namespace this path resolves through the existing
/// `/usr` bind mount (spawner ns_setup), so no extra mount logic is
/// needed for v1 — operators put binaries at
/// `/usr/share/havn/mcp-servers/<binary>` on the host and they're
/// automatically visible (and Landlock-allowed via the `/usr` rule)
/// inside the agent.
pub const MCP_SERVERS_DIR: &str = "/usr/share/havn/mcp-servers";

/// The servers the agent's own settings define as they run (from
/// `Welcome`): only theirs resolve `secret:<name>` in `env`. A server a
/// role supplies, which a team admin writes, never gets the owner's
/// secrets (issue #233).
static OWN_SERVERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Set once, from `Welcome`.
pub fn set_own_servers(names: Vec<String>) {
    let _ = OWN_SERVERS.set(names);
}

fn is_own(name: &str) -> bool {
    OWN_SERVERS
        .get()
        .is_some_and(|own| own.iter().any(|n| n == name))
}

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
    /// turn forever. rmcp tells the server with `notifications/cancelled`
    /// when the timeout drops the call.
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
        let result =
            tokio::time::timeout(self.timeout, fut)
                .await
                .map_err(|_| McpCallError::Timeout {
                    server: self.server_name.clone(),
                    seconds: self.timeout.as_secs(),
                })?;
        match result {
            Ok(result) => Ok(result),
            Err(e) => {
                // A server's own error answer is the tool's result; one of
                // its connection is the server's state.
                if !matches!(e, ServiceError::McpError(_)) {
                    call_failed(&self.server_name, &e.to_string());
                }
                Err(McpCallError::Service(e))
            }
        }
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
        let mut status = Vec::new();
        // Sort by server name so log lines are deterministic.
        let mut entries: Vec<(&String, &McpServerConfig)> = policy.mcp_servers.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        // Remote servers connect at once, each within its deadline, so a few
        // that do not answer cannot hold the agent's start for long.
        let remote = entries
            .iter()
            .filter(|(_, cfg)| cfg.enabled && cfg.is_remote())
            .map(|(name, cfg)| async move {
                let started = tokio::time::timeout(REMOTE_START, connect_remote(name, cfg, policy))
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "it did not start in {}s",
                            REMOTE_START.as_secs()
                        ))
                    });
                ((*name).clone(), started)
            });
        let mut remote: HashMap<String, anyhow::Result<McpClient>> =
            futures_util::future::join_all(remote)
                .await
                .into_iter()
                .collect();
        for (name, cfg) in entries {
            if !cfg.enabled {
                info!(server = %name, "MCP server disabled in policy; skipping");
                continue;
            }
            let started = if cfg.is_remote() {
                remote
                    .remove(name)
                    .unwrap_or_else(|| Err(anyhow::anyhow!("it was not started")))
            } else {
                if let Err(e) = validate_binary_name(&cfg.binary) {
                    warn!(
                        server = %name,
                        binary = %cfg.binary,
                        error = e,
                        "MCP server binary name rejected"
                    );
                    status.push(McpServerStatus {
                        name: name.clone(),
                        connected: false,
                        tools: Vec::new(),
                        error: Some(e.to_string()),
                    });
                    continue;
                }
                spawn_one(name, cfg).await
            };
            match started {
                Ok(client) => {
                    info!(
                        server = %name,
                        binary = %cfg.binary,
                        url = %cfg.url,
                        tool_count = client.tools.len(),
                        "MCP server up"
                    );
                    status.push(McpServerStatus {
                        name: name.clone(),
                        connected: true,
                        tools: client.tools.iter().map(|t| t.name.to_string()).collect(),
                        error: None,
                    });
                    clients.insert(name.clone(), Arc::new(client));
                }
                Err(e) => {
                    warn!(
                        server = %name,
                        binary = %cfg.binary,
                        url = %cfg.url,
                        error = %e,
                        "MCP server failed to start; agent will boot without it"
                    );
                    status.push(McpServerStatus {
                        name: name.clone(),
                        connected: false,
                        tools: Vec::new(),
                        error: Some(status_error(&format!("{e:#}"))),
                    });
                }
            }
        }
        *STATUS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
        Self { clients }
    }
}

/// How long a remote server may take to connect and list its tools.
const REMOTE_START: Duration = Duration::from_secs(90);

/// Connect to one remote server, do the handshake, list its tools.
async fn connect_remote(
    name: &str,
    cfg: &McpServerConfig,
    policy: &Policy,
) -> anyhow::Result<McpClient> {
    let svc = remote::connect(name, cfg, policy, is_own(name)).await?;
    let tools = tokio::time::timeout(
        Duration::from_secs(cfg.timeout_seconds.max(1)),
        svc.list_all_tools(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("tools/list timed out"))??;
    Ok(McpClient {
        server_name: name.to_string(),
        inner: Arc::new(svc),
        tools,
        timeout: Duration::from_secs(cfg.timeout_seconds.max(1)),
    })
}

/// Spawn one MCP server, do the handshake, list its tools.
async fn spawn_one(name: &str, cfg: &McpServerConfig) -> anyhow::Result<McpClient> {
    let bin_path = format!("{MCP_SERVERS_DIR}/{}", cfg.binary);
    let mut command = Command::new(&bin_path);
    #[cfg(target_os = "linux")]
    if crate::sandbox::has_tool_home() {
        command.env("HOME", crate::sandbox::MCP_HOME);
    }
    #[cfg(target_os = "linux")]
    crate::sandbox::apply_tool_proxy(&mut command);
    for arg in &cfg.args {
        command.arg(arg);
    }
    // `secret:<name>` values are the agent's secrets (issue #233), resolved
    // only for a server the agent's own settings define; a server whose
    // secret is not set, or that a role supplies, does not start.
    if cfg.uses_secrets() && !is_own(name) {
        anyhow::bail!(
            "its env refers to secrets, and it runs with settings the owner's role supplies, \
             not the agent's own, so they are not resolved; the owner can define the server \
             in the agent's MCP settings"
        );
    }
    for (k, v) in &cfg.env {
        let value = crate::secrets::resolve(v).map_err(|e| anyhow::anyhow!("env {k}: {e}"))?;
        command.env(k, value);
    }
    // The server's stderr goes to the runtime's, as its log, with the
    // agent's secrets redacted, also those over several lines, and long
    // lines cut.
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stderr) = stderr {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut stream = crate::secrets::Stream::default();
            let mut buf = vec![0_u8; 8 * 1024];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => crate::secrets::write_stderr(&stream.push(&buf[..n])),
                }
            }
            crate::secrets::write_stderr(&stream.finish());
        });
    }
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
    async fn a_server_a_role_supplies_gets_no_secrets() {
        // Welcome named no server as the agent's own here.
        let cfg = McpServerConfig {
            binary: "gh-mcp".into(),
            env: HashMap::from([("GH_TOKEN".to_string(), "secret:github".to_string())]),
            ..Default::default()
        };
        let Err(e) = spawn_one("gh", &cfg).await else {
            panic!("a role's server started with the owner's secret");
        };
        assert!(e.to_string().contains("role"), "{e}");
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
