//! Path resolution shared across the new ops subcommands
//! (`gateway`, `status`, `logs`, `doctor`).
//!
//! The CLI doesn't run as the gateway, so it can't ask the gateway in-process
//! where its files live. Instead it reads the same `config.toml` the gateway
//! does and derives PID / log paths from the configured `db_path`'s parent.
//!
//! Resolution order for `config.toml`:
//! 1. `$HAVN_CONFIG` (explicit override).
//! 2. `${XDG_CONFIG_HOME:-$HOME/.config}/havn/config.toml`.
//! 3. None — caller falls back to compiled defaults.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

const ENV_CONFIG: &str = "HAVN_CONFIG";

/// Subset of `GatewayConfig` we actually need on the CLI side. Defining a
/// narrower struct here means we don't take a havn-gateway dep on the CLI
/// crate (the CLI must still build when the gateway crate's deps churn).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CliConfig {
    pub listen: Option<String>,
    pub db_path: Option<PathBuf>,
}

/// Where `config.toml` is — None if it doesn't exist anywhere we look.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(ENV_CONFIG) {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let p = xdg.join("havn").join("config.toml");
    p.exists().then_some(p)
}

/// Load just the fields the CLI cares about. Errors are bubbled up — a
/// malformed config is the operator's bug to fix, not something to silently
/// fall back over.
pub fn load_config() -> anyhow::Result<CliConfig> {
    let Some(path) = config_path() else {
        return Ok(CliConfig::default());
    };
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Directory the gateway uses for state — derived from `db_path`'s parent.
/// Defaults to `$HOME/.local/share/havn` when no config exists.
#[must_use]
pub fn data_dir(cfg: &CliConfig) -> PathBuf {
    if let Some(db) = cfg.db_path.as_ref() {
        if let Some(parent) = db.parent() {
            return parent.to_path_buf();
        }
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/share/havn"))
        .unwrap_or_else(|| PathBuf::from("./havn-data"))
}

/// Path to the PID file written by `havn gateway start --detach`.
#[must_use]
pub fn pid_file(cfg: &CliConfig) -> PathBuf {
    data_dir(cfg).join("gateway.pid")
}

/// Path to the log file `havn gateway start --detach` redirects stdout/stderr
/// into. Foreground mode (`havn gateway start` without `--detach`) writes to
/// the calling terminal and ignores this file.
#[must_use]
pub fn log_file(cfg: &CliConfig) -> PathBuf {
    data_dir(cfg).join("gateway.log")
}

/// Listen address as a `host:port` string for diagnostics. Falls back to the
/// gateway's compiled-in default when config is absent.
#[must_use]
pub fn listen_addr(cfg: &CliConfig) -> String {
    cfg.listen
        .clone()
        .unwrap_or_else(|| "127.0.0.1:8080".into())
}

/// Read a UTF-8 PID from `path` and parse it. `Ok(None)` when the file
/// doesn't exist; `Err` for parse / IO failures (caller decides whether
/// that means "stale / corrupt — clean up" or "actual problem").
pub fn read_pid(path: &Path) -> anyhow::Result<Option<i32>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim();
            let pid = trimmed
                .parse::<i32>()
                .with_context(|| format!("PID file {} contains {trimmed:?}", path.display()))?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Live-process check using `kill(pid, 0)` — no signal sent, just probes
/// whether the kernel still has a process for that PID and we're allowed
/// to signal it. Returns `false` for ESRCH (no such process), `true` for
/// success or EPERM (exists but owned by someone else — still alive).
#[must_use]
pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: `kill(pid, 0)` is documented signal-safe; no memory passed.
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    // SAFETY: errno is per-thread on Linux/macOS.
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn data_dir_falls_back_when_db_path_absent() {
        let cfg = CliConfig::default();
        let d = data_dir(&cfg);
        // Either HOME-based or the relative fallback — both are absolute-or-
        // relative-path strings; what matters is no panic and a non-empty path.
        assert!(!d.as_os_str().is_empty());
    }

    #[test]
    fn data_dir_uses_db_path_parent() {
        let cfg = CliConfig {
            listen: None,
            db_path: Some(PathBuf::from("/var/lib/havn/havn.db")),
        };
        assert_eq!(data_dir(&cfg), PathBuf::from("/var/lib/havn"));
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let p = std::env::temp_dir().join("havn_test_no_such_pid_file");
        let _ = std::fs::remove_file(&p);
        assert!(read_pid(&p).expect("ok").is_none());
    }

    #[test]
    fn read_pid_parses_trimmed_decimal() {
        let p = std::env::temp_dir().join("havn_test_pid_file_42");
        std::fs::write(&p, " 4242\n").expect("write");
        assert_eq!(read_pid(&p).expect("ok"), Some(4242));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pid_alive_is_true_for_self_and_false_for_unused_high_pid() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let me = std::process::id() as i32;
        assert!(pid_alive(me), "current process must be alive");
        // PID 0x7fffffff is reserved as kernel sentinel on Linux; never a real proc.
        assert!(!pid_alive(0x7fff_fffe));
    }
}
