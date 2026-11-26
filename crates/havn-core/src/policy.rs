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
    /// Chats titled by a model (issue #179).
    pub chat_titles: ChatTitles,
    /// Tool rounds a message may take (issue #219).
    pub tool_rounds: ToolRounds,
    /// How long shell commands may run, and how many in the background
    /// (issue #215).
    pub shell: ShellLimits,
}

/// How long shell commands may run, and how many in the background (issue
/// #215). A limit of 0 counts as 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellLimits {
    /// Seconds a command runs when it does not say.
    pub timeout_seconds: u32,
    /// Seconds a command may ask for at most.
    pub max_timeout_seconds: u32,
    /// Commands running in the background at once.
    pub background: u32,
}

impl Default for ShellLimits {
    fn default() -> Self {
        Self {
            timeout_seconds: 300,
            max_timeout_seconds: 1800,
            background: 8,
        }
    }
}

/// Tool rounds one message may take (issue #219): the model calling tools
/// and reading what they return, before it must answer. At the limit it
/// sums up what it did and what is left. A limit of 0 counts as 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolRounds {
    /// A message to the agent.
    pub max: u32,
    /// A subagent's task, another agent's question, a heartbeat or a cron
    /// job: work nobody waits on.
    pub subagent: u32,
}

impl Default for ToolRounds {
    fn default() -> Self {
        Self {
            max: 100,
            subagent: 30,
        }
    }
}

/// Whether a model writes each new chat's title, and which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatTitles {
    /// A model titles each new chat from its first message. Off, a chat
    /// is titled after the start of its first message.
    pub enabled: bool,
    /// The model that writes them, in place of the gateway's
    /// `[llm] title_model`; empty for that.
    pub model: String,
}

impl Default for ChatTitles {
    fn default() -> Self {
        Self {
            enabled: true,
            model: String::new(),
        }
    }
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
            chat_titles: ChatTitles::default(),
            tool_rounds: ToolRounds::default(),
            shell: ShellLimits::default(),
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

/// Rule for one tool: the call is refused when any `deny` regex matches
/// its flattened input, and otherwise waits for the user's approval when
/// any `ask` regex does (`(?s).*` asks for every call). `tool` is a tool
/// name, or a prefix ending in `*` such as `mcp__github__*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHookRule {
    pub tool: String,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
}

/// The `ask` pattern that matches every call.
pub const ASK_EVERY_CALL: &str = "(?s).*";

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
        for model in &self.allowed_models {
            let named = model
                .split_once(':')
                .is_some_and(|(provider, name)| !provider.is_empty() && !name.is_empty());
            if model != "*" && !named {
                return Err(crate::Error::InvalidPolicy(format!(
                    "allowed_models entry {model:?} must be \"*\" or provider:model"
                )));
            }
        }
        validate_domains(
            "network_policy.allowed_domains",
            &self.network_policy.allowed_domains,
        )?;
        validate_domains(
            "network_policy.blocked_domains",
            &self.network_policy.blocked_domains,
        )?;
        for (server, config) in &self.mcp_servers {
            config.validate(server)?;
        }
        self.hooks.validate()
    }
}

/// Most entries in one domain list.
pub const MAX_DOMAINS: usize = 500;

/// Checks a domain list: at most [`MAX_DOMAINS`] entries, each an ASCII
/// host name of two labels or more (letters, digits, `-`, `.`, optionally
/// after `*.`), without scheme, port or path.
pub fn validate_domains(what: &str, domains: &[String]) -> crate::Result<()> {
    if domains.len() > MAX_DOMAINS {
        return Err(crate::Error::InvalidPolicy(format!(
            "{what}: more than {MAX_DOMAINS} entries"
        )));
    }
    for entry in domains {
        // `*` alone is what roles seeded before 0.2.0 hold, from when the
        // list was a ceiling; it matches nothing now.
        if entry == "*" {
            continue;
        }
        let name = entry.strip_prefix("*.").unwrap_or(entry);
        let labels: Vec<&str> = name.split('.').collect();
        // A single label (`com`) would cover a whole top-level domain, and
        // an all-digit last label is an IP address, not a domain.
        let valid = labels.len() >= 2
            && name.len() <= 253
            && labels.iter().all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
            && labels
                .last()
                .is_some_and(|tld| !tld.chars().all(|c| c.is_ascii_digit()));
        if !valid {
            return Err(crate::Error::InvalidPolicy(format!(
                "{what}: {entry:?} is not a domain such as example.com or *.example.com"
            )));
        }
    }
    Ok(())
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
        let patterns: usize = self
            .pre_tool_use
            .iter()
            .map(|r| r.deny.len() + r.ask.len())
            .sum();
        if patterns > MAX_HOOK_PATTERNS {
            return invalid(format!(
                "hooks: more than {MAX_HOOK_PATTERNS} deny and ask patterns"
            ));
        }
        for rule in &self.pre_tool_use {
            if rule.tool.trim().is_empty() {
                return invalid("hooks: a pre_tool_use rule has an empty tool".into());
            }
            for pattern in rule.deny.iter().chain(&rule.ask) {
                if pattern.len() > MAX_HOOK_PATTERN_LEN {
                    return invalid(format!(
                        "hooks: a {} pattern is longer than {MAX_HOOK_PATTERN_LEN} bytes",
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
            let missing = |patterns: &[String], ask: bool| -> Vec<String> {
                patterns
                    .iter()
                    .filter(|p| !merged.has(&rule.tool, p, ask))
                    .cloned()
                    .collect()
            };
            let (deny, ask) = (missing(&rule.deny, false), missing(&rule.ask, true));
            if !deny.is_empty() || !ask.is_empty() {
                merged.pre_tool_use.push(ToolHookRule {
                    tool: rule.tool.clone(),
                    deny,
                    ask,
                });
            }
        }
        merged
    }

    fn has(&self, tool: &str, pattern: &str, ask: bool) -> bool {
        self.pre_tool_use.iter().any(|r| {
            r.tool == tool
                && if ask { &r.ask } else { &r.deny }
                    .iter()
                    .any(|p| p == pattern)
        })
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

/// Parent of every agent cgroup, directly below the cgroup v2 root. It has
/// no unit suffix (`.slice`, `.service`, `.scope`), so systemd never takes
/// it for one of its units.
pub const AGENT_CGROUP_PARENT: &str = "havn";

/// Name prefix of the per-agent cgroups inside [`AGENT_CGROUP_PARENT`].
pub const AGENT_CGROUP_PREFIX: &str = "agent-";

/// An agent's cgroup relative to the cgroup root, as `/proc/<pid>/cgroup`
/// shows it: `/havn/agent-<id>`.
pub fn agent_cgroup(agent_id: impl std::fmt::Display) -> String {
    format!("/{AGENT_CGROUP_PARENT}/{AGENT_CGROUP_PREFIX}{agent_id}")
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
    /// `web_search`, run by the gateway with the operator's search backend.
    /// Needs no egress: the agent sees results, never the network. Queries
    /// leave the host, so turn it off for agents whose data must not.
    pub can_search_web: bool,
    /// `web_fetch`, run by the gateway: it reads a page and answers a
    /// question about it. Needs no egress. The URL leaves the host, so the
    /// gateway reads only URLs users sent, search results and links on
    /// pages it read, or `network_policy.allowed_domains`.
    pub can_fetch_web: bool,
    /// `transcribe`, run by the gateway with `[transcription]`: an audio
    /// file of the agent's workspace goes to the transcription endpoint or
    /// model, on the owner's budget (issue #230). Unset, as in a policy
    /// written before it, it is `can_fetch_web`'s: see
    /// [`Permissions::transcribes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_transcribe: Option<bool>,
}

impl Permissions {
    /// Whether `transcribe` is allowed: `can_transcribe`, or, unset, what
    /// `can_fetch_web` says, so a policy that keeps files from leaving the
    /// host through web_fetch keeps them from the transcription service too.
    #[must_use]
    pub fn transcribes(&self) -> bool {
        self.can_transcribe.unwrap_or(self.can_fetch_web)
    }
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
            can_search_web: true,
            can_fetch_web: true,
            can_transcribe: None,
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
    /// With `egress_allowed`, the sandbox reaches the internet only
    /// through the gateway's egress proxy: allowed domains go through, a
    /// new one waits for the owner's approval, blocked ones never do.
    /// Without it, egress reaches every public address.
    pub egress_proxy: bool,
    /// Domains the agent may reach without further checks. An entry
    /// matches the domain and its subdomains; a leading `*.` is ignored.
    pub allowed_domains: Vec<String>,
    /// Domains the agent may never reach, matched like `allowed_domains`.
    /// They win over `allowed_domains`.
    pub blocked_domains: Vec<String>,
}

impl NetworkPolicy {
    #[must_use]
    pub fn is_blocked(&self, host: &str) -> bool {
        domain_listed(&self.blocked_domains, host)
    }

    /// `host` is on `allowed_domains` and not blocked.
    #[must_use]
    pub fn is_allowed(&self, host: &str) -> bool {
        !self.is_blocked(host) && domain_listed(&self.allowed_domains, host)
    }
}

/// Whether `host` is one of `domains` or a subdomain of one. Case and a
/// trailing dot do not matter; a leading `*.` on an entry is ignored.
#[must_use]
pub fn domain_listed(domains: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    domains.iter().any(|entry| {
        let entry = entry.trim();
        let entry = entry.strip_prefix("*.").unwrap_or(entry);
        let entry = entry.trim_end_matches('.').to_ascii_lowercase();
        !entry.is_empty()
            && (host == entry
                || host
                    .strip_suffix(entry.as_str())
                    .is_some_and(|rest| rest.ends_with('.')))
    })
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
    /// The server's environment. A value `secret:<name>` is the agent's
    /// secret of that name, set by its owner and resolved when the server
    /// starts (issue #233); without it the server does not start.
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
    /// A remote server's URL (issue #234), in place of `binary`: the
    /// runtime connects to it through the agent's network egress.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// How a remote server is spoken to.
    #[serde(skip_serializing_if = "McpTransport::is_auto")]
    pub transport: McpTransport,
    /// Headers sent to a remote server. A value `secret:<name>` is the
    /// agent's secret of that name.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// How a remote server is authorized.
    #[serde(skip_serializing_if = "McpAuth::is_none")]
    pub auth: McpAuth,
}

/// How a remote MCP server is spoken to: Streamable HTTP, the older
/// HTTP+SSE transport, or Streamable HTTP falling back to HTTP+SSE.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    #[default]
    Auto,
    StreamableHttp,
    Sse,
}

impl McpTransport {
    #[must_use]
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

/// How a remote MCP server is authorized.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpAuth {
    #[default]
    None,
    /// `Authorization: Bearer <token>`; the token is best `secret:<name>`.
    Bearer { token: String },
    /// OAuth 2.1 as MCP specifies it: the owner connects the server on the
    /// dashboard, and the gateway keeps and refreshes its tokens.
    Oauth(McpOAuthConfig),
}

impl McpAuth {
    #[must_use]
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
}

/// A remote MCP server's OAuth settings. Without a `client_id` the gateway
/// registers itself with the authorization server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpOAuthConfig {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    /// The client's secret, best `secret:<name>`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_secret: String,
    /// The scopes asked for; without them, those the server names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

/// Headers of a remote MCP server, at most.
pub const MAX_MCP_HEADERS: usize = 32;
/// Bytes of a remote MCP server's URL, or of one of its header values.
pub const MAX_MCP_VALUE_BYTES: usize = 4096;

/// Headers the runtime sets itself for a remote MCP server.
const RESERVED_MCP_HEADERS: [&str; 13] = [
    "accept",
    "authorization",
    "connection",
    "content-length",
    "content-type",
    "expect",
    "host",
    "keep-alive",
    "last-event-id",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Checks a value that may be `secret:<name>`: then it must name one of the
/// agent's own secrets, as the runtime resolves it.
fn check_secret_value(what: &str, value: &str) -> crate::Result<()> {
    use crate::secret_ref::{self, SecretRef};
    if !value.starts_with(secret_ref::PREFIX) {
        return Ok(());
    }
    match secret_ref::parse(value) {
        Ok(SecretRef::Agent { .. }) => Ok(()),
        Ok(SecretRef::Credential { .. }) => Err(crate::Error::InvalidPolicy(format!(
            "{what}: {value:?} names a gateway credential; \
             an MCP server gets only the agent's own secrets, as `secret:<name>`"
        ))),
        Err(e) => Err(crate::Error::InvalidPolicy(format!("{what}: {e}"))),
    }
}

impl McpServerConfig {
    /// Checks the entry: a stdio server (`binary`) or a remote one (`url`),
    /// not both, and that each value starting with `secret:` names one of
    /// the agent's own secrets (`secret:<name>`), as the runtime resolves it.
    pub fn validate(&self, server: &str) -> crate::Result<()> {
        let invalid = |why: String| {
            Err(crate::Error::InvalidPolicy(format!(
                "mcp_servers.{server}: {why}"
            )))
        };
        for (var, value) in &self.env {
            check_secret_value(&format!("mcp_servers.{server}.env.{var}"), value)?;
        }
        if !self.is_remote() {
            if self.transport != McpTransport::Auto
                || !self.headers.is_empty()
                || !self.auth.is_none()
            {
                return invalid("transport, headers and auth are for a server with a `url`".into());
            }
            return Ok(());
        }
        if !self.binary.is_empty()
            || !self.args.is_empty()
            || !self.env.is_empty()
            || !self.extra_paths_rw.is_empty()
            || !self.extra_paths_ro.is_empty()
        {
            return invalid(
                "a server with a `url` is remote: it takes no binary, args, env or paths".into(),
            );
        }
        if let Err(why) = remote_url(&self.url) {
            return invalid(why);
        }
        if self.headers.len() > MAX_MCP_HEADERS {
            return invalid(format!("more than {MAX_MCP_HEADERS} headers"));
        }
        for (name, value) in &self.headers {
            let lower = name.to_ascii_lowercase();
            let token = !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
            if !token {
                return invalid(format!("{name:?} is not a header name"));
            }
            if RESERVED_MCP_HEADERS.contains(&lower.as_str())
                || lower.starts_with("mcp-")
                || lower.starts_with("proxy-")
            {
                return invalid(format!(
                    "header {name} is set by Havn (use `auth` for Authorization)"
                ));
            }
            check_header_value(&format!("mcp_servers.{server}.headers.{name}"), value)?;
        }
        match &self.auth {
            McpAuth::None => {}
            McpAuth::Bearer { token } => {
                if token.trim().is_empty() {
                    return invalid("auth.token is empty".into());
                }
                check_header_value(&format!("mcp_servers.{server}.auth.token"), token)?;
            }
            McpAuth::Oauth(oauth) => {
                if !self.url.starts_with("https://") {
                    return invalid("OAuth needs an https URL".into());
                }
                if !oauth.client_secret.is_empty() && oauth.client_id.is_empty() {
                    return invalid("auth.client_secret needs a client_id".into());
                }
                check_header_value(
                    &format!("mcp_servers.{server}.auth.client_id"),
                    &oauth.client_id,
                )?;
                check_header_value(
                    &format!("mcp_servers.{server}.auth.client_secret"),
                    &oauth.client_secret,
                )?;
                if oauth.scopes.len() > 64
                    || oauth.scopes.iter().any(|s| {
                        s.is_empty()
                            || s.len() > 256
                            || s.bytes()
                                .any(|b| !b.is_ascii_graphic() || b == b'"' || b == b'\\')
                    })
                {
                    return invalid(
                        "auth.scopes: each a word of printable ASCII, at most 64".into(),
                    );
                }
            }
        }
        Ok(())
    }

    /// A remote server: one with a `url`.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        !self.url.is_empty()
    }

    /// The host of a remote server's URL, in lowercase.
    #[must_use]
    pub fn remote_host(&self) -> Option<String> {
        remote_url(&self.url).ok().map(|(_, host)| host)
    }

    /// Whether `self` runs as `other`: the same binary, arguments,
    /// environment and paths, or the same URL, transport, headers and
    /// authorization (whether each is enabled, and its timeout, aside).
    #[must_use]
    pub fn runs_as(&self, other: &McpServerConfig) -> bool {
        self.binary == other.binary
            && self.args == other.args
            && self.env == other.env
            && self.extra_paths_rw == other.extra_paths_rw
            && self.extra_paths_ro == other.extra_paths_ro
            && self.url == other.url
            && self.transport == other.transport
            && self.headers == other.headers
            && self.auth == other.auth
    }

    /// Whether it is given any of the agent's secrets: a `secret:` reference
    /// in its `env`, headers or authorization, or OAuth tokens.
    #[must_use]
    pub fn uses_secrets(&self) -> bool {
        let reference = |v: &String| v.starts_with(crate::secret_ref::PREFIX);
        self.env.values().any(reference)
            || self.headers.values().any(reference)
            || match &self.auth {
                McpAuth::None => false,
                McpAuth::Bearer { token } => reference(token),
                McpAuth::Oauth(_) => true,
            }
    }
}

/// Checks a header value of a remote server: printable, bounded, and a
/// reference of the agent's own when it is `secret:`.
fn check_header_value(what: &str, value: &str) -> crate::Result<()> {
    if value.len() > MAX_MCP_VALUE_BYTES || value.bytes().any(|b| b.is_ascii_control()) {
        return Err(crate::Error::InvalidPolicy(format!(
            "{what}: at most {MAX_MCP_VALUE_BYTES} bytes, without control characters"
        )));
    }
    check_secret_value(what, value)
}

/// The scheme and lowercase host of a remote MCP server's URL: http or
/// https, with a host name or address, without user name, password or
/// fragment.
pub fn remote_url(url: &str) -> Result<(String, String), String> {
    if url.len() > MAX_MCP_VALUE_BYTES {
        return Err(format!("url: at most {MAX_MCP_VALUE_BYTES} bytes"));
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("url {url:?}: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("url {url:?}: http or https only"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "url {url:?}: no user name or password in it (use headers or auth)"
        ));
    }
    if parsed.fragment().is_some() {
        return Err(format!("url {url:?}: no #fragment"));
    }
    let host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("url {url:?}: no host"))?;
    Ok((
        parsed.scheme().to_string(),
        host.trim_end_matches('.').to_ascii_lowercase(),
    ))
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
            url: String::new(),
            transport: McpTransport::Auto,
            headers: HashMap::new(),
            auth: McpAuth::None,
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
                can_search_web: own.can_search_web && cap.can_search_web,
                can_fetch_web: own.can_fetch_web && cap.can_fetch_web,
                can_transcribe: Some(own.transcribes() && cap.transcribes()),
            },
            network_policy: NetworkPolicy {
                egress_allowed: self.network_policy.egress_allowed
                    && ceiling.network_policy.egress_allowed,
                // A ceiling that sends egress through the proxy keeps it
                // there.
                egress_proxy: self.network_policy.egress_proxy
                    || ceiling.network_policy.egress_proxy,
                // Both lists only add: allowed domains are trust the agent
                // may add, blocked domains are restrictions it may add.
                allowed_domains: union(
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
            // Off where the ceiling has them off; its model, when it names
            // one, is the one used.
            chat_titles: ChatTitles {
                enabled: self.chat_titles.enabled && ceiling.chat_titles.enabled,
                model: if ceiling.chat_titles.model.is_empty() {
                    self.chat_titles.model.clone()
                } else {
                    ceiling.chat_titles.model.clone()
                },
            },
            tool_rounds: ToolRounds {
                max: self.tool_rounds.max.min(ceiling.tool_rounds.max),
                subagent: self.tool_rounds.subagent.min(ceiling.tool_rounds.subagent),
            },
            shell: ShellLimits {
                timeout_seconds: self
                    .shell
                    .timeout_seconds
                    .min(ceiling.shell.timeout_seconds),
                max_timeout_seconds: self
                    .shell
                    .max_timeout_seconds
                    .min(ceiling.shell.max_timeout_seconds),
                background: self.shell.background.min(ceiling.shell.background),
            },
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
                can_search_web: a.can_search_web || b.can_search_web,
                can_fetch_web: a.can_fetch_web || b.can_fetch_web,
                can_transcribe: Some(a.transcribes() || b.transcribes()),
            },
            network_policy: NetworkPolicy {
                egress_allowed: self.network_policy.egress_allowed
                    || other.network_policy.egress_allowed,
                // Only a role that allows egress says how it goes.
                egress_proxy: match (
                    a.can_access_network && self.network_policy.egress_allowed,
                    b.can_access_network && other.network_policy.egress_allowed,
                ) {
                    (true, true) => {
                        self.network_policy.egress_proxy && other.network_policy.egress_proxy
                    }
                    (true, false) => self.network_policy.egress_proxy,
                    (false, true) => other.network_policy.egress_proxy,
                    (false, false) => {
                        self.network_policy.egress_proxy || other.network_policy.egress_proxy
                    }
                },
                // Every role's blocked domains hold, as its hook rules do:
                // a second role does not lift an admin's block.
                allowed_domains: union(
                    &self.network_policy.allowed_domains,
                    &other.network_policy.allowed_domains,
                ),
                blocked_domains: union(
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
            chat_titles: ChatTitles {
                enabled: self.chat_titles.enabled || other.chat_titles.enabled,
                // The model of a role that has them on comes first.
                model: [&self.chat_titles, &other.chat_titles]
                    .into_iter()
                    .filter(|t| !t.model.is_empty())
                    .min_by_key(|t| !t.enabled)
                    .map(|t| t.model.clone())
                    .unwrap_or_default(),
            },
            tool_rounds: ToolRounds {
                max: self.tool_rounds.max.max(other.tool_rounds.max),
                subagent: self.tool_rounds.subagent.max(other.tool_rounds.subagent),
            },
            shell: ShellLimits {
                timeout_seconds: self.shell.timeout_seconds.max(other.shell.timeout_seconds),
                max_timeout_seconds: self
                    .shell
                    .max_timeout_seconds
                    .max(other.shell.max_timeout_seconds),
                background: self.shell.background.max(other.shell.background),
            },
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

fn narrow_channels(own: &ChannelAllowance, cap: &ChannelAllowance) -> ChannelAllowance {
    match (own, cap) {
        (ChannelAllowance::All, other) | (other, ChannelAllowance::All) => other.clone(),
        (ChannelAllowance::Allowed(a), ChannelAllowance::Allowed(b)) => {
            ChannelAllowance::Allowed(a.iter().filter(|c| b.contains(c)).cloned().collect())
        }
    }
}

/// `a` followed by the entries of `b` it lacks.
#[must_use]
pub fn union(a: &[String], b: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = a.iter().map(String::as_str).collect();
    let mut out = a.to_vec();
    out.extend(b.iter().filter(|x| seen.insert(x.as_str())).cloned());
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
    fn tool_rounds_default_narrow_and_widen() {
        let policy: Policy = serde_json::from_str("{}").expect("empty policy");
        assert_eq!(
            policy.tool_rounds,
            ToolRounds {
                max: 100,
                subagent: 30
            }
        );
        let mut tight = Policy::default();
        tight.tool_rounds = ToolRounds {
            max: 40,
            subagent: 50,
        };
        let clamped = Policy::default().clamp_to(&tight);
        assert_eq!(
            clamped.tool_rounds,
            ToolRounds {
                max: 40,
                subagent: 30
            }
        );
        let widened = tight.widen_with(&Policy::default());
        assert_eq!(
            widened.tool_rounds,
            ToolRounds {
                max: 100,
                subagent: 50
            }
        );
    }

    #[test]
    fn shell_limits_default_and_narrow() {
        let policy: Policy = serde_json::from_str("{}").expect("empty policy");
        assert_eq!(policy.shell, ShellLimits::default());
        let mut tight = Policy::default();
        tight.shell.max_timeout_seconds = 600;
        tight.shell.background = 2;
        let clamped = Policy::default().clamp_to(&tight);
        assert_eq!(clamped.shell.max_timeout_seconds, 600);
        assert_eq!(clamped.shell.background, 2);
        assert_eq!(clamped.shell.timeout_seconds, 300);
    }

    #[test]
    fn chat_titles_are_off_where_the_ceiling_has_them_off() {
        let mut team = Policy::default();
        team.chat_titles.enabled = false;
        team.chat_titles.model = "openai:small".into();
        let mut agent = Policy::default();
        agent.chat_titles.model = "openai:big".into();
        let clamped = agent.clamp_to(&team);
        assert!(!clamped.chat_titles.enabled);
        assert_eq!(clamped.chat_titles.model, "openai:small");
        let open = agent.clamp_to(&Policy::default());
        assert!(open.chat_titles.enabled);
        assert_eq!(open.chat_titles.model, "openai:big");
        // Across roles, on wins, and so does the model of a role with them
        // on.
        let widened = team.widen_with(&Policy::default());
        assert!(widened.chat_titles.enabled);
        assert_eq!(widened.chat_titles.model, "openai:small");
        let mut on = Policy::default();
        on.chat_titles.model = "openai:on".into();
        assert_eq!(team.widen_with(&on).chat_titles.model, "openai:on");
        assert_eq!(on.widen_with(&team).chat_titles.model, "openai:on");
        // Both on: the first role's.
        let mut also = Policy::default();
        also.chat_titles.model = "openai:also".into();
        assert_eq!(on.widen_with(&also).chat_titles.model, "openai:on");
    }

    #[test]
    fn domain_lists_only_add_when_merged() {
        let mut role = Policy::default();
        role.network_policy.allowed_domains = vec!["corp.example".into()];
        role.network_policy.blocked_domains = vec!["evil.example".into()];
        let mut agent = Policy::default();
        agent.network_policy.allowed_domains = vec!["docs.rs".into()];
        agent.network_policy.blocked_domains = vec!["ads.example".into()];
        let p = agent.clamp_to(&role);
        assert_eq!(
            p.network_policy.allowed_domains,
            ["docs.rs", "corp.example"]
        );
        assert_eq!(
            p.network_policy.blocked_domains,
            ["ads.example", "evil.example"]
        );

        let other_role = Policy::default();
        let both = role.widen_with(&other_role);
        assert_eq!(
            both.network_policy.blocked_domains,
            ["evil.example"],
            "a block holds"
        );
        assert_eq!(both.network_policy.allowed_domains, ["corp.example"]);
    }

    #[test]
    fn domain_entries_are_validated() {
        let mut p = Policy::default();
        p.network_policy.allowed_domains = vec![
            "example.com".into(),
            "*.docs.rs".into(),
            "*".into(),
            "x-1.io".into(),
        ];
        assert!(p.validate().is_ok(), "the legacy * is still accepted");
        for bad in [
            "https://example.com",
            "example.com/path",
            "a..b",
            "",
            "com",
            "*.com",
            "ex ample.com",
            "-x.com",
            "a.-b.com",
            "a-.com",
            "1.2.3.4",
            "0.1",
            "例子.com",
        ] {
            p.network_policy.blocked_domains = vec![bad.into()];
            assert!(p.validate().is_err(), "{bad:?} accepted");
        }
        p.network_policy.blocked_domains = vec!["x.com".into(); MAX_DOMAINS + 1];
        assert!(p.validate().is_err());
    }

    #[test]
    fn domains_match_themselves_and_their_subdomains() {
        let list = vec!["Example.com".to_string(), "*.docs.rs".to_string()];
        assert!(domain_listed(&list, "example.com"));
        assert!(domain_listed(&list, "www.EXAMPLE.com."));
        assert!(domain_listed(&list, "docs.rs"));
        assert!(domain_listed(&list, "a.b.docs.rs"));
        assert!(!domain_listed(&list, "notexample.com"));
        assert!(!domain_listed(&list, "example.com.evil.net"));
        assert!(!domain_listed(&[String::new(), " ".into()], "example.com"));
        let net = NetworkPolicy {
            egress_allowed: false,
            egress_proxy: false,
            allowed_domains: vec!["example.com".into()],
            blocked_domains: vec!["secret.example.com".into()],
        };
        assert!(net.is_allowed("www.example.com"));
        assert!(!net.is_allowed("secret.example.com"));
        assert!(net.is_blocked("a.secret.example.com"));
        assert!(!net.is_allowed("other.org"));
    }

    #[test]
    fn transcribing_follows_reading_the_web_unless_set() {
        let read = |json: &str| -> Permissions { serde_json::from_str(json).expect("permissions") };
        assert!(read("{}").transcribes());
        assert!(!read(r#"{"can_fetch_web": false}"#).transcribes());
        assert!(read(r#"{"can_fetch_web": false, "can_transcribe": true}"#).transcribes());
        assert!(!read(r#"{"can_transcribe": false}"#).transcribes());
        let unset = serde_json::to_value(Permissions::default()).expect("json");
        assert!(unset.get("can_transcribe").is_none(), "{unset}");
    }

    #[test]
    fn clamp_never_widens_permissions_or_limits() {
        let mut cap = Policy::default();
        cap.permissions.can_use_shell = false;
        cap.permissions.can_use_mcp = false;
        cap.permissions.can_search_web = false;
        cap.permissions.can_fetch_web = false;
        cap.permissions.can_transcribe = Some(false);
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
        assert!(!p.permissions.can_search_web);
        assert!(!p.permissions.can_fetch_web);
        assert!(!p.permissions.transcribes());
        assert!(!p.network_policy.egress_allowed);
        assert_eq!(p.resource_limits.memory_mb, 256);
        assert_eq!(p.resource_limits.pids_max, 32);
        assert_eq!(p.budget.max_tokens_per_day, 10_000);
    }

    #[test]
    fn widen_takes_the_egress_route_of_roles_that_allow_egress() {
        let role = |allowed: bool, proxy: bool| {
            let mut p = Policy::default();
            p.network_policy.egress_allowed = allowed;
            p.network_policy.egress_proxy = proxy;
            p
        };
        let route = |a: &Policy, b: &Policy| {
            let p = a.widen_with(b);
            (
                p.network_policy.egress_allowed,
                p.network_policy.egress_proxy,
            )
        };
        // A role without egress does not lift another's proxy.
        assert_eq!(route(&role(true, true), &role(false, false)), (true, true));
        assert_eq!(route(&role(false, false), &role(true, true)), (true, true));
        assert_eq!(route(&role(true, false), &role(false, true)), (true, false));
        assert_eq!(route(&role(true, true), &role(true, false)), (true, false));
        assert_eq!(route(&role(true, true), &role(true, true)), (true, true));
        assert_eq!(
            route(&role(false, true), &role(false, false)),
            (false, true)
        );
        // Nor does one without network access.
        let mut offline = role(true, false);
        offline.permissions.can_access_network = false;
        assert_eq!(route(&offline, &role(true, true)), (true, true));
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
            ask: Vec::new(),
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
                ask: Vec::new(),
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
                .has("shell", "^rm -rf", false)
        );
    }

    #[test]
    fn ask_rules_merge_validate_and_match_prefixes() {
        let ask = |tool: &str, p: &str| HookPolicy {
            pre_tool_use: vec![ToolHookRule {
                tool: tool.into(),
                deny: Vec::new(),
                ask: vec![p.into()],
            }],
        };
        let merged = ask("mcp__github__*", ASK_EVERY_CALL).union(&ask("shell", "deploy"));
        assert_eq!(merged.pre_tool_use.len(), 2);
        assert!(merged.has("shell", "deploy", true));
        assert!(!merged.has("shell", "deploy", false));
        assert_eq!(merged.union(&ask("shell", "deploy")), merged, "no repeats");
        assert!(merged.validate().is_ok());
        assert!(ask("shell", "(").validate().is_err());
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

    #[test]
    fn agent_cgroup_is_outside_any_systemd_unit_name() {
        assert_eq!(agent_cgroup("abc"), "/havn/agent-abc");
        for suffix in [".slice", ".service", ".scope"] {
            assert!(!AGENT_CGROUP_PARENT.ends_with(suffix));
        }
    }

    #[test]
    fn allowed_models_need_a_provider() {
        let mut p = Policy::default();
        p.allowed_models = vec!["*".into(), "openrouter:deepseek/deepseek-r1:free".into()];
        assert!(p.validate().is_ok());
        for bad in ["claude-opus-4-7", ":claude", "anthropic:"] {
            p.allowed_models = vec![bad.into()];
            assert!(p.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn mcp_env_refers_only_to_the_agents_own_secrets() {
        let mut p = Policy::default();
        let server = |value: &str| McpServerConfig {
            binary: "gh".into(),
            env: HashMap::from([("TOKEN".to_string(), value.to_string())]),
            ..Default::default()
        };
        for ok in ["plain", "secret:github"] {
            p.mcp_servers = HashMap::from([("gh".to_string(), server(ok))]);
            assert!(p.validate().is_ok(), "{ok}");
        }
        for bad in ["secret:channel:telegram:bot", "secret:", "secret:-x"] {
            p.mcp_servers = HashMap::from([("gh".to_string(), server(bad))]);
            assert!(p.validate().is_err(), "{bad}");
        }
        assert!(server("secret:github").uses_secrets());
        assert!(server("secret:github").runs_as(&server("secret:github")));
        assert!(!server("secret:github").runs_as(&server("secret:gitlab")));
    }

    #[test]
    fn remote_mcp_servers_parse_and_are_checked() {
        let raw = serde_json::json!({
            "notion": {
                "url": "https://mcp.notion.com/mcp",
                "auth": {"type": "oauth"},
            },
            "linear": {
                "url": "https://mcp.linear.app/sse",
                "transport": "sse",
                "headers": {"X-Team": "core"},
                "auth": {"type": "bearer", "token": "secret:linear"},
            }
        });
        let parsed: HashMap<String, McpServerConfig> = serde_json::from_value(raw).expect("parse");
        let notion = &parsed["notion"];
        assert!(notion.is_remote());
        assert_eq!(notion.remote_host().as_deref(), Some("mcp.notion.com"));
        assert_eq!(notion.auth, McpAuth::Oauth(McpOAuthConfig::default()));
        assert!(notion.uses_secrets(), "OAuth tokens are the owner's");
        let linear = &parsed["linear"];
        assert_eq!(linear.transport, McpTransport::Sse);
        assert!(linear.uses_secrets());
        for (name, server) in &parsed {
            assert!(server.validate(name).is_ok(), "{name}");
        }
        // A stdio entry serializes without the remote keys.
        let stdio = serde_json::to_value(McpServerConfig {
            binary: "fs".into(),
            ..Default::default()
        })
        .expect("json");
        for key in ["url", "transport", "headers", "auth"] {
            assert!(stdio.get(key).is_none(), "{key}");
        }

        let remote = |edit: fn(&mut McpServerConfig)| {
            let mut server = McpServerConfig {
                url: "https://mcp.example.com/mcp".into(),
                ..Default::default()
            };
            edit(&mut server);
            server.validate("x")
        };
        assert!(remote(|_| {}).is_ok());
        assert!(remote(|s| s.url = "http://mcp.example.com/mcp".into()).is_ok());
        for bad in [
            (|s: &mut McpServerConfig| s.binary = "fs".into()) as fn(&mut McpServerConfig),
            |s| s.env = HashMap::from([("A".into(), "b".into())]),
            |s| s.url = "ftp://mcp.example.com".into(),
            |s| s.url = "https://user:pw@mcp.example.com".into(),
            |s| s.url = "https://mcp.example.com/#x".into(),
            |s| s.url = "not a url".into(),
            |s| s.headers = HashMap::from([("Authorization".into(), "Bearer x".into())]),
            |s| s.headers = HashMap::from([("Mcp-Session-Id".into(), "x".into())]),
            |s| s.headers = HashMap::from([("Bad Name".into(), "x".into())]),
            |s| s.headers = HashMap::from([("X-Key".into(), "a\nb".into())]),
            |s| s.headers = HashMap::from([("X-Key".into(), "secret:channel:x:y".into())]),
            |s| {
                s.auth = McpAuth::Bearer {
                    token: String::new(),
                }
            },
            |s| {
                s.url = "http://mcp.example.com/mcp".into();
                s.auth = McpAuth::Oauth(McpOAuthConfig::default());
            },
            |s| {
                s.auth = McpAuth::Oauth(McpOAuthConfig {
                    client_secret: "secret:x".into(),
                    ..Default::default()
                })
            },
        ] {
            assert!(remote(bad).is_err());
        }
        // Remote keys on a stdio entry.
        let stdio = McpServerConfig {
            binary: "fs".into(),
            headers: HashMap::from([("X-Key".into(), "v".into())]),
            ..Default::default()
        };
        assert!(stdio.validate("fs").is_err());
    }

    #[test]
    fn a_remote_server_runs_as_another_only_at_the_same_url_and_auth() {
        let server = |url: &str, token: &str| McpServerConfig {
            url: url.into(),
            auth: McpAuth::Bearer {
                token: token.into(),
            },
            ..Default::default()
        };
        let own = server("https://mcp.example.com/mcp", "secret:t");
        assert!(own.runs_as(&server("https://mcp.example.com/mcp", "secret:t")));
        assert!(!own.runs_as(&server("https://evil.example/mcp", "secret:t")));
        assert!(!own.runs_as(&server("https://mcp.example.com/mcp", "secret:u")));
        let mut headers = own.clone();
        headers.headers = HashMap::from([("X-Forward".into(), "evil".into())]);
        assert!(!own.runs_as(&headers));
    }
}
