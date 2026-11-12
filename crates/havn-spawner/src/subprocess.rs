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
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    AgentExit, AgentHandle, AgentSpawnConfig, AgentSpawner, ChildWatch, ResourceStats, Result,
    SpawnerError,
};

/// Grace period between SIGTERM and SIGKILL on `stop()`.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct SubprocessSpawner {
    children: Arc<Mutex<HashMap<AgentId, ChildWatch>>>,
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
        cmd.env_clear()
            .envs(crate::agent_environment(&config.env_passthrough, |name| {
                std::env::var_os(name)
            }));
        // Operator opt-out for kernels without Landlock; the runtime
        // refuses to start without Landlock otherwise. Forwarded on its
        // own because HAVN_* variables are never passed through.
        if std::env::var_os("HAVN_ALLOW_NO_LANDLOCK").as_deref() == Some(std::ffi::OsStr::new("1"))
        {
            cmd.env("HAVN_ALLOW_NO_LANDLOCK", "1");
        }
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
        // The agent dies with the gateway instead of outliving it.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt as _;
            let gateway_pid = std::process::id().cast_signed();
            // SAFETY: runs between fork and exec and only makes the
            // async-signal-safe prctl/getppid calls.
            unsafe {
                cmd.as_std_mut()
                    .pre_exec(move || crate::ns_setup::kill_with_parent(Some(gateway_pid)));
            }
        }

        // A previous instance the caller never stopped would otherwise be
        // forgotten here while it keeps running.
        let previous = self.children.lock().await.remove(&config.agent_id);
        if let Some(previous) = previous {
            warn!(agent_id = %config.agent_id, "stopping a previous instance before respawn");
            terminate(config.agent_id, &previous).await;
        }

        let child = cmd.spawn().map_err(|e| {
            SpawnerError::Spawn(format!("exec {}: {e}", config.runtime_binary.display()))
        })?;
        let pid = child
            .id()
            .ok_or_else(|| SpawnerError::Spawn("child has no pid".into()))?;

        info!(agent_id = %config.agent_id, pid, "subprocess agent started");

        let handle = AgentHandle::new(config.agent_id, pid);
        self.children
            .lock()
            .await
            .insert(config.agent_id, ChildWatch::start(child, &handle));
        Ok(handle)
    }

    async fn stop(&self, handle: &AgentHandle) -> Result<()> {
        let child = {
            let mut guard = self.children.lock().await;
            if guard
                .get(&handle.agent_id)
                .is_none_or(|c| !c.belongs_to(handle))
            {
                return Err(SpawnerError::NotFound(handle.agent_id));
            }
            guard
                .remove(&handle.agent_id)
                .ok_or(SpawnerError::NotFound(handle.agent_id))?
        };
        terminate(handle.agent_id, &child).await;
        Ok(())
    }

    async fn wait(&self, handle: &AgentHandle) -> Result<AgentExit> {
        let child = self
            .children
            .lock()
            .await
            .get(&handle.agent_id)
            .filter(|c| c.belongs_to(handle))
            .cloned()
            .ok_or(SpawnerError::NotFound(handle.agent_id))?;
        Ok(child.exited().await)
    }

    async fn stats(&self, _handle: &AgentHandle) -> Result<ResourceStats> {
        // Subprocess spawner does not allocate a cgroup; return zeroes so the
        // gateway's stats endpoint stays uniform across spawner backends.
        Ok(ResourceStats::default())
    }
}

/// SIGTERM, then SIGKILL once [`STOP_GRACE`] has passed. Returns at once
/// for a child that has already exited. Off unix there is no SIGTERM, so
/// the child is killed after the grace period.
async fn terminate(agent_id: AgentId, child: &ChildWatch) {
    child.terminate();
    match tokio::time::timeout(STOP_GRACE, child.exited()).await {
        Ok(exit) => info!(%agent_id, %exit, "agent exited"),
        Err(_) => {
            warn!(%agent_id, "SIGTERM grace expired; sending SIGKILL");
            child.kill();
            let exit = child.exited().await;
            info!(%agent_id, %exit, "agent killed");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
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
            env_passthrough: Vec::new(),
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
        let handle = AgentHandle::new(agent_id, child.id().expect("pid"));
        spawner
            .children
            .lock()
            .await
            .insert(agent_id, ChildWatch::start(child, &handle));
        let _ = sleep; // keep the lookup result in scope
        let waiter = {
            let spawner = SubprocessSpawner {
                children: Arc::clone(&spawner.children),
            };
            let handle = handle.clone();
            tokio::spawn(async move { spawner.wait(&handle).await })
        };
        tokio::task::yield_now().await;
        spawner.stop(&handle).await.expect("stop");
        let exit = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter resolves")
            .expect("join");
        // The waiter either got the exit or found the child already gone.
        #[cfg(unix)]
        if let Ok(exit) = exit {
            assert_eq!(exit, AgentExit::Signal(libc::SIGTERM));
        }
        #[cfg(not(unix))]
        let _ = exit;

        // Re-stop should fail with NotFound.
        let err = spawner.stop(&handle).await.expect_err("already stopped");
        assert!(matches!(err, SpawnerError::NotFound(_)));
    }

    async fn track(spawner: &SubprocessSpawner, script: &str) -> AgentHandle {
        let agent_id = AgentId::new();
        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let handle = AgentHandle::new(agent_id, child.id().expect("pid"));
        spawner
            .children
            .lock()
            .await
            .insert(agent_id, ChildWatch::start(child, &handle));
        handle
    }

    #[tokio::test]
    async fn wait_reports_a_child_that_exits_on_its_own() {
        let spawner = SubprocessSpawner::new();
        let handle = track(&spawner, "exit 3").await;

        let exit = tokio::time::timeout(Duration::from_secs(5), spawner.wait(&handle))
            .await
            .expect("exit observed without polling")
            .expect("tracked");
        assert_eq!(exit, AgentExit::Code(3));

        // Cleanup of an exited child is immediate and forgets it.
        tokio::time::timeout(Duration::from_secs(1), spawner.stop(&handle))
            .await
            .expect("no grace period for an exited child")
            .expect("stop");
        assert!(matches!(
            spawner.wait(&handle).await,
            Err(SpawnerError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn stop_ignores_a_handle_from_an_earlier_spawn() {
        let spawner = SubprocessSpawner::new();
        let handle = track(&spawner, "exec sleep 60").await;

        // Same pid, as after a pid is recycled, but an earlier spawn.
        let stale = AgentHandle {
            serial: handle.serial - 1,
            ..handle.clone()
        };
        assert!(matches!(
            spawner.stop(&stale).await,
            Err(SpawnerError::NotFound(_))
        ));
        spawner.stop(&handle).await.expect("current instance stops");
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
