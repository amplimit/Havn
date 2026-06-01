//! PreToolUse hooks — operator-defined deny rules that run before each
//! tool dispatch (Claude Code pattern, validated by web research as a
//! top user-praise feature).
//!
//! From the user's seat: in `workspace/.havn/hooks.yaml` you write a
//! few lines of YAML to forbid specific tool calls before they reach
//! the OS — no per-call confirmation prompt, no surprise. Example:
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
//! The runtime loads `hooks.yaml` once per session start (Phase 2 —
//! hot-reload comes with the gateway's reload subsystem extension if a
//! user asks). On every tool dispatch, [`HookSet::veto`] runs each
//! pattern matching the tool name against a *flattened-string view* of
//! the input (the shell tool's `command` field, the file_write's
//! `path`, etc.). On match, dispatch returns a structured tool_result
//! error so the LLM sees and adapts its plan.
//!
//! Design notes vs Claude Code:
//! - Claude Code lets hooks shell out to external commands; we keep
//!   the v1 surface to in-process regex match. External-command hooks
//!   add an exec/seccomp surface that's not worth the complexity until
//!   real users ask for it.
//! - Both forbid and allow modes exist in the literature; we ship
//!   deny-only for v1 because the failure mode is fail-safe (hook
//!   error → tool denied → user sees the deny in the result).

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use regex::Regex;
use serde::Deserialize;
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

/// Default file location: `<workspace>/.havn/hooks.yaml`. Missing file
/// is fine; returns an empty HookSet so the dispatch path is a no-op.
pub async fn load_from_workspace(workspace: &Path) -> HookSet {
    let path = workspace.join(".havn").join("hooks.yaml");
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no hooks.yaml; PreToolUse hooks inactive");
            return HookSet::default();
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "reading hooks.yaml failed; PreToolUse hooks inactive");
            return HookSet::default();
        }
    };
    match parse(&content) {
        Ok(set) => {
            info!(rules = set.rules.len(), "PreToolUse hooks loaded");
            set
        }
        Err(e) => {
            warn!(error = %e, "hooks.yaml malformed; PreToolUse hooks inactive (fail-safe)");
            HookSet::default()
        }
    }
}

/// Parse a hooks YAML body. Public for tests + future hot-reload.
pub fn parse(yaml: &str) -> Result<HookSet, String> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        pre_tool_use: Vec<RawHook>,
    }
    #[derive(Deserialize)]
    struct RawHook {
        tool: String,
        #[serde(default)]
        deny: Vec<String>,
    }
    let raw: Raw = serde_norway::from_str(yaml).map_err(|e| format!("yaml: {e}"))?;
    let mut rules = Vec::with_capacity(raw.pre_tool_use.len());
    for h in raw.pre_tool_use {
        let mut compiled = Vec::with_capacity(h.deny.len());
        for pat in &h.deny {
            let re =
                Regex::new(pat).map_err(|e| format!("regex {pat:?} for tool {:?}: {e}", h.tool))?;
            compiled.push(re);
        }
        rules.push(ToolHook {
            tool: h.tool,
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
}

/// Construct a fresh atomically-swappable handle from a starting
/// `HookSet` (typically the result of `load_from_workspace`).
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

    #[tokio::test]
    async fn load_missing_file_yields_empty_set() {
        let dir = std::env::temp_dir().join(format!("havn-hooks-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        // .havn/hooks.yaml deliberately not created.
        let set = load_from_workspace(&dir).await;
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn load_real_file_parses() {
        let dir = std::env::temp_dir().join(format!("havn-hooks-{}", uuid::Uuid::now_v7()));
        let havn_dir = dir.join(".havn");
        tokio::fs::create_dir_all(&havn_dir).await.expect("mkdir");
        let yaml = "pre_tool_use:\n  - tool: shell\n    deny:\n      - \"^rm -rf\"\n";
        tokio::fs::write(havn_dir.join("hooks.yaml"), yaml)
            .await
            .expect("write");
        let set = load_from_workspace(&dir).await;
        assert_eq!(set.rules.len(), 1);
        assert!(matches!(set.veto("shell", "rm -rf /"), Veto::Deny { .. }));
    }
}
