//! `havn doctor` — diagnostic checklist.
//!
//! Goal: tell the operator within 30 seconds *what's wrong* without
//! forcing them to grep logs or read the spec. Each check produces one
//! line: `[ok|warn|fail] <area> — <one-liner>`.
//!
//! Failure modes covered (the ones I personally hit while building havn):
//! - `HAVN_AGE_KEY` env var unset but the credentials table is non-empty
//!   → gateway refuses to boot.
//! - `runtime_binary` / `init_binary` paths in config point at stale
//!   targets (built debug-only, then `cargo clean`).
//! - cgroup v2 not mounted → spawner crashes per-agent.
//! - Stale PID file → `havn gateway start --detach` refuses.
//! - Database schema out of sync (migration didn't run).
//! - Gateway is up but no listen socket (rare, but happens when the
//!   OS denies bind).

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

use crate::api::Gateway;
use crate::paths::{self, CliConfig};

#[derive(Debug, Clone, Copy)]
enum Verdict {
    Ok,
    Warn,
    Fail,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "[ok]  ",
            Self::Warn => "[warn]",
            Self::Fail => "[fail]",
        }
    }
}

fn line(v: Verdict, area: &str, msg: &str) {
    println!("{} {area:<14} {msg}", v.tag());
}

pub async fn run() -> anyhow::Result<()> {
    let cfg = paths::load_config().unwrap_or_default();
    let mut fails = 0usize;
    let mut warns = 0usize;

    macro_rules! mark {
        ($v:expr) => {
            match $v {
                Verdict::Fail => fails += 1,
                Verdict::Warn => warns += 1,
                Verdict::Ok => {}
            }
        };
    }

    println!("havn doctor");
    println!();

    let v = check_config_present();
    mark!(v);
    let v = check_data_dir(&cfg);
    mark!(v);
    let v = check_age_key();
    mark!(v);
    let v = check_binaries();
    mark!(v);
    let v = check_cgroup_v2();
    mark!(v);
    let v = check_db(&cfg).await;
    mark!(v);
    let v = check_gateway_reachable(&cfg).await;
    mark!(v);

    println!();
    if fails == 0 && warns == 0 {
        println!("all good ✓");
    } else {
        println!("summary: {fails} fail, {warns} warn");
    }

    if fails > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn check_config_present() -> Verdict {
    match paths::config_path() {
        Some(p) => {
            line(Verdict::Ok, "config", &format!("found at {}", p.display()));
            Verdict::Ok
        }
        None => {
            line(
                Verdict::Warn,
                "config",
                "no config.toml found — running on compiled defaults. Run `havn setup`.",
            );
            Verdict::Warn
        }
    }
}

fn check_data_dir(cfg: &CliConfig) -> Verdict {
    let d = paths::data_dir(cfg);
    if !d.exists() {
        line(
            Verdict::Fail,
            "data dir",
            &format!("{} does not exist — gateway can't start", d.display()),
        );
        return Verdict::Fail;
    }
    // Probe writability with a dotfile.
    let probe = d.join(".havn_doctor_probe");
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            line(
                Verdict::Ok,
                "data dir",
                &format!("writable at {}", d.display()),
            );
            Verdict::Ok
        }
        Err(e) => {
            line(
                Verdict::Fail,
                "data dir",
                &format!("{} not writable: {e}", d.display()),
            );
            Verdict::Fail
        }
    }
}

fn check_age_key() -> Verdict {
    match std::env::var("HAVN_AGE_KEY") {
        Ok(s) if !s.is_empty() => {
            line(Verdict::Ok, "HAVN_AGE_KEY", "set");
            Verdict::Ok
        }
        Ok(_) => {
            line(Verdict::Warn, "HAVN_AGE_KEY", "set but empty");
            Verdict::Warn
        }
        Err(_) => {
            line(
                Verdict::Warn,
                "HAVN_AGE_KEY",
                "not set in this shell — gateway env may still have it (check via /proc on a running gateway). Required when credentials exist.",
            );
            Verdict::Warn
        }
    }
}

fn check_binaries() -> Verdict {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            line(
                Verdict::Fail,
                "binaries",
                &format!("can't locate self: {e}"),
            );
            return Verdict::Fail;
        }
    };
    let dir = exe.parent().unwrap_or(Path::new("."));
    let candidates = [
        ("havn-gateway", dir.join("havn-gateway")),
        ("havn-runtime", dir.join("havn-runtime")),
        ("havn-init", dir.join("havn-init")),
    ];
    let mut worst = Verdict::Ok;
    for (name, p) in candidates {
        if p.exists() {
            line(
                Verdict::Ok,
                "binaries",
                &format!("{name} → {}", p.display()),
            );
        } else {
            line(
                Verdict::Fail,
                "binaries",
                &format!(
                    "{name} missing at {} (rebuild via `cargo build --release`)",
                    p.display()
                ),
            );
            worst = Verdict::Fail;
        }
    }
    worst
}

fn check_cgroup_v2() -> Verdict {
    if !cfg!(target_os = "linux") {
        line(
            Verdict::Warn,
            "cgroup v2",
            "not Linux — agent isolation will degrade",
        );
        return Verdict::Warn;
    }
    if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        line(Verdict::Ok, "cgroup v2", "mounted at /sys/fs/cgroup");
        Verdict::Ok
    } else {
        line(
            Verdict::Fail,
            "cgroup v2",
            "/sys/fs/cgroup/cgroup.controllers missing — boot with `systemd.unified_cgroup_hierarchy=1` or use a host with cgroup v2",
        );
        Verdict::Fail
    }
}

async fn check_db(cfg: &CliConfig) -> Verdict {
    let db_path = match cfg.db_path.clone() {
        Some(p) => p,
        None => paths::data_dir(cfg).join("havn.db"),
    };
    if !db_path.exists() {
        line(
            Verdict::Warn,
            "database",
            &format!(
                "{} not yet created — first gateway start will create it",
                db_path.display()
            ),
        );
        return Verdict::Warn;
    }
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .read_only(true);
    let pool = match SqlitePool::connect_with(opts).await {
        Ok(p) => p,
        Err(e) => {
            line(Verdict::Fail, "database", &format!("connect failed: {e}"));
            return Verdict::Fail;
        }
    };
    let r: Result<i64, _> = sqlx::query_scalar(
        r"SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name IN ('users','agents','credentials')",
    )
    .fetch_one(&pool)
    .await;
    pool.close().await;
    match r {
        Ok(n) if n == 3 => {
            line(
                Verdict::Ok,
                "database",
                &format!("schema present at {}", db_path.display()),
            );
            Verdict::Ok
        }
        Ok(n) => {
            line(
                Verdict::Fail,
                "database",
                &format!("expected 3 core tables, found {n}; migrations didn't run cleanly"),
            );
            Verdict::Fail
        }
        Err(e) => {
            line(
                Verdict::Fail,
                "database",
                &format!("schema probe failed: {e}"),
            );
            Verdict::Fail
        }
    }
}

async fn check_gateway_reachable(cfg: &CliConfig) -> Verdict {
    let pid_path = paths::pid_file(cfg);
    let pid_alive = paths::read_pid(&pid_path)
        .ok()
        .flatten()
        .is_some_and(paths::pid_alive);

    let gw = match Gateway::from_env() {
        Ok(g) => g,
        Err(e) => {
            line(Verdict::Fail, "gateway api", &format!("client init: {e}"));
            return Verdict::Fail;
        }
    };
    match gw.get_json::<serde_json::Value>("/me").await {
        Ok(_) => {
            line(Verdict::Ok, "gateway api", &format!("{} responds", gw.base));
            Verdict::Ok
        }
        Err(_) if !pid_alive => {
            line(
                Verdict::Warn,
                "gateway api",
                "not running (no pid file or pid stale) — start it with `havn gateway start --detach`",
            );
            Verdict::Warn
        }
        Err(e) => {
            line(
                Verdict::Fail,
                "gateway api",
                &format!("pid alive but API unreachable at {}: {e}", gw.base),
            );
            Verdict::Fail
        }
    }
}
