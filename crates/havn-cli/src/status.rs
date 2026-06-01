//! `havn status` — system-wide overview.
//!
//! Runs through the gateway's REST API plus a few local probes. Designed
//! to answer the operator's first three questions on day one:
//!
//! 1. Is the gateway up? Where? For how long?
//! 2. How many agents do I have, and how many are alive right now?
//! 3. What LLM providers do I have credentials for?
//!
//! Anything that requires a deeper diagnosis (kernel features, decrypt
//! round-trip, DB integrity) belongs in `havn doctor`, not here.

use serde::Deserialize;

use crate::api::Gateway;
use crate::paths::{self, CliConfig};

#[derive(Debug, Deserialize)]
struct AgentBrief {
    name: String,
    status: String,
    connected: bool,
}

#[derive(Debug, Deserialize)]
struct CredentialBrief {
    provider: String,
    enabled: bool,
}

pub async fn run() -> anyhow::Result<()> {
    let cfg = paths::load_config().unwrap_or_default();

    print_gateway_block(&cfg);
    println!();
    print_api_blocks(&cfg).await;
    Ok(())
}

fn print_gateway_block(cfg: &CliConfig) {
    let pid_path = paths::pid_file(cfg);
    let listen = paths::listen_addr(cfg);
    println!("gateway");
    match paths::read_pid(&pid_path) {
        Ok(Some(pid)) if paths::pid_alive(pid) => {
            println!("  status:   running (pid {pid})");
        }
        Ok(Some(pid)) => {
            println!("  status:   stale pid {pid} (gateway not running, pid file exists)");
        }
        Ok(None) => {
            println!("  status:   no pid file (foreground mode, external supervisor, or stopped)");
        }
        Err(e) => {
            println!("  status:   pid file unreadable ({e})");
        }
    }
    println!("  listen:   {listen}");
    println!("  data_dir: {}", paths::data_dir(cfg).display());
    if let Some(p) = paths::config_path() {
        println!("  config:   {}", p.display());
    } else {
        println!("  config:   (defaults — no config.toml found)");
    }
}

async fn print_api_blocks(cfg: &CliConfig) {
    let gw = match Gateway::from_env() {
        Ok(g) => g,
        Err(e) => {
            println!("agents: (gateway client init failed — {e})");
            return;
        }
    };

    // The CLI's default Gateway URL doesn't read config.toml's listen
    // string, so when an operator's actual listen is non-default we tell
    // them what to set HAVN_GATEWAY to and skip the API blocks rather
    // than mislead with "0 agents" because we hit the wrong port.
    let configured_listen = paths::listen_addr(cfg);
    let default_url = format!("http://{configured_listen}");
    if !gw.base.contains(&configured_listen) && std::env::var("HAVN_GATEWAY").is_err() {
        println!("agents:    (skipped — config.toml listen is {configured_listen} but");
        println!(
            "           CLI default is {}. Set HAVN_GATEWAY={} and re-run.)",
            gw.base, default_url
        );
        return;
    }

    match gw.get_json::<Vec<AgentBrief>>("/agents").await {
        Ok(agents) => {
            let total = agents.len();
            let connected = agents.iter().filter(|a| a.connected).count();
            let by_status = agents.iter().fold(
                std::collections::BTreeMap::<&str, usize>::new(),
                |mut m, a| {
                    *m.entry(a.status.as_str()).or_default() += 1;
                    m
                },
            );
            println!("agents");
            println!("  total:     {total}");
            println!("  connected: {connected}");
            for (k, v) in &by_status {
                println!("  status.{k:<8} {v}");
            }
            if total > 0 && total <= 10 {
                println!();
                for a in &agents {
                    let dot = if a.connected { "●" } else { "○" };
                    println!("  {dot} {} ({})", a.name, a.status);
                }
            }
        }
        Err(e) => println!("agents: unreachable — {e}"),
    }

    println!();
    match gw.get_json::<Vec<CredentialBrief>>("/credentials").await {
        Ok(creds) => {
            let by_provider = creds.iter().fold(
                std::collections::BTreeMap::<&str, (usize, usize)>::new(),
                |mut m, c| {
                    let e = m.entry(c.provider.as_str()).or_default();
                    e.0 += 1;
                    if c.enabled {
                        e.1 += 1;
                    }
                    m
                },
            );
            println!("credentials");
            if by_provider.is_empty() {
                println!("  (none configured)");
            } else {
                for (p, (total, enabled)) in &by_provider {
                    println!("  {p:<12} {enabled}/{total} enabled");
                }
            }
        }
        Err(e) => println!("credentials: unreachable — {e}"),
    }
}
