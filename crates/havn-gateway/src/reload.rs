//! Config hot reload — gateway side of spec §8.4.
//!
//! At a high level:
//!
//! 1. A debounced fs-notify watcher observes the gateway's config file.
//! 2. On every settled change, the watcher reparses the file. Parse errors
//!    don't roll back the running config — the previous snapshot stays
//!    authoritative until the next valid edit.
//! 3. Old vs new is JSON-diffed into a flat list of changed dot-paths.
//! 4. Each path is matched against the static [`RULES`] table (longest
//!    prefix wins; unmatched paths default to `Restart`).
//! 5. The selected [`ReloadMode`] decides what actually happens:
//!     - `Off`  → nothing.
//!     - `Restart` → if anything changed at all, schedule a restart.
//!     - `Hot` → if any path requires restart, log a warning and skip;
//!       otherwise run the hot actions.
//!     - `Hybrid` (default) → run hot actions for hot paths AND schedule
//!       restart if any path is restart-only.
//!
//! "Schedule restart" in Phase 1 = log clearly and ask the operator to
//! restart. Production deployments can replace [`RestartTrigger`] with a
//! sentinel-file + SIGUSR1 implementation per spec §8.4.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::{GatewayConfig, ReloadMode};

/// One row in the static rule table. Order doesn't matter — match is by
/// longest-matching prefix.
#[derive(Debug, Clone, Copy)]
pub struct ReloadRule {
    pub prefix: &'static str,
    pub kind: ReloadKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadKind {
    /// Hot-reload: in-place update with no service interruption.
    Hot,
    /// Restart required: the path touches state that can't be safely changed
    /// in a running process (listen address, db path, …).
    Restart,
    /// Path is intentionally ignored. Reserved for fields whose changes
    /// don't affect runtime behaviour (formatting, comments, …); no current
    /// rule uses this variant but it's kept so adding such rules later is a
    /// one-line change instead of a re-design.
    #[allow(
        dead_code,
        reason = "reserved for spec-driven additions like skills.* = none"
    )]
    None,
}

/// Static rule table — see spec §8.4.
///
/// We list more-specific paths first by convention, but the matcher picks the
/// longest-prefix match regardless of declaration order.
pub const RULES: &[ReloadRule] = &[
    ReloadRule {
        prefix: "allowed_origins",
        kind: ReloadKind::Hot,
    },
    ReloadRule {
        prefix: "defaults",
        kind: ReloadKind::Hot,
    },
    ReloadRule {
        prefix: "reload",
        kind: ReloadKind::Restart,
    },
    ReloadRule {
        prefix: "listen",
        kind: ReloadKind::Restart,
    },
    ReloadRule {
        prefix: "db_path",
        kind: ReloadKind::Restart,
    },
    ReloadRule {
        prefix: "agent_socket_path",
        kind: ReloadKind::Restart,
    },
    ReloadRule {
        prefix: "workspace_root",
        kind: ReloadKind::Restart,
    },
    ReloadRule {
        prefix: "runtime_binary",
        kind: ReloadKind::Restart,
    },
];

/// Plan derived from a single config diff.
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror ReloadKind variants"
)]
#[derive(Debug, Default, Clone)]
pub struct ReloadPlan {
    pub hot_paths: Vec<String>,
    pub restart_paths: Vec<String>,
    pub ignored_paths: Vec<String>,
}

impl ReloadPlan {
    pub fn build(diff_paths: &[String]) -> Self {
        let mut plan = Self::default();
        for path in diff_paths {
            match longest_match(path, RULES).kind {
                ReloadKind::Hot => plan.hot_paths.push(path.clone()),
                ReloadKind::Restart => plan.restart_paths.push(path.clone()),
                ReloadKind::None => plan.ignored_paths.push(path.clone()),
            }
        }
        plan
    }

    pub fn is_empty(&self) -> bool {
        self.hot_paths.is_empty() && self.restart_paths.is_empty()
    }
}

/// Decision for a given mode + plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadDecision {
    /// Nothing to do (mode `Off`, or all changes ignored).
    NoOp,
    /// Restart the gateway. Phase 1 surfaces this to the operator via logs.
    Restart { reason: Vec<String> },
    /// Run hot actions only.
    HotOnly { paths: Vec<String> },
    /// Mode `Hot` and a restart path was touched: skip everything for safety.
    /// Spec §8.4: "if `mode = hot` and a restart path appears: log warning,
    /// do nothing for that path."
    SkippedDueToRestartInHotMode { paths: Vec<String> },
}

pub fn decide(plan: &ReloadPlan, mode: ReloadMode) -> ReloadDecision {
    if plan.is_empty() {
        return ReloadDecision::NoOp;
    }
    match mode {
        ReloadMode::Off => ReloadDecision::NoOp,
        ReloadMode::Restart => ReloadDecision::Restart {
            reason: plan
                .hot_paths
                .iter()
                .chain(plan.restart_paths.iter())
                .cloned()
                .collect(),
        },
        ReloadMode::Hot => {
            if plan.restart_paths.is_empty() {
                ReloadDecision::HotOnly {
                    paths: plan.hot_paths.clone(),
                }
            } else {
                ReloadDecision::SkippedDueToRestartInHotMode {
                    paths: plan.restart_paths.clone(),
                }
            }
        }
        ReloadMode::Hybrid => {
            if plan.restart_paths.is_empty() {
                ReloadDecision::HotOnly {
                    paths: plan.hot_paths.clone(),
                }
            } else {
                ReloadDecision::Restart {
                    reason: plan.restart_paths.clone(),
                }
            }
        }
    }
}

fn longest_match(path: &str, rules: &[ReloadRule]) -> ReloadRule {
    rules
        .iter()
        .copied()
        .filter(|r| path == r.prefix || path.starts_with(&format!("{}.", r.prefix)))
        .max_by_key(|r| r.prefix.len())
        .unwrap_or(ReloadRule {
            prefix: "",
            kind: ReloadKind::Restart,
        })
}

/// Recursively diff two JSON values; produce a flat list of dot-path strings
/// where they differ. Map keys are joined with `.`; arrays are compared as a
/// whole (any element change emits the whole array's path). Matches spec §8.4
/// rule grammar — rules pivot on top-level config field names.
pub fn diff_paths(old: &Value, new: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(old, new, "", &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk(old: &Value, new: &Value, prefix: &str, out: &mut Vec<String>) {
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            let keys: BTreeSet<&String> = o.keys().chain(n.keys()).collect();
            for k in keys {
                let next_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                let null = Value::Null;
                let ov = o.get(k).unwrap_or(&null);
                let nv = n.get(k).unwrap_or(&null);
                walk(ov, nv, &next_prefix, out);
            }
        }
        _ => out.push(prefix.to_string()),
    }
}

/// Hot-reloadable handles `AppState` shares with the watcher. Add new fields
/// here when adding new hot paths to [`RULES`].
#[derive(Clone)]
pub struct ReloadHandles {
    pub allowed_origins: Arc<ArcSwap<Vec<String>>>,
}

impl ReloadHandles {
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        Self {
            allowed_origins: Arc::new(ArcSwap::from_pointee(cfg.allowed_origins.clone())),
        }
    }

    /// Apply a hot-reload set: swap the underlying values for any field
    /// whose dot-path appears in `paths`.
    pub fn apply_hot(&self, new: &GatewayConfig, paths: &[String]) {
        for path in paths {
            if path.starts_with("allowed_origins") {
                self.allowed_origins
                    .store(Arc::new(new.allowed_origins.clone()));
                info!(
                    paths = ?paths,
                    new_count = new.allowed_origins.len(),
                    "hot-reloaded allowed_origins"
                );
            } else {
                warn!(
                    path,
                    "hot path has no live applier — Phase 1 silently ignores"
                );
            }
        }
    }
}

/// Restart trigger surface. Phase 1 just logs; production deployments swap
/// in a sentinel-file + SIGUSR1 implementation per spec §8.4.
#[derive(Debug, Clone, Default)]
pub struct RestartTrigger;

impl RestartTrigger {
    #[allow(
        clippy::unused_self,
        reason = "Phase 2 wires sentinel + SIGUSR1 — keeps call-site stable"
    )]
    pub fn request(&self, reason: &[String]) {
        warn!(
            paths = ?reason,
            "configuration change requires restart — schedule one to apply"
        );
    }
}

/// Spawn the watcher task. Returns immediately; the task runs forever or
/// until the channel from the underlying notify watcher closes.
pub fn spawn_watcher(
    config_path: PathBuf,
    initial: GatewayConfig,
    handles: ReloadHandles,
    restart: RestartTrigger,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_watcher(config_path, initial, handles, restart).await {
            error!(error = ?e, "config watcher terminated with error");
        }
    })
}

async fn run_watcher(
    config_path: PathBuf,
    initial: GatewayConfig,
    handles: ReloadHandles,
    restart: RestartTrigger,
) -> anyhow::Result<()> {
    use notify::{EventKind, RecursiveMode, Watcher};

    info!(path = %config_path.display(), "config watcher starting");
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = notify_tx.send(res);
    })?;
    // Watch the parent directory — many editors write a tempfile then rename,
    // which fires a Remove on the original path. Watching the dir catches both.
    let watch_target = config_path
        .parent()
        .map_or_else(|| config_path.clone(), std::path::Path::to_path_buf);
    watcher.watch(&watch_target, RecursiveMode::NonRecursive)?;

    let mut current = initial;
    let mut last_known_good = current.clone();

    let debounce = Duration::from_millis(u64::from(current.reload.debounce_ms.max(1)));
    let mode = current.reload.mode;

    loop {
        // Wait for any event referencing our config path.
        let Some(event) = notify_rx.recv().await else {
            warn!("notify channel closed; watcher exiting");
            return Ok(());
        };
        let Ok(event) = event else {
            continue;
        };
        let touches_us = event.paths.iter().any(|p| p == &config_path);
        if !touches_us {
            continue;
        }
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            continue;
        }

        // Drain any further events that arrive within the debounce window.
        // Editors typically emit a flurry of Create+Modify+Remove during save.
        let deadline = tokio::time::Instant::now() + debounce;
        while tokio::time::timeout_at(deadline, notify_rx.recv())
            .await
            .is_ok()
        {}

        match GatewayConfig::load_from(&config_path) {
            Ok(new_cfg) => {
                let old_v = serde_json::to_value(&current).unwrap_or(Value::Null);
                let new_v = serde_json::to_value(&new_cfg).unwrap_or(Value::Null);
                let changed = diff_paths(&old_v, &new_v);
                if changed.is_empty() {
                    debug!("config touched but content unchanged — ignoring");
                    continue;
                }
                let plan = ReloadPlan::build(&changed);
                let decision = decide(&plan, mode);
                debug!(?changed, ?decision, "config change");

                match decision {
                    ReloadDecision::NoOp => {}
                    ReloadDecision::HotOnly { paths } => {
                        handles.apply_hot(&new_cfg, &paths);
                    }
                    ReloadDecision::Restart { reason } => {
                        handles.apply_hot(&new_cfg, &plan.hot_paths);
                        restart.request(&reason);
                    }
                    ReloadDecision::SkippedDueToRestartInHotMode { paths } => {
                        warn!(
                            paths = ?paths,
                            "hot mode rejected change because a restart-only path was modified"
                        );
                    }
                }

                current = new_cfg.clone();
                last_known_good = new_cfg;
            }
            Err(e) => {
                warn!(error = %e, snapshot = "rolling back to last known good", "config parse failed");
                // Last-known-good is the running config; we just don't update it.
                // Log enough detail that the operator can fix the file.
                let _ = &last_known_good; // explicitly preserved
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn diff_paths_finds_top_level_change() {
        let a = json!({ "listen": "127.0.0.1:8080", "defaults": { "model": "x" } });
        let b = json!({ "listen": "0.0.0.0:8080", "defaults": { "model": "x" } });
        assert_eq!(diff_paths(&a, &b), vec!["listen".to_string()]);
    }

    #[test]
    fn diff_paths_finds_nested_change() {
        let a = json!({ "defaults": { "model": "x", "memory_mb": 512 } });
        let b = json!({ "defaults": { "model": "y", "memory_mb": 512 } });
        assert_eq!(diff_paths(&a, &b), vec!["defaults.model".to_string()]);
    }

    #[test]
    fn diff_paths_finds_addition_and_removal() {
        let a = json!({ "x": 1 });
        let b = json!({ "y": 2 });
        let mut paths = diff_paths(&a, &b);
        paths.sort();
        assert_eq!(paths, vec!["x", "y"]);
    }

    #[test]
    fn diff_paths_array_change_emits_whole_field() {
        let a = json!({ "allowed_origins": ["http://a"] });
        let b = json!({ "allowed_origins": ["http://a", "http://b"] });
        assert_eq!(diff_paths(&a, &b), vec!["allowed_origins".to_string()]);
    }

    #[test]
    fn longest_match_picks_more_specific_rule() {
        // gateway.allowed_origins is a hot path even though "gateway" alone
        // would be restart-only (had we kept that rule).
        let r = longest_match("allowed_origins", RULES);
        assert_eq!(r.kind, ReloadKind::Hot);
    }

    #[test]
    fn unknown_path_defaults_to_restart() {
        let r = longest_match("invented.path", RULES);
        assert_eq!(r.kind, ReloadKind::Restart);
    }

    #[test]
    fn plan_routes_paths_by_kind() {
        let plan = ReloadPlan::build(&[
            "allowed_origins".to_string(),
            "listen".to_string(),
            "defaults.model".to_string(),
            "totally_unknown".to_string(),
        ]);
        assert_eq!(plan.hot_paths, vec!["allowed_origins", "defaults.model"]);
        assert_eq!(plan.restart_paths, vec!["listen", "totally_unknown"]);
    }

    #[test]
    fn decide_off_means_noop() {
        let plan = ReloadPlan::build(&["allowed_origins".into()]);
        assert!(matches!(
            decide(&plan, ReloadMode::Off),
            ReloadDecision::NoOp
        ));
    }

    #[test]
    fn decide_hot_only_when_no_restart_paths() {
        let plan = ReloadPlan::build(&["allowed_origins".into()]);
        match decide(&plan, ReloadMode::Hot) {
            ReloadDecision::HotOnly { paths } => assert_eq!(paths, vec!["allowed_origins"]),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn decide_hot_skips_when_restart_path_changed() {
        let plan = ReloadPlan::build(&["allowed_origins".into(), "listen".into()]);
        match decide(&plan, ReloadMode::Hot) {
            ReloadDecision::SkippedDueToRestartInHotMode { paths } => {
                assert_eq!(paths, vec!["listen"]);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn decide_hybrid_runs_hot_then_requests_restart() {
        let plan = ReloadPlan::build(&["allowed_origins".into(), "listen".into()]);
        match decide(&plan, ReloadMode::Hybrid) {
            ReloadDecision::Restart { reason } => assert_eq!(reason, vec!["listen"]),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn decide_restart_mode_always_restarts_on_change() {
        let plan = ReloadPlan::build(&["allowed_origins".into()]);
        assert!(matches!(
            decide(&plan, ReloadMode::Restart),
            ReloadDecision::Restart { .. }
        ));
    }

    #[test]
    fn handles_apply_hot_swaps_allowed_origins() {
        let cfg = GatewayConfig::default();
        let handles = ReloadHandles::from_config(&cfg);

        let mut new_cfg = cfg.clone();
        new_cfg.allowed_origins = vec!["https://example.com".into()];
        handles.apply_hot(&new_cfg, &["allowed_origins".to_string()]);

        let live = handles.allowed_origins.load();
        assert_eq!(&**live, &vec!["https://example.com".to_string()]);
    }
}
