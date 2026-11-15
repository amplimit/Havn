//! `shell` tool — run a bash command in the agent's process context.
//!
//! Under `NamespaceSpawner` the shell runs inside the agent's namespaces,
//! cgroup, Landlock and seccomp confinement (spec §4.1), in the Landlock
//! domain of tool processes (`sandbox::confine_tool`). Under the dev-only
//! `SubprocessSpawner` it inherits whatever the runtime can do on the
//! host.

use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

use super::{Tool, ToolCtx, ToolResult};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

pub struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a bash command in the agent's environment. The command runs via \
         `sh -c` with the workspace as the working directory. 60-second \
         timeout. Stdout + stderr are returned (up to 256 KB total) along \
         with the exit code."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash one-liner." }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: command".into());
        };
        if command.trim().is_empty() {
            return ToolResult::Error("command must be non-empty".into());
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&ctx.workspace_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // A group of its own, so a timeout or a stopped turn ends every
        // process the command started, not only `sh`.
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        crate::sandbox::confine_tool(&mut cmd);

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return ToolResult::Error(format!("exec failed: {e}")),
        };
        let mut group = GroupKill(child.id());
        let output = match timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolResult::Error(format!("exec failed: {e}")),
            Err(_) => {
                return ToolResult::Error(format!(
                    "timed out after {}s",
                    COMMAND_TIMEOUT.as_secs()
                ));
            }
        };
        // It finished: what it left running in the background (a server
        // started with `&`) is the agent's to keep.
        group.0 = None;

        let mut combined = Vec::with_capacity(output.stdout.len() + output.stderr.len());
        combined.extend_from_slice(&output.stdout);
        if !output.stderr.is_empty() {
            combined.extend_from_slice(b"\n--- stderr ---\n");
            combined.extend_from_slice(&output.stderr);
        }
        let truncated = combined.len() > MAX_OUTPUT_BYTES;
        let slice = &combined[..combined.len().min(MAX_OUTPUT_BYTES)];
        let text = String::from_utf8_lossy(slice).into_owned();

        let exit = output
            .status
            .code()
            .map_or_else(|| "(killed by signal)".to_string(), |c| c.to_string());

        let header = format!("exit={exit}\n");
        let mut out = header;
        out.push_str(&text);
        if truncated {
            write!(
                out,
                "\n\n[output truncated at {MAX_OUTPUT_BYTES} bytes; full size {} bytes]",
                combined.len()
            )
            .ok();
        }

        if output.status.success() {
            ToolResult::Ok(out)
        } else {
            // Non-zero exit is still "the tool ran"; surface as Error so the LLM
            // sees `is_error=true` and can react.
            ToolResult::Error(out)
        }
    }
}

/// Kills the process group `0` leads when dropped, unless cleared: when a
/// command times out, or its turn is stopped and the call is dropped.
struct GroupKill(Option<u32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: kill has no memory-safety preconditions. The group
            // is the command's own (process_group(0)); once all of it has
            // exited the call fails with ESRCH and does nothing.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use std::sync::Arc;

    async fn ctx() -> ToolCtx {
        let pool = havn_db::agent::connect_in_memory().await.expect("db");
        let workspace = std::env::temp_dir().join(format!("havn-shell-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&workspace).await.expect("ws");
        ToolCtx {
            workspace_dir: workspace,
            agent_db: pool,
            http: reqwest::Client::new(),
            policy: Arc::new(Policy::default()),
            embedder: None,
        }
    }

    #[tokio::test]
    async fn ok_when_command_succeeds() {
        let r = ShellTool
            .execute(&ctx().await, json!({"command": "echo hi"}))
            .await;
        assert!(!r.is_error(), "{}", r.text());
        assert!(r.text().contains("exit=0"));
        assert!(r.text().contains("hi"));
    }

    #[tokio::test]
    async fn error_when_command_fails() {
        let r = ShellTool
            .execute(&ctx().await, json!({"command": "false"}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("exit=1"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_a_call_kills_what_the_command_started() {
        let ctx = ctx().await;
        let call = ShellTool.execute(
            &ctx,
            json!({"command": "sleep 30 & echo $! > bg.pid; wait"}),
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1500), call)
                .await
                .is_err(),
            "the command waits on its background sleep"
        );
        let pid = std::fs::read_to_string(ctx.workspace_dir.join("bg.pid")).expect("pid file");
        let stat = format!("/proc/{}/stat", pid.trim());
        let mut gone = false;
        for _ in 0..40 {
            // Gone, or a zombie waiting for a reaper that is not ours.
            gone = std::fs::read_to_string(&stat).map_or(true, |s| {
                s.rsplit(')')
                    .next()
                    .is_some_and(|rest| rest.trim_start().starts_with('Z'))
            });
            if gone {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(gone, "the background sleep outlived its stopped call");
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let r = ShellTool
            .execute(&ctx().await, json!({"command": "   "}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("non-empty"));
    }

    #[tokio::test]
    async fn captures_stderr() {
        let ctx = ctx().await;
        let r = ShellTool
            .execute(&ctx, json!({"command": "echo to-out; echo to-err 1>&2"}))
            .await;
        assert!(r.text().contains("to-out"));
        assert!(r.text().contains("to-err"));
    }
}
