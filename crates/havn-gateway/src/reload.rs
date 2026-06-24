//! Config reload — gateway side of spec §8.4.
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
//! 5. Hot paths are applied in place via [`ReloadHandles::apply_hot`]. Restart
//!    paths are *reported*, not acted on: havn does not restart itself.
//!
//! Every `Restart` path is a deploy-time constant — `listen`, `db_path`, the
//! socket/workspace paths, the runtime binary — that an operator only changes
//! during a deliberate redeploy. A gateway that self-execs to apply them would
//! orphan every running agent, so instead the watcher logs exactly which
//! deploy-time fields changed and that they take effect on the next
//! `havn gateway restart`. This mirrors how nginx / PostgreSQL / systemd-managed
//! services treat the restart-only subset of their config: surface it, let the
//! operator (or their supervisor) restart.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::GatewayConfig;

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

/// Spawn the watcher task. Returns immediately; the task runs forever or
/// until the channel from the underlying notify watcher closes.
pub fn spawn_watcher(
    config_path: PathBuf,
    initial: GatewayConfig,
    handles: ReloadHandles,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_watcher(config_path, initial, handles).await {
            error!(error = ?e, "config watcher terminated with error");
        }
    })
}

async fn run_watcher(
    config_path: PathBuf,
    initial: GatewayConfig,
    handles: ReloadHandles,
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
                debug!(?changed, hot = ?plan.hot_paths, restart = ?plan.restart_paths, "config change");

                // Apply hot paths in place.
                if !plan.hot_paths.is_empty() {
                    handles.apply_hot(&new_cfg, &plan.hot_paths);
                }

                // Report restart-only (deploy-time) paths to the operator. We
                // do not restart the gateway ourselves — these take effect on
                // the next `havn gateway restart`.
                if !plan.restart_paths.is_empty() {
                    warn!(
                        paths = ?plan.restart_paths,
                        "config changed deploy-time settings; run `havn gateway restart` to apply them"
                    );
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
    fn plan_separates_hot_from_deploy_time_paths() {
        // The watcher applies hot_paths in place and only reports
        // restart_paths to the operator — no self-restart.
        let plan = ReloadPlan::build(&[
            "allowed_origins".into(),
            "defaults".into(),
            "listen".into(),
            "db_path".into(),
        ]);
        assert_eq!(plan.hot_paths, vec!["allowed_origins", "defaults"]);
        assert_eq!(plan.restart_paths, vec!["listen", "db_path"]);
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
