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
//! - Deny-only. The gateway validates policy hooks strictly where a
//!   policy is written (`Policy::validate`). The workspace file is read
//!   leniently: unknown keys are ignored, and invalid patterns and rules
//!   past the caps are dropped with a warning. A file that is not valid
//!   YAML drops the workspace layer; the policy rules always apply.
//! - Patterns compile once per session start, with twice the size limit
//!   the gateway validates against, so a regex crate upgrade that grows
//!   compiled programs does not stop agents from starting.

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use havn_core::{HookPolicy, MAX_HOOK_PATTERN_LEN, MAX_HOOK_PATTERNS, MAX_HOOK_RULES};
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
    /// Patterns whose calls wait for the user's approval.
    pub ask: Vec<Regex>,
}

impl ToolHook {
    /// Whether the rule is about `tool`: its name, a prefix ending in `*`,
    /// and `file_write` covering `file_edit`, which changes the same files.
    /// `view_image`, `transcribe` and `watch_video` read files: a rule
    /// about `file_read` covers them.
    fn covers(&self, tool: &str) -> bool {
        if matches!(tool, "view_image" | "transcribe" | "watch_video") && self.covers("file_read") {
            return true;
        }
        match self.tool.strip_suffix('*') {
            Some(prefix) => tool.starts_with(prefix),
            None => self.tool == tool || (self.tool == "file_write" && tool == "file_edit"),
        }
    }
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
/// An invalid policy pattern is an error. The rule and pattern caps
/// apply to the workspace file, not to the policy: a resolved policy can
/// hold the union of several validated ones. A reload must also go
/// through here so it keeps the policy rules.
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

const COMPILE_SIZE_LIMIT: usize = 2 * havn_core::HOOK_REGEX_SIZE_LIMIT;

/// Lenient parse of the workspace file; only YAML that does not parse at
/// all is an error.
fn parse(yaml: &str) -> Result<HookSet, String> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        pre_tool_use: Vec<Rule>,
    }
    #[derive(Deserialize)]
    struct Rule {
        tool: String,
        #[serde(default)]
        deny: Vec<String>,
        #[serde(default)]
        ask: Vec<String>,
    }
    let file: File = serde_norway::from_str(yaml).map_err(|e| format!("yaml: {e}"))?;
    if file.pre_tool_use.len() > MAX_HOOK_RULES {
        warn!(
            ignored = file.pre_tool_use.len() - MAX_HOOK_RULES,
            "hooks.yaml has more than {MAX_HOOK_RULES} rules; ignoring the rest"
        );
    }
    let mut patterns_left = MAX_HOOK_PATTERNS;
    let mut rules = Vec::new();
    'rules: for rule in file.pre_tool_use.into_iter().take(MAX_HOOK_RULES) {
        if rule.tool.trim().is_empty() {
            warn!("hooks.yaml rule with an empty tool ignored");
            continue;
        }
        let mut compiled = [Vec::new(), Vec::new()];
        for (kind, pattern) in rule
            .deny
            .iter()
            .map(|p| (0, p))
            .chain(rule.ask.iter().map(|p| (1, p)))
        {
            if patterns_left == 0 {
                warn!(
                    "hooks.yaml has more than {MAX_HOOK_PATTERNS} deny and ask patterns; ignoring the rest"
                );
                let [deny, ask] = compiled;
                rules.push(ToolHook {
                    tool: rule.tool,
                    deny,
                    ask,
                });
                break 'rules;
            }
            let re = if pattern.len() > MAX_HOOK_PATTERN_LEN {
                Err(format!("longer than {MAX_HOOK_PATTERN_LEN} bytes"))
            } else {
                havn_core::compile_hook_pattern(pattern, COMPILE_SIZE_LIMIT)
                    .map_err(|e| e.to_string())
            };
            match re {
                Ok(re) => {
                    compiled[kind].push(re);
                    patterns_left -= 1;
                }
                // An ask pattern that cannot be used asks about every call
                // of the tool rather than none.
                Err(e) if kind == 1 => {
                    warn!(tool = %rule.tool, error = %e, "hooks.yaml ask pattern unusable; asking about every call");
                    if let Ok(every) = havn_core::compile_hook_pattern(
                        havn_core::policy::ASK_EVERY_CALL,
                        COMPILE_SIZE_LIMIT,
                    ) {
                        compiled[1].push(every);
                        patterns_left -= 1;
                    }
                }
                Err(e) => warn!(tool = %rule.tool, error = %e, "hooks.yaml pattern ignored"),
            }
        }
        let [deny, ask] = compiled;
        rules.push(ToolHook {
            tool: rule.tool,
            deny,
            ask,
        });
    }
    Ok(HookSet { rules })
}

fn compile(hooks: &HookPolicy) -> Result<HookSet, String> {
    let mut rules = Vec::with_capacity(hooks.pre_tool_use.len());
    for h in &hooks.pre_tool_use {
        let compile_all = |patterns: &[String]| -> Result<Vec<Regex>, String> {
            patterns
                .iter()
                .map(|pat| {
                    havn_core::compile_hook_pattern(pat, COMPILE_SIZE_LIMIT)
                        .map_err(|e| format!("tool {:?}: {e}", h.tool))
                })
                .collect()
        };
        rules.push(ToolHook {
            tool: h.tool.clone(),
            deny: compile_all(&h.deny)?,
            ask: compile_all(&h.ask)?,
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
    Deny {
        reason: String,
    },
    /// The call may run once the user approves it.
    Ask {
        reason: String,
    },
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
        let mut ask = None;
        for rule in &self.rules {
            if !rule.covers(tool) {
                continue;
            }
            if ask.is_none()
                && let Some(pat) = rule.ask.iter().find(|p| p.is_match(flat_input))
            {
                ask = Some(format!(
                    "PreToolUse hook ({}/ask): pattern {:?} matched",
                    rule.tool,
                    pat.as_str()
                ));
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
        // A deny anywhere wins over an ask.
        match ask {
            Some(reason) => Veto::Ask { reason },
            None => Veto::Allow,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The patterns of the rules about `file_read`, deny and ask alike:
    /// `file_find` and `file_search` leave out the files they match, as
    /// reading them would be refused or wait for approval.
    pub fn guarded_reads(&self) -> Vec<String> {
        self.rules
            .iter()
            // A rule for every tool (`*`) is about commands and URLs as
            // much as paths: only rules naming file tools count.
            .filter(|rule| rule.tool.starts_with("file") && rule.covers("file_read"))
            .flat_map(|rule| rule.deny.iter().chain(&rule.ask))
            .map(|p| p.as_str().to_string())
            .collect()
    }

    /// Tool names that rules target but `known` does not contain. A
    /// typo here would otherwise leave a rule that never fires.
    pub fn unknown_tools<'a>(&'a self, known: &[&str]) -> Vec<&'a str> {
        let mut unknown: Vec<&str> = self
            .rules
            .iter()
            .map(|r| r.tool.as_str())
            .filter(|t| !t.ends_with('*') && !known.contains(t))
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
    fn parse_skips_invalid_regex_only() {
        let set =
            set_with("pre_tool_use:\n  - tool: shell\n    deny: [\"[unclosed\", \"^sudo \"]\n");
        assert!(matches!(set.veto("shell", "sudo ls"), Veto::Deny { .. }));
        assert_eq!(set.rules[0].deny.len(), 1);
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
    fn file_write_rules_guard_file_edit() {
        let set = set_with(
            "pre_tool_use:
  - tool: file_write
    deny: ['^\\.git/']
",
        );
        assert!(matches!(
            set.veto("file_edit", ".git/config"),
            Veto::Deny { .. }
        ));
        assert!(matches!(set.veto("file_edit", "notes.md"), Veto::Allow));
        let edit_only = set_with(
            "pre_tool_use:
  - tool: file_edit
    deny: [\"x\"]
",
        );
        assert!(matches!(edit_only.veto("file_write", "x"), Veto::Allow));
    }

    #[test]
    fn file_read_rules_are_the_guarded_reads() {
        let set = set_with(
            "pre_tool_use:
  - tool: file_read
    deny: ['^secrets/']
    ask: ['\\.env$']
  - tool: file_*
    deny: ['^private/']
  - tool: shell
    deny: ['rm']
  - tool: '*'
    deny: ['curl']
",
        );
        assert_eq!(set.guarded_reads(), ["^secrets/", "\\.env$", "^private/"]);
    }

    #[test]
    fn ask_rules_wait_and_deny_wins() {
        let set = set_with(
            "pre_tool_use:
  - tool: mcp__github__*
    ask: ['(?s).*']
  - tool: shell
    ask: ['deploy']
    deny: ['^rm']
",
        );
        assert!(matches!(
            set.veto("mcp__github__create_issue", "{}"),
            Veto::Ask { .. }
        ));
        assert!(matches!(set.veto("mcp__gitlab__x", "{}"), Veto::Allow));
        assert!(matches!(set.veto("shell", "make deploy"), Veto::Ask { .. }));
        assert!(
            matches!(set.veto("shell", "rm deploy"), Veto::Deny { .. }),
            "deny wins"
        );
        assert!(matches!(set.veto("shell", "ls"), Veto::Allow));
        assert!(
            set.unknown_tools(&["shell"]).is_empty(),
            "prefix rules are not typos"
        );
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
            Veto::Allow | Veto::Ask { .. } => panic!("should have denied"),
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
    fn workspace_file_keeps_rules_up_to_the_caps() {
        let rules: String = (0..=MAX_HOOK_RULES)
            .map(|i| format!("  - tool: shell\n    deny: [\"^p{i}$\"]\n"))
            .collect();
        let set = set_with(&format!("pre_tool_use:\n{rules}"));
        assert_eq!(set.rules.len(), MAX_HOOK_RULES);
        assert!(matches!(set.veto("shell", "p0"), Veto::Deny { .. }));
        let last = MAX_HOOK_RULES - 1;
        assert!(matches!(
            set.veto("shell", &format!("p{last}")),
            Veto::Deny { .. }
        ));
        assert!(matches!(
            set.veto("shell", &format!("p{MAX_HOOK_RULES}")),
            Veto::Allow
        ));

        let patterns = (0..=MAX_HOOK_PATTERNS)
            .map(|i| format!("\"^q{i}$\""))
            .collect::<Vec<_>>()
            .join(", ");
        let set = set_with(&format!(
            "pre_tool_use:\n  - tool: shell\n    deny: [{patterns}]\n  - tool: file_read\n    deny: [\"x\"]\n"
        ));
        let kept: usize = set.rules.iter().map(|r| r.deny.len()).sum();
        assert_eq!(kept, MAX_HOOK_PATTERNS);
        assert!(matches!(set.veto("shell", "q0"), Veto::Deny { .. }));
        assert!(matches!(set.veto("file_read", "x"), Veto::Allow));
    }

    #[test]
    fn rules_about_reading_files_cover_viewing_images() {
        let set = set_with(
            "pre_tool_use:\n  - tool: file_read\n    deny: [\"^private/\"]\n  - tool: file_*\n    ask: [\"^shared/\"]\n  - tool: file_write\n    deny: [\"^out/\"]\n",
        );
        assert!(matches!(
            set.veto("view_image", "private/a.png"),
            Veto::Deny { .. }
        ));
        assert!(matches!(
            set.veto("view_image", "shared/a.png"),
            Veto::Ask { .. }
        ));
        assert!(
            matches!(set.veto("view_image", "out/a.png"), Veto::Allow),
            "writing is not reading"
        );
        assert!(matches!(set.veto("view_image", "a.png"), Veto::Allow));
        assert!(matches!(
            set.veto("transcribe", "private/call.ogg"),
            Veto::Deny { .. }
        ));
        assert!(matches!(set.veto("transcribe", "call.ogg"), Veto::Allow));
        assert!(matches!(
            set.veto("watch_video", "private/talk.mp4"),
            Veto::Deny { .. }
        ));
        assert!(matches!(
            set.veto("watch_video", "shared/talk.mp4"),
            Veto::Ask { .. }
        ));
        assert!(matches!(set.veto("watch_video", "talk.mp4"), Veto::Allow));
    }

    #[test]
    fn workspace_file_ignores_unknown_keys() {
        let set = set_with(
            "version: 1\npre_tool_use:\n  - tool: shell\n    note: legacy\n    deny: [\"^sudo \"]\n",
        );
        assert!(matches!(set.veto("shell", "sudo ls"), Veto::Deny { .. }));
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
