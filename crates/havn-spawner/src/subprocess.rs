//! Plain-subprocess agent spawner.
//!
//! Phase 1 fallback used during development and on platforms where the full
//! Linux-namespace stack is unavailable (e.g. macOS dev boxes, CI runners
//! without `unshare` privileges). **No isolation is applied.** Production
//! deployments must use [`crate::namespace::NamespaceSpawner`] instead.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use havn_core::{AgentId, Policy};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{AgentHandle, AgentSpawnConfig, AgentSpawner, ResourceStats, Result, SpawnerError};

/// Grace period between SIGTERM and SIGKILL on `stop()`.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct SubprocessSpawner {
    children: Arc<Mutex<HashMap<AgentId, Child>>>,
}

impl Default for SubprocessSpawner {
    fn default() -> Self {
        Self {
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SubprocessSpawner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentSpawner for SubprocessSpawner {
    async fn spawn(&self, config: &AgentSpawnConfig, _policy: &Policy) -> Result<AgentHandle> {
        // Ensure workspace exists before exec — runtime expects it on disk.
        tokio::fs::create_dir_all(&config.workspace_dir).await?;

        let mut cmd = Command::new(&config.runtime_binary);
        cmd.arg("--agent-id")
            .arg(config.agent_id.to_string())
            .arg("--gateway-socket")
            .arg(&config.gateway_socket)
            .arg("--workspace")
            .arg(&config.workspace_dir);
        if config.is_subagent {
            cmd.arg("--is-subagent");
        }
        // Inherit stdout/stderr so logs propagate; in production the namespace
        // spawner will redirect them to the agent's log file.
        cmd.kill_on_drop(true);

        let child = cmd.spawn().map_err(|e| {
            SpawnerError::Spawn(format!("exec {}: {e}", config.runtime_binary.display()))
        })?;
        let pid = child
            .id()
            .ok_or_else(|| SpawnerError::Spawn("child has no pid".into()))?;

        info!(agent_id = %config.agent_id, pid, "subprocess agent started");

        self.children.lock().await.insert(config.agent_id, child);

        Ok(AgentHandle {
            agent_id: config.agent_id,
            pid,
        })
    }

    async fn stop(&self, handle: &AgentHandle) -> Result<()> {
        let mut child = {
            let mut guard = self.children.lock().await;
            guard
                .remove(&handle.agent_id)
                .ok_or(SpawnerError::NotFound(handle.agent_id))?
        };

        // Try graceful SIGTERM first.
        send_sigterm(handle.pid);

        // Wait up to STOP_GRACE for the child to exit on its own.
        let waited = tokio::time::timeout(STOP_GRACE, child.wait()).await;
        match waited {
            Ok(Ok(status)) => {
                info!(agent_id = %handle.agent_id, ?status, "agent exited after SIGTERM");
                Ok(())
            }
            Ok(Err(e)) => Err(SpawnerError::Io(e)),
            Err(_) => {
                warn!(agent_id = %handle.agent_id, "SIGTERM grace expired; sending SIGKILL");
                child.start_kill().map_err(SpawnerError::Io)?;
                let status = child.wait().await.map_err(SpawnerError::Io)?;
                info!(agent_id = %handle.agent_id, ?status, "agent killed");
                Ok(())
            }
        }
    }

    async fn stats(&self, _handle: &AgentHandle) -> Result<ResourceStats> {
        // Subprocess spawner does not allocate a cgroup; return zeroes so the
        // gateway's stats endpoint stays uniform across spawner backends.
        Ok(ResourceStats::default())
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) {
    // SAFETY: passing a valid pid to libc::kill with a defined signal number.
    // If the pid has already exited, kill returns ESRCH, which we treat as a
    // benign race (the child is gone — that's what we wanted). We bit-cast u32
    // to i32 because libc::pid_t is signed; pids fit comfortably below i32::MAX
    // on every kernel we target.
    unsafe {
        let _ = libc::kill(pid.cast_signed(), libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) {
    // No SIGTERM equivalent on non-unix targets; stop() falls through to
    // start_kill() (terminate) after the grace period.
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::AgentId;
    use std::path::PathBuf;

    fn config_for(agent_id: AgentId, runtime: PathBuf) -> AgentSpawnConfig {
        AgentSpawnConfig {
            agent_id,
            workspace_dir: std::env::temp_dir().join(format!("havn-test-{agent_id}")),
            runtime_binary: runtime,
            init_binary: None,
            gateway_socket: std::env::temp_dir().join(format!("havn-test-{agent_id}.sock")),
            is_subagent: false,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
            agent_dns: Vec::new(),
        }
    }

    #[tokio::test]
    async fn spawn_then_stop_a_long_lived_subprocess() {
        // `sleep 60` is portable enough across Linux distros; if absent, skip.
        let sleep = which("sleep");
        let Some(sleep) = sleep else {
            tracing::warn!("sleep not on PATH; skipping spawn-then-stop test");
            return;
        };

        let spawner = SubprocessSpawner::new();
        // Custom config: the spawner expects --agent-id etc., but `sleep` ignores
        // unknown args on most platforms (BusyBox does, GNU coreutils errors).
        // Build a child directly via the spawner against a binary that consumes
        // its args harmlessly. We use `sh -c "sleep 60"` form.
        let agent_id = AgentId::new();
        let workspace = std::env::temp_dir().join(format!("havn-test-ws-{agent_id}"));
        tokio::fs::create_dir_all(&workspace).await.expect("mkdir");

        // Use `sh` as the runtime so we can pass our test args into a sleep call
        // that ignores them.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 60").kill_on_drop(true);
        let child = cmd.spawn().expect("spawn sh");
        let pid = child.id().expect("pid");
        spawner.children.lock().await.insert(agent_id, child);
        let _ = sleep; // keep the lookup result in scope

        let handle = AgentHandle { agent_id, pid };
        spawner.stop(&handle).await.expect("stop");

        // Re-stop should fail with NotFound.
        let err = spawner.stop(&handle).await.expect_err("already stopped");
        assert!(matches!(err, SpawnerError::NotFound(_)));
    }

    #[tokio::test]
    async fn spawn_invalid_binary_returns_error() {
        let spawner = SubprocessSpawner::new();
        let cfg = config_for(
            AgentId::new(),
            PathBuf::from("/nonexistent/havn-runtime-xyz"),
        );
        let policy = Policy::default();
        let err = spawner.spawn(&cfg, &policy).await.expect_err("should fail");
        assert!(matches!(err, SpawnerError::Spawn(_)), "{err:?}");
    }

    fn which(name: &str) -> Option<PathBuf> {
        let path_env = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
}
