//! `file_read` and `file_write` tools — workspace-scoped filesystem access.
//!
//! Phase 1 path safety: reject absolute paths and any `..` parent-traversal
//! component. Phase 1 has no Landlock; the namespace spawner (next vertical)
//! adds kernel-level enforcement that survives bugs in this code.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolResult};

/// Maximum bytes returned by `file_read` and accepted by `file_write`.
const MAX_FILE_BYTES: usize = 256 * 1024;

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &'static str {
        "file_read"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the agent's workspace. Path is relative \
         to the workspace root; '..' traversal and absolute paths are rejected. \
         Returns up to 256 KB."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative path within the workspace." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: path".into());
        };
        let resolved = match safe_join(&ctx.workspace_dir, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e),
        };
        match tokio::fs::read(&resolved).await {
            Ok(bytes) => {
                if bytes.len() > MAX_FILE_BYTES {
                    return ToolResult::Error(format!(
                        "file is {} bytes (cap {})",
                        bytes.len(),
                        MAX_FILE_BYTES
                    ));
                }
                match String::from_utf8(bytes) {
                    Ok(s) => ToolResult::Ok(s),
                    Err(_) => ToolResult::Error("file is not valid UTF-8".into()),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ToolResult::Error(format!("file not found: {path}"))
            }
            Err(e) => ToolResult::Error(format!("read failed: {e}")),
        }
    }
}

pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &'static str {
        "file_write"
    }

    fn description(&self) -> &'static str {
        "Write UTF-8 text to a file in the agent's workspace, overwriting any \
         existing content. Path is relative to the workspace root; '..' \
         traversal and absolute paths are rejected. Maximum content size 256 KB."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Relative path within the workspace." },
                "content": { "type": "string", "description": "UTF-8 text to write." }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: path".into());
        };
        let Some(content) = input.get("content").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: content".into());
        };
        if content.len() > MAX_FILE_BYTES {
            return ToolResult::Error(format!(
                "content is {} bytes (cap {})",
                content.len(),
                MAX_FILE_BYTES
            ));
        }

        let resolved = match safe_join(&ctx.workspace_dir, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e),
        };

        // Create parent directory if needed (still inside the workspace).
        if let Some(parent) = resolved.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::Error(format!("create_dir_all: {e}"));
        }

        match tokio::fs::write(&resolved, content).await {
            Ok(()) => {
                let mut msg = format!("wrote {} bytes to {}", content.len(), path);
                // Optional Aider-style auto-commit: when the workspace
                // is a git repo AND the operator opted in via env, stage
                // and commit the new content. Failure is non-fatal — a
                // failed commit shouldn't fail the write.
                if auto_commit_enabled() {
                    if let Some(commit_msg) =
                        try_auto_commit(&ctx.workspace_dir, &resolved, path).await
                    {
                        msg.push_str(&format!(" (committed: {commit_msg})"));
                    }
                }
                ToolResult::Ok(msg)
            }
            Err(e) => ToolResult::Error(format!("write failed: {e}")),
        }
    }
}

/// Read once at module load. Cheap: env_var lookup + bool parse.
/// Operator opts in via `HAVN_AUTO_COMMIT=1` (or `true`) — default off
/// because surprise commits in someone else's git repo would be
/// strictly worse than no auto-commit at all.
fn auto_commit_enabled() -> bool {
    matches!(
        std::env::var("HAVN_AUTO_COMMIT").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

/// If `workspace` is a git repo, stage `resolved` and commit. Returns
/// the short commit hash on success, `None` on any failure (the caller
/// surfaces neither — auto-commit failures are best-effort and don't
/// fail the underlying write).
async fn try_auto_commit(workspace: &Path, resolved: &Path, rel_path: &str) -> Option<String> {
    if !tokio::fs::try_exists(workspace.join(".git"))
        .await
        .unwrap_or(false)
    {
        return None;
    }
    // git add
    let add = tokio::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .arg("add")
        .arg(resolved)
        .output()
        .await
        .ok()?;
    if !add.status.success() {
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&add.stderr),
            "auto_commit: git add failed; skipping"
        );
        return None;
    }
    // git commit. -q to silence chatter; we surface the hash from a
    // follow-up rev-parse so we don't have to parse `git commit`'s
    // human-readable output.
    let commit = tokio::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .arg("commit")
        .arg("-q")
        .arg("-m")
        .arg(format!("[agent] file_write: {rel_path} (havn)"))
        .output()
        .await
        .ok()?;
    if !commit.status.success() {
        // "nothing to commit" is the common case when the file content
        // was identical — not an error.
        let stderr = String::from_utf8_lossy(&commit.stderr);
        if stderr.contains("nothing to commit") || stderr.contains("no changes") {
            return None;
        }
        tracing::debug!(stderr = %stderr, "auto_commit: git commit failed; skipping");
        return None;
    }
    let hash = tokio::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD")
        .output()
        .await
        .ok()?;
    if !hash.status.success() {
        return None;
    }
    let s = String::from_utf8(hash.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Join a user-provided path under `workspace`, rejecting absolute paths and
/// any component that could escape the workspace (`..`, root, prefix).
fn safe_join(workspace: &Path, user_path: &str) -> Result<PathBuf, String> {
    if user_path.is_empty() {
        return Err("path must be non-empty".into());
    }
    let candidate = Path::new(user_path);
    if candidate.is_absolute() {
        return Err("absolute paths are not allowed".into());
    }
    for comp in candidate.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err("parent-directory ('..') components are not allowed".into());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("root or prefix components are not allowed".into());
            }
        }
    }
    Ok(workspace.join(candidate))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use std::sync::Arc;

    async fn ctx() -> ToolCtx {
        let pool = havn_db::agent::connect_in_memory().await.expect("agent db");
        let workspace = std::env::temp_dir().join(format!("havn-files-{}", uuid::Uuid::now_v7()));
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
    async fn write_then_read_roundtrip() {
        let ctx = ctx().await;
        let w = FileWriteTool;
        let r = w
            .execute(&ctx, json!({"path": "notes.md", "content": "hello"}))
            .await;
        assert!(!r.is_error(), "{}", r.text());

        let r2 = FileReadTool
            .execute(&ctx, json!({"path": "notes.md"}))
            .await;
        assert_eq!(r2.text(), "hello");
    }

    #[tokio::test]
    async fn parent_traversal_rejected() {
        let ctx = ctx().await;
        let r = FileReadTool
            .execute(&ctx, json!({"path": "../etc/passwd"}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("'..'"));
    }

    #[tokio::test]
    async fn absolute_path_rejected() {
        let ctx = ctx().await;
        let r = FileReadTool
            .execute(&ctx, json!({"path": "/etc/passwd"}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("absolute"));
    }

    #[tokio::test]
    async fn write_creates_subdirectories() {
        let ctx = ctx().await;
        let r = FileWriteTool
            .execute(&ctx, json!({"path": "a/b/c.txt", "content": "deep"}))
            .await;
        assert!(!r.is_error(), "{}", r.text());
        let r2 = FileReadTool
            .execute(&ctx, json!({"path": "a/b/c.txt"}))
            .await;
        assert_eq!(r2.text(), "deep");
    }

    #[tokio::test]
    async fn write_oversize_rejected() {
        let ctx = ctx().await;
        let big = "x".repeat(MAX_FILE_BYTES + 1);
        let r = FileWriteTool
            .execute(&ctx, json!({"path": "big.txt", "content": big}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("cap"));
    }

    #[tokio::test]
    async fn read_missing_file_friendly_error() {
        let ctx = ctx().await;
        let r = FileReadTool
            .execute(&ctx, json!({"path": "does-not-exist.md"}))
            .await;
        assert!(r.is_error());
        assert!(r.text().contains("not found"));
    }

    #[tokio::test]
    async fn auto_commit_no_op_without_git_dir() {
        // When the workspace isn't a git repo, try_auto_commit returns
        // None silently — never errors, never tries to invoke git in
        // a way that fails noisily.
        let ws = std::env::temp_dir().join(format!("havn-files-noac-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&ws).await.expect("ws");
        let p = ws.join("a.txt");
        tokio::fs::write(&p, "hi").await.expect("write");
        let result = try_auto_commit(&ws, &p, "a.txt").await;
        assert!(result.is_none(), "no .git → no commit hash");
    }

    #[tokio::test]
    async fn auto_commit_succeeds_in_real_git_repo() {
        // Use the real `git` binary if available — skip silently when
        // not (CI without git is the only path that hits this branch;
        // dev machines and Linux CI both have it).
        if tokio::process::Command::new("git")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let ws = std::env::temp_dir().join(format!("havn-files-ac-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&ws).await.expect("ws");
        // Initialise a fresh repo with a stable identity so commits
        // don't trip on a missing user.email.
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@havn"],
            vec!["config", "user.name", "test"],
            vec!["commit", "--allow-empty", "-q", "-m", "init"],
        ] {
            let st = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&ws)
                .args(args)
                .output()
                .await
                .expect("git");
            assert!(st.status.success(), "git step failed: {st:?}");
        }
        let p = ws.join("a.txt");
        tokio::fs::write(&p, "first").await.expect("write");
        let h = try_auto_commit(&ws, &p, "a.txt").await;
        assert!(h.is_some(), "expected a commit hash on first write");
        // Identical re-write → "nothing to commit" → returns None.
        let h2 = try_auto_commit(&ws, &p, "a.txt").await;
        assert!(h2.is_none(), "no-op re-commit should return None");
    }
}
