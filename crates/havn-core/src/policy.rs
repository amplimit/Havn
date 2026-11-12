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
    /// Operator PreToolUse deny rules, same shape as a workspace
    /// `.havn/hooks.yaml`. The runtime applies them together with the
    /// workspace file, which can add rules but not remove these.
    pub hooks: HookPolicy,
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
            hooks: HookPolicy::default(),
        }
    }
}

/// PreToolUse hooks: guardrails against model mistakes, matched as
/// regexes on a tool's flattened input. They are not a security
/// boundary; `permissions`, `network_policy` and the sandbox are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookPolicy {
    pub pre_tool_use: Vec<ToolHookRule>,
}

/// Deny rule for one tool: the call is refused when any `deny` regex
/// matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHookRule {
    pub tool: String,
    #[serde(default)]
    pub deny: Vec<String>,
}

pub const MAX_HOOK_RULES: usize = 64;
pub const MAX_HOOK_PATTERNS: usize = 256;
pub const MAX_HOOK_PATTERN_LEN: usize = 1024;
/// Compiled-program and DFA-cache limit [`HookPolicy::validate`] applies
/// to each pattern.
pub const HOOK_REGEX_SIZE_LIMIT: usize = 256 * 1024;

/// Compile one deny pattern with a small compiled-program and DFA-cache
/// limit, so a single pattern cannot claim much CPU or memory.
pub fn compile_hook_pattern(pattern: &str, size_limit: usize) -> crate::Result<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .size_limit(size_limit)
        .dfa_size_limit(size_limit)
        .build()
        .map_err(|e| crate::Error::InvalidPolicy(format!("hook regex {pattern:?}: {e}")))
}

impl Policy {
    /// Checks that deserialisation cannot express. Deserialising stays
    /// cheap because the gateway resolves policies on every request;
    /// call this where a policy is written.
    pub fn validate(&self) -> crate::Result<()> {
        self.hooks.validate()
    }
}

impl HookPolicy {
    /// Enforces the rule, pattern and length caps and compiles every
    /// pattern.
    pub fn validate(&self) -> crate::Result<()> {
        let invalid = |m: String| Err(crate::Error::InvalidPolicy(m));
        if self.pre_tool_use.len() > MAX_HOOK_RULES {
            return invalid(format!(
                "hooks: more than {MAX_HOOK_RULES} pre_tool_use rules"
            ));
        }
        let patterns: usize = self.pre_tool_use.iter().map(|r| r.deny.len()).sum();
        if patterns > MAX_HOOK_PATTERNS {
            return invalid(format!(
                "hooks: more than {MAX_HOOK_PATTERNS} deny patterns"
            ));
        }
        for rule in &self.pre_tool_use {
            if rule.tool.trim().is_empty() {
                return invalid("hooks: a pre_tool_use rule has an empty tool".into());
            }
            for pattern in &rule.deny {
                if pattern.len() > MAX_HOOK_PATTERN_LEN {
                    return invalid(format!(
                        "hooks: a {} deny pattern is longer than {MAX_HOOK_PATTERN_LEN} bytes",
                        rule.tool
                    ));
                }
                compile_hook_pattern(pattern, HOOK_REGEX_SIZE_LIMIT)?;
            }
        }
        Ok(())
    }

    /// Every rule of `self` and `other`; a pattern already present for
    /// the same tool is not repeated.
    #[must_use]
    pub fn union(&self, other: &HookPolicy) -> HookPolicy {
        let mut merged = self.clone();
        for rule in &other.pre_tool_use {
            let missing: Vec<String> = rule
                .deny
                .iter()
                .filter(|p| !merged.denies(&rule.tool, p))
                .cloned()
                .collect();
            if !missing.is_empty() {
                merged.pre_tool_use.push(ToolHookRule {
                    tool: rule.tool.clone(),
                    deny: missing,
                });
            }
        }
        merged
    }

    fn denies(&self, tool: &str, pattern: &str) -> bool {
        self.pre_tool_use
            .iter()
            .any(|r| r.tool == tool && r.deny.iter().any(|p| p == pattern))
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

impl Policy {
    #[must_use]
    pub fn clamp_to(&self, ceiling: &Policy) -> Policy {
        let own = &self.permissions;
        let cap = &ceiling.permissions;
        Policy {
            max_agents: self.max_agents.min(ceiling.max_agents),
            allowed_models: narrow_wildcard_list(&self.allowed_models, &ceiling.allowed_models),
            resource_limits: ResourceLimits {
                memory_mb: min_limit(
                    self.resource_limits.memory_mb,
                    ceiling.resource_limits.memory_mb,
                ),
                cpu_cores: min_cpu(
                    self.resource_limits.cpu_cores,
                    ceiling.resource_limits.cpu_cores,
                ),
                pids_max: min_limit(
                    self.resource_limits.pids_max,
                    ceiling.resource_limits.pids_max,
                ),
            },
            budget: BudgetPolicy {
                max_tokens_per_day: min_limit(
                    self.budget.max_tokens_per_day,
                    ceiling.budget.max_tokens_per_day,
                ),
                on_exhaust: stricter_exhaust(self.budget.on_exhaust, ceiling.budget.on_exhaust),
            },
            permissions: Permissions {
                can_install_skills: own.can_install_skills && cap.can_install_skills,
                can_access_network: own.can_access_network && cap.can_access_network,
                can_use_shell: own.can_use_shell && cap.can_use_shell,
                can_view_own_logs: own.can_view_own_logs && cap.can_view_own_logs,
                can_export_memory: own.can_export_memory && cap.can_export_memory,
                can_bind_channels: narrow_channels(&own.can_bind_channels, &cap.can_bind_channels),
                can_spawn_subagents: own.can_spawn_subagents && cap.can_spawn_subagents,
                can_schedule_cron: own.can_schedule_cron && cap.can_schedule_cron,
                can_use_mcp: own.can_use_mcp && cap.can_use_mcp,
                can_query_other_agents: own.can_query_other_agents && cap.can_query_other_agents,
            },
            network_policy: NetworkPolicy {
                egress_allowed: self.network_policy.egress_allowed
                    && ceiling.network_policy.egress_allowed,
                allowed_domains: narrow_allowlist(
                    &self.network_policy.allowed_domains,
                    &ceiling.network_policy.allowed_domains,
                ),
                blocked_domains: union(
                    &self.network_policy.blocked_domains,
                    &ceiling.network_policy.blocked_domains,
                ),
            },
            context_toolsets: union_disabled(&self.context_toolsets, &ceiling.context_toolsets),
            admin_visibility: ceiling.admin_visibility.clone(),
            mcp_servers: ceiling
                .mcp_servers
                .iter()
                .filter_map(|(name, granted)| {
                    let requested = self.mcp_servers.get(name)?;
                    let mut server = granted.clone();
                    server.enabled = granted.enabled && requested.enabled;
                    Some((name.clone(), server))
                })
                .collect(),
            hooks: self.hooks.union(&ceiling.hooks),
        }
    }

    #[must_use]
    pub fn widen_with(&self, other: &Policy) -> Policy {
        let a = &self.permissions;
        let b = &other.permissions;
        Policy {
            max_agents: self.max_agents.max(other.max_agents),
            allowed_models: widen_wildcard_list(&self.allowed_models, &other.allowed_models),
            resource_limits: ResourceLimits {
                memory_mb: max_limit(
                    self.resource_limits.memory_mb,
                    other.resource_limits.memory_mb,
                ),
                cpu_cores: max_cpu(
                    self.resource_limits.cpu_cores,
                    other.resource_limits.cpu_cores,
                ),
                pids_max: max_limit(
                    self.resource_limits.pids_max,
                    other.resource_limits.pids_max,
                ),
            },
            budget: BudgetPolicy {
                max_tokens_per_day: max_limit(
                    self.budget.max_tokens_per_day,
                    other.budget.max_tokens_per_day,
                ),
                on_exhaust: if stricter_exhaust(self.budget.on_exhaust, other.budget.on_exhaust)
                    == self.budget.on_exhaust
                {
                    other.budget.on_exhaust
                } else {
                    self.budget.on_exhaust
                },
            },
            permissions: Permissions {
                can_install_skills: a.can_install_skills || b.can_install_skills,
                can_access_network: a.can_access_network || b.can_access_network,
                can_use_shell: a.can_use_shell || b.can_use_shell,
                can_view_own_logs: a.can_view_own_logs || b.can_view_own_logs,
                can_export_memory: a.can_export_memory || b.can_export_memory,
                can_bind_channels: widen_channels(&a.can_bind_channels, &b.can_bind_channels),
                can_spawn_subagents: a.can_spawn_subagents || b.can_spawn_subagents,
                can_schedule_cron: a.can_schedule_cron || b.can_schedule_cron,
                can_use_mcp: a.can_use_mcp || b.can_use_mcp,
                can_query_other_agents: a.can_query_other_agents || b.can_query_other_agents,
            },
            network_policy: NetworkPolicy {
                egress_allowed: self.network_policy.egress_allowed
                    || other.network_policy.egress_allowed,
                allowed_domains: widen_allowlist(
                    &self.network_policy.allowed_domains,
                    &other.network_policy.allowed_domains,
                ),
                blocked_domains: intersect(
                    &self.network_policy.blocked_domains,
                    &other.network_policy.blocked_domains,
                ),
            },
            context_toolsets: intersect_disabled(&self.context_toolsets, &other.context_toolsets),
            admin_visibility: AdminVisibility {
                can_view_agent_status: self.admin_visibility.can_view_agent_status
                    || other.admin_visibility.can_view_agent_status,
                can_view_agent_config: self.admin_visibility.can_view_agent_config
                    || other.admin_visibility.can_view_agent_config,
                can_view_conversations: self.admin_visibility.can_view_conversations
                    || other.admin_visibility.can_view_conversations,
                can_view_audit_log: self.admin_visibility.can_view_audit_log
                    || other.admin_visibility.can_view_audit_log,
            },
            mcp_servers: {
                let mut merged = self.mcp_servers.clone();
                for (name, server) in &other.mcp_servers {
                    merged.entry(name.clone()).or_insert_with(|| server.clone());
                }
                merged
            },
            // Deny rules are guardrails an operator wrote for a role, so
            // combining roles keeps all of them rather than only the
            // rules every role shares.
            hooks: self.hooks.union(&other.hooks),
        }
    }
}

fn max_limit<T: Copy + Ord + Default>(a: T, b: T) -> T {
    let unlimited = T::default();
    if a == unlimited || b == unlimited {
        unlimited
    } else {
        a.max(b)
    }
}

fn max_cpu(a: f64, b: f64) -> f64 {
    if a <= 0.0 || b <= 0.0 { 0.0 } else { a.max(b) }
}

fn widen_wildcard_list(a: &[String], b: &[String]) -> Vec<String> {
    if a.iter().chain(b).any(|m| m == "*") {
        vec!["*".to_string()]
    } else {
        union(a, b)
    }
}

fn widen_allowlist(a: &[String], b: &[String]) -> Vec<String> {
    if a.is_empty() || b.is_empty() {
        Vec::new()
    } else {
        widen_wildcard_list(a, b)
    }
}

fn widen_channels(a: &ChannelAllowance, b: &ChannelAllowance) -> ChannelAllowance {
    match (a, b) {
        (ChannelAllowance::Allowed(x), ChannelAllowance::Allowed(y)) => {
            ChannelAllowance::Allowed(union(x, y))
        }
        _ => ChannelAllowance::All,
    }
}

fn intersect(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|x| b.contains(x)).cloned().collect()
}

fn intersect_disabled(a: &ContextToolsets, b: &ContextToolsets) -> ContextToolsets {
    ContextToolsets(
        a.0.iter()
            .filter_map(|(context, entry)| {
                let other = b.0.get(context)?;
                Some((
                    context.clone(),
                    ContextToolsetEntry {
                        disabled: intersect(&entry.disabled, &other.disabled),
                    },
                ))
            })
            .collect(),
    )
}

fn min_limit<T: Copy + Ord + Default>(a: T, b: T) -> T {
    let unlimited = T::default();
    if a == unlimited {
        b
    } else if b == unlimited {
        a
    } else {
        a.min(b)
    }
}

fn min_cpu(a: f64, b: f64) -> f64 {
    if a <= 0.0 {
        b
    } else if b <= 0.0 {
        a
    } else {
        a.min(b)
    }
}

fn stricter_exhaust(a: BudgetExhaustAction, b: BudgetExhaustAction) -> BudgetExhaustAction {
    let rank = |x: BudgetExhaustAction| match x {
        BudgetExhaustAction::WarnOnly => 0,
        BudgetExhaustAction::WarnAndPause => 1,
        BudgetExhaustAction::Fail => 2,
    };
    if rank(a) >= rank(b) { a } else { b }
}

fn narrow_wildcard_list(own: &[String], cap: &[String]) -> Vec<String> {
    if cap.iter().any(|m| m == "*") {
        own.to_vec()
    } else if own.iter().any(|m| m == "*") {
        cap.to_vec()
    } else {
        own.iter().filter(|m| cap.contains(m)).cloned().collect()
    }
}

fn narrow_allowlist(own: &[String], cap: &[String]) -> Vec<String> {
    match (own.is_empty(), cap.is_empty()) {
        (true, _) => cap.to_vec(),
        (false, true) => own.to_vec(),
        (false, false) => narrow_wildcard_list(own, cap),
    }
}

fn narrow_channels(own: &ChannelAllowance, cap: &ChannelAllowance) -> ChannelAllowance {
    match (own, cap) {
        (ChannelAllowance::All, other) | (other, ChannelAllowance::All) => other.clone(),
        (ChannelAllowance::Allowed(a), ChannelAllowance::Allowed(b)) => {
            ChannelAllowance::Allowed(a.iter().filter(|c| b.contains(c)).cloned().collect())
        }
    }
}

fn union(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    out.extend(b.iter().filter(|x| !a.contains(x)).cloned());
    out
}

fn union_disabled(own: &ContextToolsets, cap: &ContextToolsets) -> ContextToolsets {
    let mut merged = own.0.clone();
    for (context, entry) in &cap.0 {
        let slot = merged.entry(context.clone()).or_default();
        slot.disabled = union(&slot.disabled, &entry.disabled);
    }
    ContextToolsets(merged)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn clamp_never_widens_permissions_or_limits() {
        let mut cap = Policy::default();
        cap.permissions.can_use_shell = false;
        cap.permissions.can_use_mcp = false;
        cap.resource_limits.memory_mb = 256;
        cap.resource_limits.pids_max = 32;
        cap.budget.max_tokens_per_day = 10_000;
        let mut own = Policy::default();
        own.permissions.can_use_shell = true;
        own.permissions.can_use_mcp = true;
        own.permissions.can_spawn_subagents = true;
        own.resource_limits.memory_mb = 0;
        own.resource_limits.pids_max = 4096;
        own.budget.max_tokens_per_day = 0;
        own.network_policy.egress_allowed = true;

        let p = own.clamp_to(&cap);
        assert!(!p.permissions.can_use_shell);
        assert!(!p.permissions.can_use_mcp);
        assert!(!p.permissions.can_spawn_subagents);
        assert!(!p.network_policy.egress_allowed);
        assert_eq!(p.resource_limits.memory_mb, 256);
        assert_eq!(p.resource_limits.pids_max, 32);
        assert_eq!(p.budget.max_tokens_per_day, 10_000);
    }

    #[test]
    fn widen_takes_the_most_permissive_of_two_roles() {
        let mut a = Policy::default();
        a.permissions.can_use_shell = false;
        a.resource_limits.memory_mb = 512;
        a.budget.max_tokens_per_day = 1_000;
        a.allowed_models = vec!["m1".into()];
        a.context_toolsets.0.insert(
            "cron".into(),
            ContextToolsetEntry {
                disabled: vec!["shell".into(), "web_fetch".into()],
            },
        );
        let mut b = Policy::default();
        b.permissions.can_use_shell = true;
        b.resource_limits.memory_mb = 1024;
        b.budget.max_tokens_per_day = 0;
        b.allowed_models = vec!["m2".into()];
        b.context_toolsets.0.insert(
            "cron".into(),
            ContextToolsetEntry {
                disabled: vec!["shell".into()],
            },
        );
        let p = a.widen_with(&b);
        assert!(p.permissions.can_use_shell);
        assert_eq!(p.resource_limits.memory_mb, 1024);
        assert_eq!(p.budget.max_tokens_per_day, 0);
        assert_eq!(p.allowed_models, vec!["m1".to_string(), "m2".to_string()]);
        assert_eq!(
            p.context_toolsets.0["cron"].disabled,
            vec!["shell".to_string()]
        );
    }

    #[test]
    fn clamp_keeps_narrower_override_values() {
        let cap = Policy::default();
        let mut own = Policy::default();
        own.permissions.can_use_shell = false;
        own.resource_limits.memory_mb = 128;
        own.resource_limits.cpu_cores = 0.5;
        own.allowed_models = vec!["claude-opus-5-5".into()];
        own.budget.on_exhaust = BudgetExhaustAction::Fail;
        let p = own.clamp_to(&cap);
        assert!(!p.permissions.can_use_shell);
        assert_eq!(p.resource_limits.memory_mb, 128);
        assert!((p.resource_limits.cpu_cores - 0.5).abs() < f64::EPSILON);
        assert_eq!(p.allowed_models, vec!["claude-opus-5-5".to_string()]);
        assert_eq!(p.budget.on_exhaust, BudgetExhaustAction::Fail);
    }

    #[test]
    fn clamp_intersects_lists_and_unions_disabled_tools() {
        let mut cap = Policy::default();
        cap.allowed_models = vec!["a".into(), "b".into()];
        cap.permissions.can_bind_channels = ChannelAllowance::Allowed(vec!["webchat".into()]);
        cap.context_toolsets.0.insert(
            "cron".into(),
            ContextToolsetEntry {
                disabled: vec!["shell".into()],
            },
        );
        let mut own = Policy::default();
        own.allowed_models = vec!["*".into()];
        own.context_toolsets.0.insert(
            "cron".into(),
            ContextToolsetEntry {
                disabled: vec!["web_fetch".into()],
            },
        );
        let p = own.clamp_to(&cap);
        assert_eq!(p.allowed_models, vec!["a".to_string(), "b".to_string()]);
        assert!(matches!(
            p.permissions.can_bind_channels,
            ChannelAllowance::Allowed(ref c) if c == &vec!["webchat".to_string()]
        ));
        let disabled = &p.context_toolsets.0["cron"].disabled;
        assert!(disabled.contains(&"shell".to_string()));
        assert!(disabled.contains(&"web_fetch".to_string()));
    }

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
    fn policy_without_hooks_deserialises() {
        let raw = serde_json::json!({
            "max_agents": 3,
            "permissions": { "can_use_shell": false },
        });
        let p: Policy = serde_json::from_value(raw).expect("parse");
        assert_eq!(p.max_agents, 3);
        assert!(p.hooks.pre_tool_use.is_empty());
    }

    #[test]
    fn policy_hooks_round_trip_and_reject_invalid_regex() {
        let raw = serde_json::json!({
            "hooks": { "pre_tool_use": [
                { "tool": "shell", "deny": ["^rm -rf", "^sudo "] },
                { "tool": "web_fetch" },
            ]},
        });
        let p: Policy = serde_json::from_value(raw).expect("parse");
        assert_eq!(p.hooks.pre_tool_use.len(), 2);
        assert!(p.hooks.pre_tool_use[1].deny.is_empty());
        let back: Policy =
            serde_json::from_str(&serde_json::to_string(&p).expect("serialize")).expect("parse");
        assert_eq!(back.hooks, p.hooks);

        p.validate().expect("valid");

        let bad = serde_json::json!({
            "hooks": { "pre_tool_use": [{ "tool": "shell", "deny": ["[unclosed"] }] },
        });
        let parsed: Policy = serde_json::from_value(bad).expect("deserialise stays cheap");
        assert!(parsed.validate().is_err());
    }

    #[test]
    fn hook_typos_are_rejected() {
        for raw in [
            serde_json::json!({ "hooks": { "pre_tool_uses": [] } }),
            serde_json::json!({ "hooks": { "pre_tool_use": [{ "tool": "shell", "denny": ["x"] }] } }),
        ] {
            assert!(serde_json::from_value::<Policy>(raw).is_err());
        }
        assert!(hooks_with(" ", &["x"]).validate().is_err());
    }

    #[test]
    fn hook_validation_enforces_limits() {
        let rule = |n: usize| ToolHookRule {
            tool: "shell".into(),
            deny: (0..n).map(|i| format!("^p{i}")).collect(),
        };
        let too_many_rules = HookPolicy {
            pre_tool_use: (0..=MAX_HOOK_RULES).map(|_| rule(1)).collect(),
        };
        assert!(too_many_rules.validate().is_err());
        let too_many_patterns = HookPolicy {
            pre_tool_use: vec![rule(MAX_HOOK_PATTERNS + 1)],
        };
        assert!(too_many_patterns.validate().is_err());
        let at_limit = HookPolicy {
            pre_tool_use: vec![rule(MAX_HOOK_PATTERNS)],
        };
        at_limit.validate().expect("at the limit");

        let long = "a".repeat(MAX_HOOK_PATTERN_LEN + 1);
        assert!(hooks_with("shell", &[&long]).validate().is_err());
        // Short, but compiles to a program far over the size limit.
        assert!(
            hooks_with("shell", &["(\\w{100}){100}"])
                .validate()
                .is_err()
        );
    }

    fn hooks_with(tool: &str, deny: &[&str]) -> HookPolicy {
        HookPolicy {
            pre_tool_use: vec![ToolHookRule {
                tool: tool.into(),
                deny: deny.iter().map(|p| (*p).to_string()).collect(),
            }],
        }
    }

    #[test]
    fn clamp_and_widen_keep_every_hook_rule() {
        let mut cap = Policy::default();
        cap.hooks = hooks_with("shell", &["^rm -rf", "^sudo "]);
        let mut own = Policy::default();
        own.hooks = hooks_with("shell", &["^sudo ", "kubectl delete"]);

        for p in [own.clamp_to(&cap), own.widen_with(&cap)] {
            let patterns: Vec<&str> = p
                .hooks
                .pre_tool_use
                .iter()
                .filter(|r| r.tool == "shell")
                .flat_map(|r| r.deny.iter().map(String::as_str))
                .collect();
            assert_eq!(patterns, vec!["^sudo ", "kubectl delete", "^rm -rf"]);
        }
        assert!(
            Policy::default()
                .clamp_to(&cap)
                .hooks
                .denies("shell", "^rm -rf")
        );
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
