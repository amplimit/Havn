//! Policy schema — the data form of every agent permission and quota.
//!
//! Stored as JSONB in `Role.policy`; serialized as YAML in role-manager exports.
//! See spec §6.2 for the full field list and enforcement points.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub max_agents: u32,
    pub allowed_models: Vec<String>,
    pub resource_limits: ResourceLimits,
    pub budget: BudgetPolicy,
    pub permissions: Permissions,
    pub network_policy: NetworkPolicy,
    pub context_toolsets: ContextToolsets,
    pub admin_visibility: AdminVisibility,
    /// Whitelist of MCP servers this agent may invoke (spec §13 Phase 3).
    /// Empty (the default) means "no MCP servers". Each entry's
    /// `binary` is a name looked up under `/usr/share/havn/mcp-servers/`
    /// — never a host path, so an agent cannot escape to anything the
    /// operator hasn't pre-installed. The runtime registers each
    /// server's tools as `mcp__<server>__<tool>`.
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_agents: 1,
            allowed_models: vec!["*".into()],
            resource_limits: ResourceLimits::default(),
            budget: BudgetPolicy::default(),
            permissions: Permissions::default(),
            network_policy: NetworkPolicy::default(),
            context_toolsets: ContextToolsets::default(),
            admin_visibility: AdminVisibility::default(),
            mcp_servers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceLimits {
    pub memory_mb: u64,
    pub cpu_cores: f64,
    pub pids_max: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_mb: 512,
            cpu_cores: 1.0,
            pids_max: 64,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetPolicy {
    /// 0 = unlimited.
    ///
    /// v0.6 (spec §7.3): tokens are the only first-class budget unit
    /// havn enforces. Earlier drafts had `max_usd_per_day` driven by a
    /// model-pricing table; cut because pricing changes weekly and a
    /// stale table is worse than no table. Operators who care about $
    /// compute it from `credential_usages` token totals in their own
    /// analytics pipeline.
    pub max_tokens_per_day: u64,
    pub on_exhaust: BudgetExhaustAction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetExhaustAction {
    WarnOnly,
    #[default]
    WarnAndPause,
    Fail,
}

/// Permission flags. Mirrors spec §6.2 one-to-one — each flag maps to a
/// concrete enforcement point listed in §6.3, so the field set is fixed
/// by the spec rather than free design space.
#[allow(
    clippy::struct_excessive_bools,
    reason = "policy schema mirrors spec §6.2"
)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub can_install_skills: bool,
    pub can_access_network: bool,
    pub can_use_shell: bool,
    pub can_view_own_logs: bool,
    pub can_export_memory: bool,
    pub can_bind_channels: ChannelAllowance,
    pub can_spawn_subagents: bool,
    pub can_schedule_cron: bool,
    /// Master gate for the `mcp_servers` whitelist (spec §13 Phase 3).
    /// When false the runtime skips spawning every MCP server in
    /// `policy.mcp_servers` and the `mcp__*__*` tools never reach the
    /// LLM. Default off, same as `can_spawn_subagents` — pricier
    /// capabilities are opt-in.
    pub can_use_mcp: bool,
    /// Cross-agent query (spec §4.4 v0.7). When true the runtime
    /// registers `agent_query`, letting this agent ask another agent
    /// owned by the same user a question and receive the answer as a
    /// `tool_result`. Default off — same opt-in posture as
    /// `can_spawn_subagents` and `can_use_mcp`. Recursion is prevented
    /// at the runtime level (an agent serving an incoming query never
    /// sees `agent_query` in its registry, regardless of this flag).
    pub can_query_other_agents: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            can_install_skills: true,
            can_access_network: true,
            can_use_shell: true,
            can_view_own_logs: true,
            can_export_memory: true,
            can_bind_channels: ChannelAllowance::All,
            can_spawn_subagents: false,
            can_schedule_cron: true,
            can_use_mcp: false,
            can_query_other_agents: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChannelAllowance {
    #[default]
    All,
    Allowed(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkPolicy {
    pub egress_allowed: bool,
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
}

/// One MCP server allowed to run as an agent-runtime child process
/// (spec §13 Phase 3, decision #4 + bonus). The operator pre-installs
/// the binary under `/usr/share/havn/mcp-servers/<binary>`; the
/// spawner bind-mounts that directory RO into the agent's mount
/// namespace. Per-server `extra_paths_*` are unioned into the agent's
/// Landlock allowlist by the spawner so the server can do its job
/// (e.g. the `filesystem` MCP server needs `/workspace/data` writable)
/// without weakening the agent's overall sandbox more than necessary.
///
/// Note on the union model: because Landlock is one-shot per process
/// and child processes inherit, the spawner applies the union of all
/// enabled servers' paths once at agent boot. This means the agent's
/// own `file_read`/`file_write` tools can also reach those paths — we
/// don't get per-server isolation. Acceptable v1; tightening would
/// need either a per-server reaper subprocess or the eBPF-based
/// LSM stacking, neither worth the complexity yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// File name (NOT path) under `/usr/share/havn/mcp-servers/`.
    /// Empty default so a config that forgets to set it errors loudly
    /// at startup rather than silently picking up the directory itself.
    pub binary: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    /// Read-write paths the spawner unions into Landlock.
    pub extra_paths_rw: Vec<PathBuf>,
    /// Read-only paths the spawner unions into Landlock.
    pub extra_paths_ro: Vec<PathBuf>,
    /// How long any single `tools/call` may run before the runtime
    /// abandons it and returns a `tool_result` error. Per-server so a
    /// flaky server can be quarantined without globally bumping the
    /// timeout. Default 60s.
    pub timeout_seconds: u64,
    /// Disable a server without removing the entry — useful when an
    /// operator is debugging.
    pub enabled: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            binary: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            extra_paths_rw: Vec::new(),
            extra_paths_ro: Vec::new(),
            timeout_seconds: 60,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextToolsets(pub HashMap<String, ContextToolsetEntry>);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextToolsetEntry {
    pub disabled: Vec<String>,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "policy schema mirrors spec §6.2"
)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminVisibility {
    pub can_view_agent_status: bool,
    pub can_view_agent_config: bool,
    pub can_view_conversations: bool,
    pub can_view_audit_log: bool,
}

impl Default for AdminVisibility {
    fn default() -> Self {
        Self {
            can_view_agent_status: true,
            can_view_agent_config: true,
            // Privacy default — admin cannot read chat content unless explicitly enabled.
            can_view_conversations: false,
            can_view_audit_log: true,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn default_policy_round_trips_through_json() {
        let p = Policy::default();
        let json = serde_json::to_string(&p).expect("serialize");
        let parsed: Policy = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed.resource_limits.memory_mb, 512);
        assert_eq!(parsed.budget.on_exhaust, BudgetExhaustAction::WarnAndPause);
        assert!(!parsed.admin_visibility.can_view_conversations);
        assert!(!parsed.permissions.can_use_mcp);
        assert!(parsed.mcp_servers.is_empty());
    }

    #[test]
    fn mcp_server_config_round_trips_with_defaults() {
        // Operators write YAML/JSON with as few fields as they can get
        // away with — serde defaults must apply so a minimal config
        // stays compact.
        let raw = serde_json::json!({
            "filesystem": {
                "binary": "mcp-server-filesystem",
                "args": ["--root", "/workspace/data"],
                "extra_paths_rw": ["/workspace/data"],
            }
        });
        let parsed: HashMap<String, McpServerConfig> = serde_json::from_value(raw).expect("parse");
        let fs = parsed.get("filesystem").expect("entry");
        assert_eq!(fs.binary, "mcp-server-filesystem");
        assert_eq!(fs.args, vec!["--root", "/workspace/data"]);
        assert_eq!(fs.extra_paths_rw, vec![PathBuf::from("/workspace/data")]);
        assert!(fs.extra_paths_ro.is_empty());
        assert_eq!(fs.timeout_seconds, 60);
        assert!(fs.enabled);
        assert!(fs.env.is_empty());
    }
}
