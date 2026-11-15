//! `file_read`, `file_write` and `file_edit` tools — workspace-scoped
//! filesystem access.
//!
//! Path safety: reject absolute paths and any `..` parent-traversal
//! component. Landlock and the namespace spawner's minimal rootfs add
//! kernel-level enforcement that survives bugs in this code.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolResult};

/// Maximum bytes returned by `file_read` and accepted by `file_write`, and
/// the largest file `file_edit` reads or leaves.
const MAX_FILE_BYTES: usize = 256 * 1024;
/// Most replacements one `file_edit` call makes.
const MAX_EDITS: usize = 50;

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
                        try_auto_commit(&ctx.workspace_dir, &resolved, path, "file_write").await
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

pub struct FileEditTool;

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &'static str {
        "file_edit"
    }

    fn description(&self) -> &'static str {
        "Change part of a UTF-8 text file in the agent's workspace by replacing \
         exact text, instead of rewriting the whole file with file_write. \
         old_string must match the file exactly, white space and indentation \
         included, and occur once, unless replace_all is set; read the file first \
         and include enough surrounding lines to make it unique. For several \
         changes to one file, pass them as edits; they apply in order, all or \
         none. Path rules are those of file_write; the file must exist."
    }

    fn input_schema(&self) -> Value {
        let change = json!({
            "old_string": { "type": "string", "description": "Exact text to replace." },
            "new_string": { "type": "string", "description": "Text to put in its place." },
            "replace_all": {
                "type": "boolean",
                "description": "Replace every occurrence instead of requiring one."
            }
        });
        let mut properties = json!({
            "path": { "type": "string", "description": "Relative path within the workspace." },
            "edits": {
                "type": "array",
                "description": "Several replacements, applied in order; instead of old_string and new_string.",
                "maxItems": MAX_EDITS,
                "items": {
                    "type": "object",
                    "properties": change.clone(),
                    "required": ["old_string", "new_string"]
                }
            }
        });
        if let (Some(all), Some(one)) = (properties.as_object_mut(), change.as_object()) {
            all.extend(one.clone());
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["path"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: path".into());
        };
        let edits = match edits_of(&input) {
            Ok(edits) => edits,
            Err(e) => return ToolResult::Error(e),
        };
        let resolved = match safe_join(&ctx.workspace_dir, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e),
        };
        let bytes = match tokio::fs::read(&resolved).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::Error(format!(
                    "file not found: {path}; create it with file_write"
                ));
            }
            Err(e) => return ToolResult::Error(format!("read failed: {e}")),
        };
        if bytes.len() > MAX_FILE_BYTES {
            return ToolResult::Error(format!(
                "file is {} bytes (cap {MAX_FILE_BYTES})",
                bytes.len()
            ));
        }
        let Ok(original) = String::from_utf8(bytes) else {
            return ToolResult::Error("file is not valid UTF-8".into());
        };
        let (edited, report) = match apply_edits(&original, &edits) {
            Ok(done) => done,
            Err(e) => return ToolResult::Error(format!("{path}: {e}; the file was not changed")),
        };
        if edited.len() > MAX_FILE_BYTES {
            return ToolResult::Error(format!(
                "the edited file would be {} bytes (cap {MAX_FILE_BYTES}); the file was not changed",
                edited.len()
            ));
        }
        if let Err(e) = tokio::fs::write(&resolved, &edited).await {
            return ToolResult::Error(format!("write failed: {e}"));
        }
        let mut msg = format!("edited {path}: {report}");
        if auto_commit_enabled()
            && let Some(commit_msg) =
                try_auto_commit(&ctx.workspace_dir, &resolved, path, "file_edit").await
        {
            msg.push_str(&format!(" (committed: {commit_msg})"));
        }
        ToolResult::Ok(msg)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Edit {
    old: String,
    new: String,
    all: bool,
}

/// The replacements in a `file_edit` input: `edits`, or one given by
/// `old_string` and `new_string`.
fn edits_of(input: &Value) -> Result<Vec<Edit>, String> {
    let one = |v: &Value, which: &str| -> Result<Edit, String> {
        let text = |key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("{which}missing required field: {key}"))
        };
        Ok(Edit {
            old: text("old_string")?,
            new: text("new_string")?,
            all: v
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    };
    let edits = match input.get("edits") {
        Some(Value::Array(list)) => {
            if input.get("old_string").is_some() {
                return Err("give either edits or old_string and new_string, not both".into());
            }
            if list.len() > MAX_EDITS {
                return Err(format!("more than {MAX_EDITS} edits in one call"));
            }
            list.iter()
                .enumerate()
                .map(|(i, v)| one(v, &format!("edit {}: ", i + 1)))
                .collect::<Result<Vec<_>, _>>()?
        }
        Some(_) => return Err("edits must be an array".into()),
        None => vec![one(input, "")?],
    };
    if edits.is_empty() {
        return Err("edits is empty".into());
    }
    Ok(edits)
}

/// `text` with `edits` applied in order, and a summary; an error names the
/// first edit that cannot apply. A file with CRLF line ends is matched
/// with the edits' line ends converted when they do not match as given.
fn apply_edits(text: &str, edits: &[Edit]) -> Result<(String, String), String> {
    let mut text = text.to_string();
    let crlf = text.contains("\r\n");
    let mut lines = Vec::new();
    let mut total = 0;
    for (i, edit) in edits.iter().enumerate() {
        let which = if edits.len() > 1 {
            format!("edit {}: ", i + 1)
        } else {
            String::new()
        };
        if edit.old.is_empty() {
            return Err(format!("{which}old_string is empty"));
        }
        if edit.old == edit.new {
            return Err(format!("{which}old_string and new_string are the same"));
        }
        let (mut old, mut new) = (edit.old.clone(), edit.new.clone());
        if crlf && !text.contains(&old) && !old.contains("\r\n") && old.contains('\n') {
            old = old.replace('\n', "\r\n");
            new = new.replace("\r\n", "\n").replace('\n', "\r\n");
        }
        let count = text.matches(old.as_str()).count();
        match count {
            0 => {
                return Err(format!(
                    "{which}old_string was not found; it must match the file exactly, \
                     white space and indentation included (read the file again to see \
                     its current text)"
                ));
            }
            n if n > 1 && !edit.all => {
                return Err(format!(
                    "{which}old_string occurs {n} times; add surrounding lines to make it \
                     unique, or set replace_all to replace every occurrence"
                ));
            }
            _ => {}
        }
        if let Some(at) = text.find(old.as_str()) {
            lines.push(text[..at].matches('\n').count() + 1);
        }
        text = if edit.all {
            text.replace(old.as_str(), &new)
        } else {
            text.replacen(old.as_str(), &new, 1)
        };
        total += count;
    }
    let at = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let report = format!(
        "{total} replacement{} (at line {at})",
        if total == 1 { "" } else { "s" }
    );
    Ok((text, report))
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
async fn try_auto_commit(
    workspace: &Path,
    resolved: &Path,
    rel_path: &str,
    tool: &str,
) -> Option<String> {
    if !tokio::fs::try_exists(workspace.join(".git"))
        .await
        .unwrap_or(false)
    {
        return None;
    }
    // git add
    let add = git(workspace)
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
    let commit = git(workspace)
        .arg("commit")
        .arg("-q")
        .arg("-m")
        .arg(format!("[agent] {tool}: {rel_path} (havn)"))
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
    let hash = git(workspace)
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

/// `git -C <workspace>`, run as a tool's process: the workspace's git
/// config and hooks are the agent's to write.
fn git(workspace: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(workspace);
    #[cfg(target_os = "linux")]
    crate::sandbox::confine_tool(&mut cmd);
    cmd
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

    #[tokio::test]
    async fn edit_replaces_one_exact_occurrence() {
        let ctx = ctx().await;
        tokio::fs::write(ctx.workspace_dir.join("a.txt"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        let r = FileEditTool
            .execute(
                &ctx,
                json!({"path": "a.txt", "old_string": "two\n", "new_string": "2\n"}),
            )
            .await;
        assert!(!r.is_error(), "{}", r.text());
        assert!(
            r.text().contains("1 replacement (at line 2)"),
            "{}",
            r.text()
        );
        let now = tokio::fs::read_to_string(ctx.workspace_dir.join("a.txt"))
            .await
            .unwrap();
        assert_eq!(now, "one\n2\nthree\n");
    }

    #[tokio::test]
    async fn edit_refuses_missing_and_ambiguous_text_and_keeps_the_file() {
        let ctx = ctx().await;
        let file = ctx.workspace_dir.join("b.txt");
        tokio::fs::write(&file, "x = 1\nx = 1\n").await.unwrap();
        let missing = FileEditTool
            .execute(
                &ctx,
                json!({"path": "b.txt", "old_string": "y = 1", "new_string": "y = 2"}),
            )
            .await;
        assert!(missing.is_error());
        assert!(missing.text().contains("not found"), "{}", missing.text());
        let twice = FileEditTool
            .execute(
                &ctx,
                json!({"path": "b.txt", "old_string": "x = 1", "new_string": "x = 2"}),
            )
            .await;
        assert!(twice.text().contains("occurs 2 times"), "{}", twice.text());
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "x = 1\nx = 1\n"
        );
        let all = FileEditTool
            .execute(
                &ctx,
                json!({"path": "b.txt", "old_string": "x = 1", "new_string": "x = 2", "replace_all": true}),
            )
            .await;
        assert!(all.text().contains("2 replacements"), "{}", all.text());
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "x = 2\nx = 2\n"
        );
    }

    #[tokio::test]
    async fn several_edits_apply_all_or_none() {
        let ctx = ctx().await;
        let file = ctx.workspace_dir.join("c.txt");
        tokio::fs::write(&file, "alpha\nbeta\n").await.unwrap();
        let failed = FileEditTool
            .execute(
                &ctx,
                json!({"path": "c.txt", "edits": [
                    {"old_string": "alpha", "new_string": "ALPHA"},
                    {"old_string": "gamma", "new_string": "GAMMA"}
                ]}),
            )
            .await;
        assert!(failed.text().contains("edit 2:"), "{}", failed.text());
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "alpha\nbeta\n"
        );
        let done = FileEditTool
            .execute(
                &ctx,
                json!({"path": "c.txt", "edits": [
                    {"old_string": "alpha", "new_string": "ALPHA"},
                    {"old_string": "ALPHA\nbeta", "new_string": "ALPHA\nBETA"}
                ]}),
            )
            .await;
        assert!(!done.is_error(), "{}", done.text());
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "ALPHA\nBETA\n"
        );
    }

    #[tokio::test]
    async fn edit_matches_crlf_files_and_checks_paths() {
        let ctx = ctx().await;
        let file = ctx.workspace_dir.join("d.txt");
        tokio::fs::write(&file, "a\r\nb\r\nc\r\n").await.unwrap();
        let r = FileEditTool
            .execute(
                &ctx,
                json!({"path": "d.txt", "old_string": "a\nb", "new_string": "a\nB"}),
            )
            .await;
        assert!(!r.is_error(), "{}", r.text());
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "a\r\nB\r\nc\r\n"
        );
        for bad in [
            json!({"path": "../x", "old_string": "a", "new_string": "b"}),
            json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}),
            json!({"path": "d.txt", "old_string": "", "new_string": "b"}),
            json!({"path": "d.txt", "old_string": "a", "new_string": "a"}),
            json!({"path": "d.txt", "edits": []}),
            json!({"path": "d.txt", "edits": [], "old_string": "a", "new_string": "b"}),
        ] {
            assert!(
                FileEditTool.execute(&ctx, bad.clone()).await.is_error(),
                "{bad}"
            );
        }
    }
}
