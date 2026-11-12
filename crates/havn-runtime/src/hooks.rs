//! PreToolUse hooks — deny rules that run before each tool dispatch
//! (Claude Code pattern).
//!
//! Rules come from two layers, applied together:
//!
//! 1. The policy the gateway resolves for the agent and ships in the
//!    Welcome frame (`policy.hooks`). The agent cannot edit it, so these
//!    are the operator's rules.
//! 2. An optional `<workspace>/.havn/hooks.yaml`. The agent can write its
//!    workspace, so this file can only add rules on top of the policy's;
//!    deleting or rewriting it never removes a policy rule.
//!
//! Both use the same shape (the policy form nests it under `hooks:`):
//!
//! ```yaml
//! pre_tool_use:
//!   - tool: shell
//!     deny:
//!       - "^rm -rf"            # block recursive force-delete
//!       - "^sudo "              # never sudo
//!       - "kubectl delete"      # block production destructive ops
//!   - tool: web_fetch
//!     deny:
//!       - "://internal\\.corp\\.net"
//! ```
//!
//! Hooks are guardrails against model mistakes, not a security
//! boundary: each pattern is a regex on a *flattened-string view* of the
//! input (the shell tool's `command` field, the file_write's `path`,
//! etc.), and a determined agent can phrase the same action so no
//! pattern matches (`rm  -rf`, `cd / && rm -rf`, a script file).
//! Enforcement belongs to the policy permissions (`can_use_shell`,
//! `can_access_network`, `network_policy`, mounts) and the sandbox.
//!
//! The runtime loads both layers once per session start. On every tool
//! dispatch, [`HookSet::veto`] runs each pattern matching the tool name;
//! on match, dispatch returns a structured tool_result error so the LLM
//! sees and adapts its plan.
//!
//! Design notes vs Claude Code:
//! - Claude Code lets hooks shell out to external commands; we keep
//!   the v1 surface to in-process regex match. External-command hooks
//!   add an exec/seccomp surface that's not worth the complexity until
//!   real users ask for it.
//! - Deny-only: a malformed workspace file is ignored (the policy rules
//!   still apply). The gateway validates policy hooks where a policy is
//!   written (`Policy::validate`), so an invalid one is not expected here.
//! - Patterns compile once per session start, with the size limits of
//!   [`havn_core::compile_hook_pattern`].

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use havn_core::HookPolicy;
use regex::Regex;
use tracing::{debug, info, warn};

/// One per-tool veto rule. `tool` matches against [`crate::tools::Tool::name`];
/// `deny` is a list of regex patterns; the rule fires when **any** pattern
/// matches the dispatch's flattened input string.
#[derive(Debug, Clone)]
pub struct ToolHook {
    pub tool: String,
    pub deny: Vec<Regex>,
}

/// Loaded hook configuration. Wrapped in `Arc<ArcSwap<...>>` in the
/// runtime so the gateway's hot-reload (spec §8.4) can drop a fresh
/// HookSet in atomically without locking the dispatch path.
#[derive(Debug, Default, Clone)]
pub struct HookSet {
    rules: Vec<ToolHook>,
}

/// Live handle to the hook set, swappable atomically on reload.
pub type Hooks = Arc<ArcSwap<HookSet>>;

/// Build the session's hook set: the policy's rules plus any rules in
/// `<workspace>/.havn/hooks.yaml`. The workspace file can only add rules.
/// An invalid policy pattern is an error; a missing, malformed or
/// over-limit workspace file only drops the workspace layer. The rule
/// and pattern caps apply to the workspace file, not to the policy: a
/// resolved policy can hold the union of several validated ones. A
/// reload must also go through here so it keeps the policy rules.
pub async fn load(policy: &HookPolicy, workspace: &Path) -> Result<HookSet, String> {
    let mut set = compile(policy).map_err(|e| format!("policy hooks: {e}"))?;
    let workspace_rules = load_from_workspace(workspace).await.rules;
    set.rules.extend(workspace_rules);
    if !set.is_empty() {
        info!(
            policy_rules = policy.pre_tool_use.len(),
            total_rules = set.rules.len(),
            "PreToolUse hooks loaded"
        );
    }
    Ok(set)
}

/// Default file location: `<workspace>/.havn/hooks.yaml`. Missing file
/// is fine; returns an empty HookSet.
async fn load_from_workspace(workspace: &Path) -> HookSet {
    let path = workspace.join(".havn").join("hooks.yaml");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no workspace hooks.yaml");
            return HookSet::default();
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "reading hooks.yaml failed; workspace hooks inactive");
            return HookSet::default();
        }
    };
    match parse(&content) {
        Ok(set) => set,
        Err(e) => {
            warn!(error = %e, "hooks.yaml malformed; workspace hooks inactive, policy hooks still apply");
            HookSet::default()
        }
    }
}

fn parse(yaml: &str) -> Result<HookSet, String> {
    let raw: HookPolicy = serde_norway::from_str(yaml).map_err(|e| format!("yaml: {e}"))?;
    raw.validate().map_err(|e| e.to_string())?;
    compile(&raw)
}

fn compile(hooks: &HookPolicy) -> Result<HookSet, String> {
    let mut rules = Vec::with_capacity(hooks.pre_tool_use.len());
    for h in &hooks.pre_tool_use {
        let mut compiled = Vec::with_capacity(h.deny.len());
        for pat in &h.deny {
            let re = havn_core::compile_hook_pattern(pat)
                .map_err(|e| format!("tool {:?}: {e}", h.tool))?;
            compiled.push(re);
        }
        rules.push(ToolHook {
            tool: h.tool.clone(),
            deny: compiled,
        });
    }
    Ok(HookSet { rules })
}

/// Outcome of a hook check. `Allow` means dispatch proceeds; `Deny`
/// carries a human-readable reason that's surfaced in the LLM's
/// tool_result so it can adapt.
#[derive(Debug, PartialEq, Eq)]
pub enum Veto {
    Allow,
    Deny { reason: String },
}

impl HookSet {
    /// Check whether a dispatch of `tool` with `flat_input` is denied
    /// by any rule. The first matching rule wins.
    ///
    /// `flat_input` is whatever the tool considers its primary
    /// argument: the shell command string, the file path, the URL.
    /// Tools that don't have a meaningful flat input (memory_search
    /// query) can pass an empty string — patterns then need to match
    /// the empty string, which is rare by design.
    pub fn veto(&self, tool: &str, flat_input: &str) -> Veto {
        for rule in &self.rules {
            if rule.tool != tool {
                continue;
            }
            for pat in &rule.deny {
                if pat.is_match(flat_input) {
                    return Veto::Deny {
                        reason: format!(
                            "blocked by PreToolUse hook ({}/{}): pattern {:?} matched",
                            rule.tool,
                            "deny",
                            pat.as_str()
                        ),
                    };
                }
            }
        }
        Veto::Allow
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Tool names that rules target but `known` does not contain. A
    /// typo here would otherwise leave a rule that never fires.
    pub fn unknown_tools<'a>(&'a self, known: &[&str]) -> Vec<&'a str> {
        let mut unknown: Vec<&str> = self
            .rules
            .iter()
            .map(|r| r.tool.as_str())
            .filter(|t| !known.contains(t))
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        unknown
    }
}

/// Construct a fresh atomically-swappable handle from a starting
/// `HookSet` (typically the result of [`load`]).
pub fn handle(initial: HookSet) -> Hooks {
    Arc::new(ArcSwap::from_pointee(initial))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn set_with(yaml: &str) -> HookSet {
        parse(yaml).expect("parse")
    }

    #[test]
    fn parse_empty_yaml_yields_empty_set() {
        assert!(set_with("").is_empty());
        assert!(set_with("pre_tool_use: []").is_empty());
    }

    #[test]
    fn parse_rejects_invalid_regex() {
        let yaml = "pre_tool_use:\n  - tool: shell\n    deny: [\"[unclosed\"]\n";
        assert!(parse(yaml).is_err());
    }

    #[test]
    fn veto_blocks_matching_shell() {
        let set = set_with(
            "pre_tool_use:\n  - tool: shell\n    deny:\n      - \"^rm -rf\"\n      - \"^sudo \"\n",
        );
        assert!(matches!(set.veto("shell", "rm -rf /"), Veto::Deny { .. }));
        assert!(matches!(
            set.veto("shell", "sudo apt update"),
            Veto::Deny { .. }
        ));
        assert!(matches!(set.veto("shell", "ls -la"), Veto::Allow));
    }

    #[test]
    fn veto_isolates_per_tool() {
        let set = set_with("pre_tool_use:\n  - tool: shell\n    deny: [\"sudo\"]\n");
        // Same string but different tool — no veto.
        assert!(matches!(set.veto("web_fetch", "sudo"), Veto::Allow));
    }

    #[test]
    fn veto_returns_pattern_in_reason() {
        let set = set_with("pre_tool_use:\n  - tool: shell\n    deny: [\"kubectl delete\"]\n");
        match set.veto("shell", "kubectl delete pod x") {
            Veto::Deny { reason } => {
                assert!(reason.contains("kubectl delete"));
                assert!(reason.contains("shell"));
            }
            Veto::Allow => panic!("should have denied"),
        }
    }

    fn workspace_with(yaml: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let havn_dir = dir.path().join(".havn");
        std::fs::create_dir_all(&havn_dir).expect("mkdir");
        if let Some(yaml) = yaml {
            std::fs::write(havn_dir.join("hooks.yaml"), yaml).expect("write");
        }
        dir
    }

    #[tokio::test]
    async fn load_missing_file_yields_empty_set() {
        let dir = workspace_with(None);
        let set = load(&HookPolicy::default(), dir.path())
            .await
            .expect("load");
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn load_real_file_parses() {
        let dir = workspace_with(Some(
            "pre_tool_use:\n  - tool: shell\n    deny:\n      - \"^rm -rf\"\n",
        ));
        let set = load(&HookPolicy::default(), dir.path())
            .await
            .expect("load");
        assert_eq!(set.rules.len(), 1);
        assert!(matches!(set.veto("shell", "rm -rf /"), Veto::Deny { .. }));
    }

    #[test]
    fn workspace_file_over_limits_is_rejected() {
        let rules: String = (0..=havn_core::MAX_HOOK_RULES)
            .map(|i| format!("  - tool: shell\n    deny: [\"^p{i}\"]\n"))
            .collect();
        assert!(parse(&format!("pre_tool_use:\n{rules}")).is_err());
        assert!(parse("pre_tool_use:\n  - tool: shell\n    denny: [\"x\"]\n").is_err());
    }

    #[test]
    fn unknown_tools_lists_rule_targets_missing_from_registry() {
        let set = set_with(
            "pre_tool_use:\n  - tool: shell\n    deny: [\"x\"]\n  - tool: shel\n    deny: [\"x\"]\n",
        );
        assert_eq!(set.unknown_tools(&["shell", "file_read"]), vec!["shel"]);
    }

    fn policy_denying_echo() -> HookPolicy {
        serde_norway::from_str("pre_tool_use:\n  - tool: shell\n    deny: [\"^echo\"]\n")
            .expect("policy hooks")
    }

    #[tokio::test]
    async fn policy_hooks_deny_without_workspace_file() {
        let dir = workspace_with(None);
        let set = load(&policy_denying_echo(), dir.path())
            .await
            .expect("load");
        assert!(matches!(set.veto("shell", "echo hi"), Veto::Deny { .. }));
        assert!(matches!(set.veto("shell", "ls"), Veto::Allow));
    }

    #[tokio::test]
    async fn workspace_hooks_add_rules_to_policy() {
        let dir = workspace_with(Some(
            "pre_tool_use:\n  - tool: shell\n    deny: [\"^sudo \"]\n",
        ));
        let set = load(&policy_denying_echo(), dir.path())
            .await
            .expect("load");
        assert!(matches!(set.veto("shell", "echo hi"), Veto::Deny { .. }));
        assert!(matches!(
            set.veto("shell", "sudo apt update"),
            Veto::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn workspace_file_cannot_remove_policy_rule() {
        let policy = policy_denying_echo();
        for yaml in [
            "pre_tool_use: []\n",
            "pre_tool_use:\n  - tool: shell\n    deny: []\n",
            "pre_tool_use: [not: valid: yaml\n",
        ] {
            let dir = workspace_with(Some(yaml));
            let set = load(&policy, dir.path()).await.expect("load");
            assert!(
                matches!(set.veto("shell", "echo hi"), Veto::Deny { .. }),
                "workspace {yaml:?} removed the policy rule"
            );
        }
    }
}
