//! `havn start` — launch the gateway by `exec`-ing the sibling
//! `havn-gateway` binary. The CLI process becomes the gateway; signals
//! reach it directly without an intermediate parent.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, anyhow};

/// Override for tests + non-default deployments. Useful when the gateway
/// binary lives outside the directory of the CLI binary (rare in
/// production, common in dev).
const ENV_GATEWAY_BIN: &str = "HAVN_GATEWAY_BIN";

pub fn run() -> anyhow::Result<()> {
    let bin = resolve_gateway_bin()?;
    println!("starting {}", bin.display());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = Command::new(&bin).args(std::env::args().skip(2)).exec();
        // Reached only if exec failed.
        Err(err).with_context(|| format!("exec {}", bin.display()))
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(&bin)
            .args(std::env::args().skip(2))
            .status()
            .with_context(|| format!("spawn {}", bin.display()))?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("gateway exited with {status}"))
        }
    }
}

/// Find the gateway binary. Resolution order:
/// 1. `$HAVN_GATEWAY_BIN` (explicit override).
/// 2. Sibling of the current `havn` binary (so a `cargo run --bin havn`
///    in the workspace finds `target/debug/havn-gateway` automatically).
pub(crate) fn resolve_gateway_bin() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os(ENV_GATEWAY_BIN) {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().context("locating current executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current exe has no parent: {}", exe.display()))?;
    Ok(dir.join("havn-gateway"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn env_override_wins_over_sibling_lookup() {
        // SAFETY: tests run in a single thread by default for env mutation,
        // but we guard with a unique env-var name and clean up.
        unsafe {
            std::env::set_var(ENV_GATEWAY_BIN, "/custom/path/havn-gateway");
        }
        let bin = resolve_gateway_bin().unwrap();
        assert_eq!(bin, PathBuf::from("/custom/path/havn-gateway"));
        unsafe {
            std::env::remove_var(ENV_GATEWAY_BIN);
        }
    }
}
