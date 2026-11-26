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
use tracing::{info, warn};

pub mod agent_query;
pub mod files;
pub mod gateway;
pub mod memory;
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
    /// Agent's writable workspace root. The file tools are scoped here.
    pub workspace_dir: PathBuf,
    /// Per-agent `SQLite` pool for memory / conversation queries.
    pub agent_db: SqlitePool,
    /// Active policy snapshot. Registry building consults `permissions`;
    /// tools may read it for finer checks.
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
    /// `text`, and the image at workspace path `path` shown to the model
    /// with it (issue #227). The gateway reads the file.
    Image {
        text: String,
        path: String,
    },
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
            Self::Ok(s) | Self::Error(s) | Self::Image { text: s, .. } => s,
        }
    }

    /// With the agent's secrets redacted from its text (issue #233).
    #[must_use]
    pub fn redacted(self) -> Self {
        match self {
            Self::Ok(s) => Self::Ok(crate::secrets::redact(&s)),
            Self::Error(s) => Self::Error(crate::secrets::redact(&s)),
            Self::Image { text, path } => Self::Image {
                text: crate::secrets::redact(&text),
                path,
            },
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
    /// Asks the user about calls a hook's `ask` rule matches.
    approvals: Option<Arc<crate::approvals::Approvals>>,
}

impl ToolRegistry {
    /// Build the standard tool set with two-layer policy enforcement (spec §6):
    ///
    /// 1. **Capability layer (here, build time)** — drop tools whose
    ///    underlying capability is forbidden outright (`can_use_shell`,
    ///    `can_install_skills`). The LLM never sees these even with no
    ///    context filter. Tools the gateway runs (`web_search`,
    ///    `web_fetch`) are added by [`Self::with_gateway_tools`].
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
            Arc::new(memory::ConversationSearchTool),
            Arc::new(files::FileReadTool),
            Arc::new(files::FileWriteTool),
            Arc::new(files::FileEditTool),
            Arc::new(files::FileFindTool),
            Arc::new(files::FileSearchTool),
            Arc::new(files::ViewImageTool),
        ];
        if policy.permissions.can_use_shell {
            tools.push(Arc::new(shell::ShellTool));
            tools.push(Arc::new(shell::ProcessTool));
        }
        if policy.permissions.can_install_skills {
            tools.push(Arc::new(skill_manage::SkillManageTool));
        }
        Self {
            tools,
            policy: Arc::clone(policy),
            hooks: None,
            approvals: None,
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

    /// Offer the tools the gateway runs for this agent. The gateway lists
    /// only what the agent's policy allows; a name the registry already
    /// has keeps its local tool.
    pub fn with_gateway_tools(
        mut self,
        specs: Vec<havn_proto::GatewayToolSpec>,
        pending: &gateway::PendingGatewayCalls,
        writer_tx: &tokio::sync::mpsc::Sender<havn_proto::AgentToGateway>,
    ) -> Self {
        for spec in specs {
            if self.tools.iter().any(|t| t.name() == spec.name) {
                warn!(tool = %spec.name, "gateway tool shadows a local tool; ignoring it");
                continue;
            }
            self.tools.push(Arc::new(gateway::GatewayTool::new(
                spec,
                Arc::clone(pending),
                writer_tx.clone(),
            )));
        }
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

    /// Ask the user, through `approvals`, before calls a hook's `ask`
    /// rule matches.
    #[must_use]
    pub fn with_approvals(mut self, approvals: Arc<crate::approvals::Approvals>) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// The registry without the tool called `name`.
    #[must_use]
    pub fn without(mut self, name: &str) -> Self {
        self.tools.retain(|t| t.name() != name);
        self
    }

    /// Names of every registered tool, regardless of context.
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// True iff the named tool is disabled in this execution context by
    /// `policy.context_toolsets[context].disabled`.
    /// Disabling `file_write` disables `file_edit` too: both change files,
    /// and a policy written before `file_edit` existed means both. Likewise
    /// disabling `file_read` disables `file_find`, `file_search`, `view_image`
    /// and `transcribe`, and disabling `shell` disables `process`, which
    /// reaches what it started.
    fn disabled_in(&self, context: &str, tool_name: &str) -> bool {
        // Scheduled runs, heartbeats and other agents' queries answer no
        // one's chat message: there are no past chats of theirs to search.
        if tool_name == "conversation_search"
            && matches!(
                context,
                context::CRON | context::HEARTBEAT | context::AGENT_QUERY
            )
        {
            return true;
        }
        self.policy
            .context_toolsets
            .0
            .get(context)
            .is_some_and(|entry| {
                entry.disabled.iter().any(|d| {
                    d == tool_name
                        || (d == "file_write" && tool_name == "file_edit")
                        || (d == "file_read"
                            && matches!(
                                tool_name,
                                "file_find" | "file_search" | "view_image" | "transcribe"
                            ))
                        || (d == "shell" && tool_name == "process")
                })
            })
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
        // An unknown tool is refused before any hook asks about it.
        let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
            warn!(name, "tool dispatch failed: unknown tool");
            return ToolResult::Error(format!("unknown tool: {name}"));
        };
        // In a conversation that is not the owner's, what runs, changes or
        // reads things waits for the owner (issue #255). So it does in a
        // group, though they wrote (issue #297): others read it, and their
        // lines come with the owner's.
        let owner = gateway::FROM_OWNER
            .try_with(|owner| *owner)
            .unwrap_or(false);
        let in_group = gateway::IN_GROUP.try_with(|group| *group).unwrap_or(false);
        let review = if owner && !in_group {
            None
        } else {
            crate::approvals::Review::of(name)
        };
        let mut must_ask = false;
        if let Some(hooks) = &self.hooks {
            // The snapshot guard is not held across the approval wait.
            let verdict = {
                let snapshot = hooks.load();
                (!snapshot.is_empty())
                    .then(|| snapshot.veto(name, &flatten_input_for_hook(name, &input)))
            };
            if let Some(veto) = verdict {
                match veto {
                    crate::hooks::Veto::Allow => {}
                    crate::hooks::Veto::Deny { reason } => {
                        warn!(name, context, "PreToolUse hook denied dispatch");
                        return ToolResult::Error(format!(
                            "tool {name:?} dispatch denied by hook: {reason}"
                        ));
                    }
                    // The owner is asked about it anyway, every time.
                    crate::hooks::Veto::Ask { .. } if review.is_some() => must_ask = true,
                    crate::hooks::Veto::Ask { reason } => {
                        let conversation = crate::tools::gateway::CONVERSATION
                            .try_with(Clone::clone)
                            .ok();
                        let chat = crate::tools::gateway::CHAT.try_with(Clone::clone).ok();
                        let decided = match &self.approvals {
                            Some(approvals) => {
                                // The whole input: what is approved, and
                                // what "for this conversation" remembers.
                                let whole = input.to_string();
                                approvals
                                    .ask(
                                        conversation.as_deref(),
                                        chat.as_deref(),
                                        name,
                                        &whole,
                                        &reason,
                                    )
                                    .await
                            }
                            None => Err("it needs approval, which is not set up here".to_string()),
                        };
                        if let Err(why) = decided {
                            info!(name, context, why = %why, "tool call not approved");
                            return ToolResult::Error(format!("tool {name:?} was not run: {why}"));
                        }
                    }
                }
            }
        }

        // Finding and searching files leave out the files a rule about
        // file_read guards; the registry says which, whatever the model
        // sent in that field.
        let shown = review.map(|_| input.clone());
        let mut input = input;
        if matches!(name, "file_find" | "file_search")
            && let Some(obj) = input.as_object_mut()
        {
            let guarded = self
                .hooks
                .as_ref()
                .map(|h| h.load().guarded_reads())
                .unwrap_or_default();
            obj.insert(files::GUARDED_READS.into(), serde_json::json!(guarded));
        }

        // The agent's secrets are redacted from what the tool returns
        // before anyone sees it: the owner reviewing it, the model, the
        // chat that keeps it (issue #233).
        let run = async { tool.execute(ctx, input).await.redacted() };
        match (review, shown) {
            (Some(review), Some(shown)) => {
                let approvals = self.approvals.clone().or_else(crate::approvals::global);
                crate::approvals::guarded(
                    approvals.as_deref(),
                    &ctx.agent_db,
                    review,
                    name,
                    &shown,
                    must_ask,
                    run,
                )
                .await
            }
            _ => run.await,
        }
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
/// | `file_read` / `file_write` / `file_edit` | `path` |
/// | `file_find` / `file_search` | `path` |
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
        "file_read" | "file_write" | "file_edit" | "view_image" | "transcribe" => pick("path"),
        "file_find" | "file_search" => pick("path"),
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
    fn standard_includes_shell_and_skills_when_allowed() {
        let policy = Policy::default();
        assert!(policy.permissions.can_use_shell);

        let names = names_in(&registry(policy).schemas());
        for expected in [
            "memory_remember",
            "memory_search",
            "memory_forget",
            "file_read",
            "file_write",
            "file_edit",
            "file_find",
            "file_search",
            "shell",
            "skill_manage",
        ] {
            assert!(names.contains(&expected.into()), "missing: {expected}");
        }
        assert!(
            !names.contains(&"web_fetch".into()),
            "web_fetch comes from the gateway"
        );
    }

    #[test]
    fn capability_layer_filters_disallowed_tools() {
        let mut policy = Policy::default();
        policy.permissions.can_access_network = false;
        policy.permissions.can_use_shell = false;
        policy.permissions.can_install_skills = false;
        let names = names_in(&registry(policy).schemas());

        assert!(!names.contains(&"shell".into()));
        assert!(!names.contains(&"skill_manage".into()));
        // Always-on tools still present.
        assert!(names.contains(&"memory_remember".into()));
        assert!(names.contains(&"file_read".into()));
        assert!(
            names.contains(&"file_find".into()) && names.contains(&"file_search".into()),
            "found without shell"
        );
    }

    // ---- context layer (per call) -----------------------------------------

    fn policy_with_cron_disabled(disabled: &[&str]) -> Policy {
        let mut policy = Policy::default();
        policy.network_policy.egress_allowed = true;
        policy.context_toolsets.0.insert(
            context::CRON.into(),
            ContextToolsetEntry {
                disabled: disabled.iter().map(|s| (*s).to_string()).collect(),
            },
        );
        policy
    }

    #[test]
    fn conversation_search_is_offered_only_where_a_chat_message_is_answered() {
        let reg = registry(Policy::default());
        for hidden in [context::CRON, context::HEARTBEAT, context::AGENT_QUERY] {
            assert!(
                !names_in(&reg.schemas_for(hidden)).contains(&"conversation_search".into()),
                "{hidden}"
            );
        }
        for shown in [context::WEBCHAT, context::SUBAGENT] {
            assert!(
                names_in(&reg.schemas_for(shown)).contains(&"conversation_search".into()),
                "{shown}"
            );
        }
    }

    #[test]
    fn context_layer_hides_tool_in_one_context_only() {
        // Spec §6.2 example: cron jobs run unattended; disable interactive
        // tools. Webchat should still see the full set.
        let reg = registry(policy_with_cron_disabled(&["shell", "file_write"]));

        let cron = names_in(&reg.schemas_for(context::CRON));
        let web = names_in(&reg.schemas_for(context::WEBCHAT));

        assert!(!cron.contains(&"shell".into()));
        assert!(!cron.contains(&"file_write".into()));
        assert!(web.contains(&"shell".into()));
        assert!(web.contains(&"file_write".into()));
        // What reaches the shell's processes goes with it (issue #215).
        assert!(!cron.contains(&"process".into()));
        assert!(web.contains(&"process".into()));
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

    #[tokio::test]
    async fn someone_else_s_turn_reads_files_only_through_the_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("notes.md"), "the owner's note").expect("write");
        let reg = registry(Policy::default());
        let env = ToolCtx {
            workspace_dir: dir.path().to_path_buf(),
            agent_db: dummy_pool().await,
            policy: Arc::clone(&reg.policy),
            embedder: None,
        };
        let read = || {
            reg.execute_for(
                context::WEBCHAT,
                "file_read",
                serde_json::json!({"path": "notes.md"}),
                &env,
            )
        };
        // Issue #255: with no one to ask, someone else's turn gets nothing.
        for res in [gateway::FROM_OWNER.scope(false, read()).await, read().await] {
            assert!(res.is_error(), "{}", res.text());
            assert!(!res.text().contains("the owner's note"), "{}", res.text());
        }
        let res = gateway::FROM_OWNER.scope(true, read()).await;
        assert!(res.text().contains("the owner's note"), "{}", res.text());
        // Issue #297: in a group, which others read, the owner's turn reads
        // and runs things through them too.
        let res = gateway::IN_GROUP
            .scope(true, gateway::FROM_OWNER.scope(true, read()))
            .await;
        assert!(res.is_error(), "{}", res.text());
        assert!(!res.text().contains("the owner's note"), "{}", res.text());
        let write = reg.execute_for(
            context::WEBCHAT,
            "file_write",
            serde_json::json!({"path": "out.md", "content": "done"}),
            &env,
        );
        let res = gateway::IN_GROUP
            .scope(true, gateway::FROM_OWNER.scope(true, write))
            .await;
        assert!(res.is_error(), "{}", res.text());
        assert!(!dir.path().join("out.md").exists());
    }

    #[tokio::test]
    async fn finding_files_follows_file_read_and_its_rules() {
        // The owner's turn: someone else's waits for the owner (issue #255).
        gateway::FROM_OWNER
            .scope(true, async {
                let mut policy = Policy::default();
                policy.hooks.pre_tool_use.push(havn_core::ToolHookRule {
                    tool: "file_read".into(),
                    deny: vec!["^secrets/".into()],
                    ask: Vec::new(),
                });
                let dir = tempfile::tempdir().expect("tempdir");
                std::fs::create_dir_all(dir.path().join("secrets")).expect("dir");
                std::fs::write(dir.path().join("secrets/key.md"), "k").expect("write");
                std::fs::write(dir.path().join("notes.md"), "n").expect("write");
                let set = crate::hooks::load(&policy.hooks, dir.path())
                    .await
                    .expect("hooks");
                let reg = registry(policy.clone()).with_hooks(crate::hooks::handle(set));
                let env = ToolCtx {
                    workspace_dir: dir.path().to_path_buf(),
                    agent_db: dummy_pool().await,
                    policy: Arc::clone(&reg.policy),
                    embedder: None,
                };
                // The model cannot clear the rules by sending the field itself.
                let res = reg
                    .execute_for(
                        context::WEBCHAT,
                        "file_find",
                        serde_json::json!({"pattern": "*.md", files::GUARDED_READS: []}),
                        &env,
                    )
                    .await;
                assert!(!res.text().contains("secrets/key.md"), "{}", res.text());
                assert!(res.text().contains("notes.md"), "{}", res.text());
                // Nor hide files from a registry that has no rules.
                let res = registry(Policy::default())
                    .execute_for(
                        context::WEBCHAT,
                        "file_find",
                        serde_json::json!({"pattern": "*.md", files::GUARDED_READS: ["."]}),
                        &env,
                    )
                    .await;
                assert!(res.text().contains("secrets/key.md"), "{}", res.text());

                policy.context_toolsets.0.insert(
                    context::CRON.into(),
                    ContextToolsetEntry {
                        disabled: vec!["file_read".into()],
                    },
                );
                let names = names_in(&registry(policy).schemas_for(context::CRON));
                assert!(
                    !names.contains(&"file_find".into())
                        && !names.contains(&"file_search".into())
                        && !names.contains(&"view_image".into())
                );
            })
            .await;
    }

    #[tokio::test]
    async fn execute_for_applies_policy_hook_to_mcp_tool() {
        let mut policy = Policy::default();
        policy.hooks.pre_tool_use.push(havn_core::ToolHookRule {
            tool: "mcp__fs__write_file".into(),
            deny: vec!["/etc/".into()],
            ask: Vec::new(),
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let set = crate::hooks::load(&policy.hooks, dir.path())
            .await
            .expect("hooks");
        // Stands in for the MCP server's tool, which the hook must guard.
        struct McpWrite;
        #[async_trait]
        impl Tool for McpWrite {
            fn name(&self) -> &str {
                "mcp__fs__write_file"
            }
            fn description(&self) -> &str {
                "write"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _ctx: &ToolCtx, _input: serde_json::Value) -> ToolResult {
                ToolResult::Ok("written".into())
            }
        }
        let mut reg = registry(policy).with_hooks(crate::hooks::handle(set));
        reg.tools.push(Arc::new(McpWrite));
        let env = ToolCtx {
            workspace_dir: dir.path().to_path_buf(),
            agent_db: dummy_pool().await,
            policy: Arc::clone(&reg.policy),
            embedder: None,
        };

        let res = reg
            .execute_for(
                context::WEBCHAT,
                "mcp__fs__write_file",
                serde_json::json!({"path": "/etc/passwd", "content": "x"}),
                &env,
            )
            .await;
        assert!(res.is_error(), "{res:?}");
        assert!(res.text().contains("denied by hook"), "{}", res.text());
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
            "file_edit",
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
