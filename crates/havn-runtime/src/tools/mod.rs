//! Tool execution surface for the agent runtime (spec §9.2).
//!
//! Each [`Tool`] is a self-contained unit: name, JSON-schema input, async
//! `execute`. The [`ToolRegistry`] holds the standard set, filters by policy
//! at construction time, exposes their schemas to the LLM (Anthropic
//! `tools` field), and dispatches by name when the model returns a
//! `tool_use` block.
//!
//! Tools that require capabilities the agent doesn't have (e.g., `shell`
//! when `can_use_shell=false`) are absent from the registry — the LLM
//! never sees them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use havn_core::Policy;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::warn;

pub mod agent_query;
pub mod files;
pub mod memory;
pub mod network;
pub mod shell;
pub mod skill_manage;
pub mod subagent;

/// Stable execution-context names. Match the keys spec §6.2 uses in
/// `policy.context_toolsets` so the YAML/JSON the operator writes there
/// lines up one-to-one with the runtime's filter calls.
///
/// Inspired by Claude Code's per-mode tool sets (`ALL_AGENT_DISALLOWED_TOOLS`,
/// `ASYNC_AGENT_ALLOWED_TOOLS`): the registry stays static, but the *visible*
/// tool list per call is filtered by a context tag. That keeps the
/// schema-passed-to-LLM honest — the model never sees, and therefore can't
/// invoke, a tool the policy forbids in the current context.
pub mod context {
    pub const WEBCHAT: &str = "webchat";
    pub const HEARTBEAT: &str = "heartbeat";
    pub const CRON: &str = "cron";
    /// Used by the same-process `subagent_spawn` flow (spec §4.5). The
    /// child loop runs under this context tag so operators can surgically
    /// disable specific tools inside subagents via
    /// `policy.context_toolsets.subagent.disabled`.
    pub const SUBAGENT: &str = "subagent";
    /// Used by the cross-agent `IncomingQuery` flow (spec §4.4 v0.7).
    /// The temporary loop that answers another agent's query runs under
    /// this tag — operators can surgically restrict what an agent
    /// exposes when serving incoming queries via
    /// `policy.context_toolsets.agent_query.disabled`.
    pub const AGENT_QUERY: &str = "agent_query";
}

/// Per-call execution context shared across all tools in a single tool call.
#[derive(Clone)]
pub struct ToolCtx {
    /// Agent's writable workspace root. `file_read` / `file_write` are scoped here.
    pub workspace_dir: PathBuf,
    /// Per-agent `SQLite` pool for memory / conversation queries.
    pub agent_db: SqlitePool,
    /// Shared HTTP client for `web_fetch`.
    pub http: reqwest::Client,
    /// Active policy snapshot. Phase 1 only consults `permissions` at registry
    /// build time, but tools that need finer-grained checks (e.g. domain
    /// allowlist for `web_fetch`) read this directly.
    #[allow(
        dead_code,
        reason = "consumed by per-tool runtime checks in next vertical"
    )]
    pub policy: Arc<Policy>,
    /// Hybrid-retrieval embedding provider (spec §9.4 v0.7). `None`
    /// when the operator hasn't enabled embeddings — `memory_remember`
    /// then skips the vector write and `memory_search` falls back to
    /// FTS5-only.
    pub embedder: crate::embedding::EmbedderHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolResult {
    Ok(String),
    Error(String),
}

impl ToolResult {
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "used by tests + future runtime checks")
    )]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "used by tests + future runtime checks")
    )]
    pub fn text(&self) -> &str {
        match self {
            Self::Ok(s) | Self::Error(s) => s,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable string name used as the tool key in the registry +
    /// the JSON `name` field Anthropic sees. Returns `&str` (not
    /// `&'static str`) so dynamically-named tools — currently MCP
    /// adapters whose names come from `tools/list` at runtime — can
    /// hand back a borrow of their own field. Built-in tools that
    /// return string literals still satisfy this since `&'static str`
    /// is valid as `&str` for any shorter lifetime.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, ctx: &ToolCtx, input: serde_json::Value) -> ToolResult;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    /// Snapshot consulted at execute / schemas-for time for `context_toolsets`
    /// filtering. Held as `Arc` so cloning a registry is cheap (the runtime
    /// clones it into every per-message Ctx).
    policy: Arc<Policy>,
    /// PreToolUse hook set. `None` until the runtime wires one in via
    /// [`Self::with_hooks`]; on dispatch the registry consults
    /// `HookSet::veto` and refuses denied calls. Held as `arc_swap`'s
    /// `Hooks` so a future hot-reload (spec §8.4) can swap the rules
    /// without locking the dispatch path.
    hooks: Option<crate::hooks::Hooks>,
}

impl ToolRegistry {
    /// Build the standard tool set with two-layer policy enforcement (spec §6):
    ///
    /// 1. **Capability layer (here, build time)** — drop tools whose
    ///    underlying capability is forbidden outright (`can_use_shell`,
    ///    `can_access_network`). The LLM never sees these even with no
    ///    context filter.
    /// 2. **Context layer (per call, see [`Self::schemas_for`] /
    ///    [`Self::execute_for`])** — within an allowed capability set,
    ///    further hide tools based on `policy.context_toolsets[<ctx>].
    ///    disabled`. Lets a single agent expose `shell` in webchat but
    ///    not in cron, for example.
    ///
    /// This mirrors Claude Code's `filterToolsByDenyRules` (deny check
    /// happens at request time, not registry build time) — the registry
    /// stays static, the *visible* tool list per call is dynamic.
    pub fn standard(policy: &Arc<Policy>) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            // Always-on tools.
            Arc::new(memory::MemoryRememberTool),
            Arc::new(memory::MemorySearchTool),
            Arc::new(memory::MemoryForgetTool),
            Arc::new(files::FileReadTool),
            Arc::new(files::FileWriteTool),
        ];
        if policy.permissions.can_access_network {
            tools.push(Arc::new(network::WebFetchTool));
        }
        if policy.permissions.can_use_shell {
            tools.push(Arc::new(shell::ShellTool));
        }
        if policy.permissions.can_install_skills {
            tools.push(Arc::new(skill_manage::SkillManageTool));
        }
        Self {
            tools,
            policy: Arc::clone(policy),
            hooks: None,
        }
    }

    /// Append `subagent_spawn` to the registry. Called only by the parent
    /// runtime after constructing the [`subagent::Bridge`]; subagent loops
    /// reuse the parent's pre-`with_subagent` registry and therefore
    /// cannot recursively spawn (spec §4.5 rule 3).
    ///
    /// Gated by `policy.permissions.can_spawn_subagents` — if the policy
    /// denies the capability, this is a no-op.
    pub fn with_subagent(mut self, bridge: std::sync::Arc<subagent::Bridge>) -> Self {
        if !self.policy.permissions.can_spawn_subagents {
            return self;
        }
        self.tools
            .push(Arc::new(subagent::SubagentSpawnTool::new(bridge)));
        self
    }

    /// Append `agent_query` to the registry (spec §4.4 v0.7). The
    /// `IncomingQuery` handler intentionally builds its temp loop on a
    /// registry that has NOT been through this function — that's the
    /// recursion guard (rule "no agent can query while serving a
    /// query").
    ///
    /// Gated by `policy.permissions.can_query_other_agents` — opt-in,
    /// default off, same posture as `with_subagent` and `with_mcp`.
    pub fn with_agent_query(
        mut self,
        pending: agent_query::PendingQueries,
        writer_tx: tokio::sync::mpsc::Sender<havn_proto::AgentToGateway>,
    ) -> Self {
        if !self.policy.permissions.can_query_other_agents {
            return self;
        }
        self.tools.push(Arc::new(agent_query::AgentQueryTool::new(
            pending, writer_tx,
        )));
        self
    }

    /// Register every tool exposed by every successfully-started MCP
    /// server (spec §13 Phase 3). Each registers as
    /// `mcp__<server>__<tool>`. No-op when the registry's policy
    /// disables `can_use_mcp` or no servers came up — even with the
    /// gate on, an operator who whitelisted servers that all failed
    /// to spawn ends up with the same tool surface as if MCP were
    /// off, which is what we want.
    ///
    /// Collisions: two MCP servers can each expose a tool whose
    /// `<server>::<tool>` collapses to the same registry name only
    /// if their server names also collide — but `Policy.mcp_servers`
    /// is a `HashMap<String, _>` so server names are unique by
    /// construction. Within a single server, the MCP spec already
    /// requires tool names to be unique. So no collision can happen
    /// here without an upstream protocol violation.
    pub fn with_mcp(mut self, servers: &crate::mcp::McpServers) -> Self {
        if !self.policy.permissions.can_use_mcp {
            return self;
        }
        for (_name, client) in servers.iter() {
            for tool in &client.tools {
                self.tools
                    .push(Arc::new(crate::mcp::tool::McpToolHandle::new(
                        Arc::clone(client),
                        tool.clone(),
                    )));
            }
        }
        self
    }

    /// Wire a [`crate::hooks::Hooks`] handle. Calls to
    /// [`Self::execute_for`] then consult the hook set for vetoes
    /// before dispatching. Empty handle (no rules) is fine — dispatch
    /// stays a single map lookup. The runtime calls this once at
    /// startup; subagents inherit the parent's hooks.
    pub fn with_hooks(mut self, hooks: crate::hooks::Hooks) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// True iff the named tool is disabled in this execution context by
    /// `policy.context_toolsets[context].disabled`.
    fn disabled_in(&self, context: &str, tool_name: &str) -> bool {
        self.policy
            .context_toolsets
            .0
            .get(context)
            .is_some_and(|entry| entry.disabled.iter().any(|d| d == tool_name))
    }

    /// JSON array of tool descriptors in the shape Anthropic's `/v1/messages`
    /// expects under `tools`. **Unfiltered** — call sites that want the
    /// per-context filter applied should use [`Self::schemas_for`].
    ///
    /// Kept for tests and non-context call paths. The runtime always goes
    /// through `schemas_for(context)` so the LLM only sees what it can use.
    pub fn schemas(&self) -> serde_json::Value {
        self.schemas_iter(self.tools.iter().map(Arc::as_ref))
    }

    /// Per-context schema list: same as [`Self::schemas`] but with tools the
    /// policy disables in `context` filtered out. This is what gets sent to
    /// the LLM on every call.
    pub fn schemas_for(&self, context: &str) -> serde_json::Value {
        self.schemas_iter(
            self.tools
                .iter()
                .map(Arc::as_ref)
                .filter(|t| !self.disabled_in(context, t.name())),
        )
    }

    fn schemas_iter<'a>(
        &self,
        iter: impl Iterator<Item = &'a (dyn Tool + 'a)>,
    ) -> serde_json::Value {
        serde_json::Value::Array(
            iter.map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
                })
            })
            .collect(),
        )
    }

    /// True when the registry exposes no tools in this context (LLM should
    /// be called without a `tools` field).
    pub fn is_empty_for(&self, context: &str) -> bool {
        self.tools
            .iter()
            .all(|t| self.disabled_in(context, t.name()))
    }

    /// Dispatch `name` with the per-context filter applied. If the tool
    /// would be hidden from the LLM in this context, surfaces a
    /// `ToolResult::Error` instead of executing — defence in depth in
    /// case the LLM somehow invokes a tool it doesn't have a schema for
    /// (cached prior turns, prompt-injection echo, etc.).
    pub async fn execute_for(
        &self,
        context: &str,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolCtx,
    ) -> ToolResult {
        if self.disabled_in(context, name) {
            warn!(
                name,
                context, "tool blocked by context_toolsets — refusing dispatch"
            );
            return ToolResult::Error(format!(
                "tool {name:?} is disabled in {context:?} context by policy"
            ));
        }

        // PreToolUse hook check (spec §15 future hook surface, shipped
        // early because web research found this is the highest-praised
        // ergonomic feature in Claude Code). Consult before
        // registry-find so a denied call doesn't even pay the lookup
        // cost; surface a structured error so the LLM sees the deny
        // and adapts its plan.
        if let Some(hooks) = &self.hooks {
            let snapshot = hooks.load();
            if !snapshot.is_empty() {
                let flat = flatten_input_for_hook(name, &input);
                if let crate::hooks::Veto::Deny { reason } = snapshot.veto(name, &flat) {
                    warn!(name, context, "PreToolUse hook denied dispatch");
                    return ToolResult::Error(format!(
                        "tool {name:?} dispatch denied by hook: {reason}"
                    ));
                }
            }
        }

        let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
            warn!(name, "tool dispatch failed: unknown tool");
            return ToolResult::Error(format!("unknown tool: {name}"));
        };
        tool.execute(ctx, input).await
    }
}

/// Flatten a tool's JSON input into a single string the hook engine
/// can run regex against. Per-tool rules pick the field that's
/// meaningful for veto purposes:
///
/// | Tool | Flattened |
/// |------|-----------|
/// | `shell` | `command` |
/// | `web_fetch` | `url` |
/// | `file_read` / `file_write` | `path` |
/// | `memory_search` | `query` |
/// | other | concatenated values, lossy fallback |
///
/// Tools whose input structure changes can update this match arm.
fn flatten_input_for_hook(tool: &str, input: &serde_json::Value) -> String {
    let pick = |k: &str| {
        input
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match tool {
        "shell" => pick("command"),
        "web_fetch" => pick("url"),
        "file_read" | "file_write" => pick("path"),
        "memory_search" => pick("query"),
        "memory_remember" | "memory_forget" => pick("key"),
        "subagent_spawn" => pick("task"),
        "skill_manage" => format!(
            "{}:{}",
            input
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            input
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        ),
        // Fallback: serialize and trim. Hooks can still match on the
        // raw JSON if they want to be precise; deny patterns over the
        // raw blob are blunt but safe.
        _ => input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::{ContextToolsetEntry, Policy};

    fn names_in(schemas: &serde_json::Value) -> Vec<String> {
        schemas
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name")?.as_str().map(str::to_string))
            .collect()
    }

    fn registry(policy: Policy) -> ToolRegistry {
        ToolRegistry::standard(&Arc::new(policy))
    }

    // ---- capability layer (build time) ------------------------------------

    #[test]
    fn standard_includes_network_and_shell_when_allowed() {
        let policy = Policy::default();
        assert!(policy.permissions.can_access_network);
        assert!(policy.permissions.can_use_shell);

        let names = names_in(&registry(policy).schemas());
        for expected in [
            "memory_remember",
            "memory_search",
            "memory_forget",
            "file_read",
            "file_write",
            "web_fetch",
            "shell",
            "skill_manage",
        ] {
            assert!(names.contains(&expected.into()), "missing: {expected}");
        }
    }

    #[test]
    fn capability_layer_filters_disallowed_tools() {
        let mut policy = Policy::default();
        policy.permissions.can_access_network = false;
        policy.permissions.can_use_shell = false;
        policy.permissions.can_install_skills = false;
        let names = names_in(&registry(policy).schemas());

        assert!(!names.contains(&"web_fetch".into()));
        assert!(!names.contains(&"shell".into()));
        assert!(!names.contains(&"skill_manage".into()));
        // Always-on tools still present.
        assert!(names.contains(&"memory_remember".into()));
        assert!(names.contains(&"file_read".into()));
    }

    // ---- context layer (per call) -----------------------------------------

    fn policy_with_cron_disabled(disabled: &[&str]) -> Policy {
        let mut policy = Policy::default();
        policy.context_toolsets.0.insert(
            context::CRON.into(),
            ContextToolsetEntry {
                disabled: disabled.iter().map(|s| (*s).to_string()).collect(),
            },
        );
        policy
    }

    #[test]
    fn context_layer_hides_tool_in_one_context_only() {
        // Spec §6.2 example: cron jobs run unattended; disable interactive
        // tools. Webchat should still see the full set.
        let reg = registry(policy_with_cron_disabled(&["shell", "web_fetch"]));

        let cron = names_in(&reg.schemas_for(context::CRON));
        let web = names_in(&reg.schemas_for(context::WEBCHAT));

        assert!(!cron.contains(&"shell".into()));
        assert!(!cron.contains(&"web_fetch".into()));
        assert!(web.contains(&"shell".into()));
        assert!(web.contains(&"web_fetch".into()));
    }

    #[tokio::test]
    async fn execute_for_refuses_disabled_tool_even_if_invoked() {
        // Defence in depth: if a cached prior-turn tool_use somehow asks for
        // `shell` in a cron run, the runtime must refuse rather than
        // silently exec it. Mirrors Claude Code's deny-rule check at request
        // time, not registry build time.
        let reg = registry(policy_with_cron_disabled(&["shell"]));
        let env = ToolCtx {
            workspace_dir: std::path::PathBuf::from("/tmp"),
            agent_db: dummy_pool().await,
            http: reqwest::Client::new(),
            policy: Arc::clone(&reg.policy),
            embedder: None,
        };

        let res = reg
            .execute_for(
                context::CRON,
                "shell",
                serde_json::json!({"command": "echo hello"}),
                &env,
            )
            .await;
        assert!(res.is_error(), "{res:?}");
        assert!(res.text().contains("disabled"), "{}", res.text());
    }

    #[test]
    fn unknown_context_applies_no_filtering() {
        // Future-proofing: a context name nobody configured falls through
        // to the full capability set rather than blocking everything.
        let reg = registry(policy_with_cron_disabled(&["shell"]));
        let names = names_in(&reg.schemas_for("some-future-context"));
        assert!(names.contains(&"shell".into()));
    }

    #[test]
    fn is_empty_for_reflects_filter() {
        // A policy that disables every tool in cron context → empty schema
        // there but non-empty in webchat.
        let reg = registry(policy_with_cron_disabled(&[
            "memory_remember",
            "memory_search",
            "memory_forget",
            "file_read",
            "file_write",
            "web_fetch",
            "shell",
            "skill_manage",
        ]));
        assert!(reg.is_empty_for(context::CRON));
        assert!(!reg.is_empty_for(context::WEBCHAT));
    }

    async fn dummy_pool() -> sqlx::SqlitePool {
        // execute_for short-circuits before touching the DB for the disabled
        // case, so an in-memory pool with no migrations is fine.
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite")
    }

    // ---- MCP layer (spec §13 Phase 3) ----------------------------------

    /// Empty MCP-server set with `can_use_mcp = false` should be a
    /// strict no-op on the registry.
    #[test]
    fn with_mcp_no_op_when_can_use_mcp_disabled() {
        let mut policy = Policy::default();
        policy.permissions.can_use_mcp = false; // explicit, default already false
        let r0 = registry(policy.clone());
        let baseline = names_in(&r0.schemas());

        let r1 = registry(policy).with_mcp(&crate::mcp::McpServers::empty());
        let names = names_in(&r1.schemas());
        assert_eq!(
            names, baseline,
            "with_mcp must not change tool list when gate is off"
        );
        // Sanity: no mcp__ prefixed tools snuck in either way.
        assert!(names.iter().all(|n| !n.starts_with("mcp__")));
    }

    /// `can_use_mcp = true` + empty server map: still a no-op (no
    /// tools to register, but the call must succeed cleanly).
    #[test]
    fn with_mcp_no_op_when_no_servers_running() {
        let mut policy = Policy::default();
        policy.permissions.can_use_mcp = true;
        let r0 = registry(policy.clone());
        let baseline = names_in(&r0.schemas());
        let r1 = registry(policy).with_mcp(&crate::mcp::McpServers::empty());
        assert_eq!(names_in(&r1.schemas()), baseline);
    }
}
