//! `file_read`, `file_write`, `file_edit`, `file_find`, `file_search` and
//! `view_image` tools — workspace-scoped filesystem access.
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

/// `view_image` (issue #227): an image of the workspace shown to the
/// model with the tool's result. The gateway reads the file and applies the
/// image limits and the model's `vision`.
pub struct ViewImageTool;

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &'static str {
        "view_image"
    }

    fn description(&self) -> &'static str {
        "Look at an image in the agent's workspace: a screenshot you took, a chart you          drew, a picture you downloaded. PNG, JPEG, GIF or WebP. Path is relative to the          workspace root. The image is shown to you with this tool's result, when your          model can see images."
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
        match tokio::fs::symlink_metadata(&resolved).await {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return ToolResult::Error(format!("not a file: {path}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::Error(format!("file not found: {path}"));
            }
            Err(e) => return ToolResult::Error(format!("read failed: {e}")),
        }
        let mut head = [0_u8; 12];
        let read = match tokio::fs::File::open(&resolved).await {
            Ok(mut file) => {
                use tokio::io::AsyncReadExt as _;
                file.read(&mut head).await
            }
            Err(e) => Err(e),
        };
        let n = match read {
            Ok(n) => n,
            Err(e) => return ToolResult::Error(format!("read failed: {e}")),
        };
        if !is_image(&head[..n]) {
            return ToolResult::Error(format!("{path} is not a PNG, JPEG, GIF or WebP image"));
        }
        let path = Path::new(path)
            .components()
            .filter_map(|c| match c {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        ToolResult::Image {
            text: format!("The image {path}:"),
            path,
        }
    }
}

/// Whether `head`, a file's first bytes, starts a PNG, JPEG, GIF or WebP
/// image.
fn is_image(head: &[u8]) -> bool {
    head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        || head.starts_with(&[0xFF, 0xD8, 0xFF])
        || head.starts_with(b"GIF87a")
        || head.starts_with(b"GIF89a")
        || head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP"
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
        if let Ok(meta) = tokio::fs::metadata(&resolved).await {
            if meta.is_dir() {
                return ToolResult::Error(format!("{path} is a directory"));
            }
            if meta.len() > MAX_FILE_BYTES as u64 {
                return ToolResult::Error(format!(
                    "file is {} bytes (cap {MAX_FILE_BYTES})",
                    meta.len()
                ));
            }
        }
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
/// `old_string` and `new_string`. A `null` field counts as absent.
fn edits_of(input: &Value) -> Result<Vec<Edit>, String> {
    let field = |v: &Value, key: &str| v.get(key).filter(|x| !x.is_null()).cloned();
    let one = |v: &Value, which: &str| -> Result<Edit, String> {
        let text = |key: &str| {
            field(v, key)
                .and_then(|x| x.as_str().map(str::to_string))
                .ok_or_else(|| format!("{which}missing required field: {key}"))
        };
        Ok(Edit {
            old: text("old_string")?,
            new: text("new_string")?,
            all: field(v, "replace_all")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        })
    };
    let edits = match field(input, "edits") {
        Some(Value::Array(list)) => {
            if ["old_string", "new_string", "replace_all"]
                .iter()
                .any(|key| field(input, key).is_some())
            {
                return Err(
                    "give either edits or old_string and new_string, not both; with edits, \
                     each edit has its own replace_all"
                        .into(),
                );
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

/// `text` with every line end CRLF.
fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Lines (from 1) where `needle` starts in `text`, the first few of them.
fn lines_of(text: &str, needle: &str) -> Vec<usize> {
    text.match_indices(needle)
        .take(5)
        .map(|(at, _)| text[..at].matches('\n').count() + 1)
        .collect()
}

/// `text` with `edits` applied in order, each to the result of the one
/// before, and a summary; an error names the first edit that cannot apply
/// and leaves the result unbuilt. In a file whose line ends are all CRLF,
/// `old_string` written with LF also matches, and `new_string` gets CRLF.
fn apply_edits(text: &str, edits: &[Edit]) -> Result<(String, String), String> {
    let mut text = text.to_string();
    let is_crlf = text.contains("\r\n") && !text.replace("\r\n", "").contains('\n');
    let mut notes = Vec::new();
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
        let (old, new) = if is_crlf {
            let old = if text.contains(&edit.old) {
                edit.old.clone()
            } else {
                crlf(&edit.old)
            };
            (old, crlf(&edit.new))
        } else {
            (edit.old.clone(), edit.new.clone())
        };
        if old == new {
            return Err(format!("{which}old_string and new_string are the same"));
        }
        let count = text.matches(old.as_str()).count();
        match count {
            0 => {
                let order = if i > 0 {
                    " Edits apply in order: this one is matched against the text after \
                     the edits before it."
                } else {
                    ""
                };
                return Err(format!(
                    "{which}old_string was not found; it must match the file exactly, \
                     white space and indentation included (read the file again to see \
                     its current text).{order}"
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
        let size = (text.len() - count * old.len()).saturating_add(count.saturating_mul(new.len()));
        if size > MAX_FILE_BYTES {
            return Err(format!(
                "{which}the edited file would be {size} bytes (cap {MAX_FILE_BYTES})"
            ));
        }
        let lines = lines_of(&text, &old)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let more = if count > 5 { ", ..." } else { "" };
        notes.push(if edits.len() > 1 {
            format!("edit {}: line {lines}{more}", i + 1)
        } else {
            format!("line {lines}{more}")
        });
        text = if edit.all {
            text.replace(old.as_str(), &new)
        } else {
            text.replacen(old.as_str(), &new, 1)
        };
        total += count;
    }
    let where_ = if edits.len() > 1 {
        format!(
            "{}; each edit's lines are counted before it applied",
            notes.join("; ")
        )
    } else {
        notes.join("")
    };
    let report = format!(
        "{total} replacement{} (at {where_})",
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

/// The field of a `file_find` or `file_search` input in which the registry
/// passes the patterns of the hook rules about `file_read`.
pub const GUARDED_READS: &str = "guarded_reads";

/// Most directory entries one `file_find` or `file_search` looks at.
const MAX_WALK_ENTRIES: usize = 50_000;
/// Largest file `file_search` reads.
const MAX_SEARCH_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Results returned unless the call asks for fewer, and the most it may ask for.
const DEFAULT_RESULTS: usize = 100;
const MAX_RESULTS: usize = 500;
/// Characters of a matching line shown, and the most lines of context.
const MAX_LINE_CHARS: usize = 300;
const MAX_CONTEXT_LINES: usize = 3;
/// Links named in a result's notes.
const MAX_LINKS_NAMED: usize = 5;
/// Bytes looked at to tell a binary file.
const BINARY_SNIFF_BYTES: usize = 8192;
/// Most bytes of results one call returns, as `file_read` returns.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Whether `rel` (a path within the workspace, `/`-separated) is left out
/// of finds and searches: version control, and the runtime's own state
/// (the agent's database and `.havn`).
fn hidden(rel: &str) -> bool {
    let top = rel.split('/').next().unwrap_or("");
    rel.split('/').any(|part| part == ".git")
        || top == ".havn"
        || top == "agent.db"
        || top.starts_with("agent.db-")
}

/// A glob as a regex over workspace paths: `**` crosses directories, `*`
/// and `?` do not, `{a,b}` is either, `[...]` a set. A glob without `/`
/// matches a file's name in any directory. Case is ignored.
fn glob_regex(glob: &str) -> Result<regex::Regex, String> {
    let mut re = String::from(if glob.contains('/') { "^" } else { "^(?:.*/)?" });
    let chars: Vec<char> = glob.trim_start_matches("./").chars().collect();
    let mut i = 0;
    let mut braces = 0usize;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 1;
                if chars.get(i + 1) == Some(&'/') {
                    i += 1;
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '{' => {
                braces += 1;
                re.push_str("(?:");
            }
            '}' if braces > 0 => {
                braces -= 1;
                re.push(')');
            }
            ',' if braces > 0 => re.push('|'),
            '[' => {
                // A set: `!` or `^` first negates it, and a `]` right after
                // that is one of its characters. A `-` between two
                // characters is a range; anything else is taken as written.
                let mut j = i + 1;
                re.push('[');
                if matches!(chars.get(j), Some('!' | '^')) {
                    re.push('^');
                    j += 1;
                }
                let first = j;
                loop {
                    let Some(&c) = chars.get(j) else {
                        return Err(format!("unclosed '[' in {glob:?}"));
                    };
                    if c == ']' && j > first {
                        break;
                    }
                    let range = c == '-'
                        && j > first
                        && chars[j - 1] != '-'
                        && chars.get(j + 1).is_some_and(|n| *n != ']' && *n != '-');
                    if range {
                        re.push('-');
                    } else {
                        re.push_str(&regex::escape(&c.to_string()));
                    }
                    j += 1;
                }
                re.push(']');
                i = j;
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    if braces > 0 {
        return Err(format!("unclosed '{{' in {glob:?}"));
    }
    re.push('$');
    regex::RegexBuilder::new(&re)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("bad glob {glob:?}: {e}"))
}

/// A file a walk found.
struct Found {
    rel: String,
    path: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

/// What a walk saw besides files.
#[derive(Default)]
struct Walked {
    /// Links met, never followed.
    links: Vec<String>,
    /// Files that would have been in the result, left out because a hook
    /// rule about `file_read` guards them.
    guarded: usize,
    /// It stopped at [`MAX_WALK_ENTRIES`].
    stopped: bool,
}

/// The patterns in [`GUARDED_READS`] of `input`; a pattern that does not
/// compile guards everything, as a rule that cannot be checked.
fn guards_of(input: &Value) -> Vec<regex::Regex> {
    input
        .get(GUARDED_READS)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|p| {
            regex::Regex::new(p).unwrap_or_else(|_| regex::Regex::new("").expect("empty regex"))
        })
        .collect()
}

/// The directory or file `user_path` names in `workspace` (the whole
/// workspace when empty), with its path as the walk writes it (its names
/// joined by `/`, so `./a//b/.` is `a/b`); refused when it, or a link on
/// the way, leads out of it, or it is the runtime's own.
fn start_of(workspace: &Path, user_path: Option<&str>) -> Result<(PathBuf, String), String> {
    let Some(user_path) = user_path.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok((workspace.to_path_buf(), String::new()));
    };
    safe_join(workspace, user_path)?;
    let rel = Path::new(user_path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if rel.is_empty() {
        return Ok((workspace.to_path_buf(), String::new()));
    }
    if hidden(&rel) {
        return Err(format!("{user_path} is not searched"));
    }
    let joined = workspace.join(&rel);
    let root = workspace
        .canonicalize()
        .map_err(|e| format!("workspace: {e}"))?;
    let real = joined
        .canonicalize()
        .map_err(|_| format!("not found: {user_path}"))?;
    if !real.starts_with(&root) || is_link_on_way(workspace, &rel) {
        return Err(format!(
            "{user_path} leads out of the workspace or through a link"
        ));
    }
    Ok((joined, rel))
}

/// `rel` as seen from `base`, the folder a call looks in: a glob with `/`
/// matches from there.
fn under<'a>(rel: &'a str, base: &str) -> &'a str {
    if base.is_empty() {
        return rel;
    }
    rel.strip_prefix(base)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(rel)
}

/// Whether a component of `user_path` under `workspace` is a link.
fn is_link_on_way(workspace: &Path, user_path: &str) -> bool {
    let mut at = workspace.to_path_buf();
    Path::new(user_path).components().any(|c| {
        at.push(c);
        std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink())
    })
}

/// Every file under `start` (`rel` within the workspace), in name order,
/// handed to `each` until it returns false; links are noted, not followed.
fn walk(start: &Path, rel: &str, each: &mut dyn FnMut(Found) -> bool) -> Walked {
    let mut walked = Walked::default();
    let mut seen = 0usize;
    let mut stack: Vec<(PathBuf, String)> = vec![(start.to_path_buf(), rel.to_string())];
    while let Some((path, rel)) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            walked.links.push(rel);
            continue;
        }
        if meta.is_file() {
            let found = Found {
                rel,
                path,
                size: meta.len(),
                modified: meta.modified().ok(),
            };
            if !each(found) {
                return walked;
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        let mut children: Vec<(PathBuf, String)> = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            seen += 1;
            if seen > MAX_WALK_ENTRIES {
                walked.stopped = true;
                return walked;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if !hidden(&child) {
                children.push((entry.path(), child));
            }
        }
        // Popped from the end: reversed, they come out in name order.
        children.sort_by(|a, b| b.1.cmp(&a.1));
        stack.extend(children);
    }
    walked
}

/// The `limit` a call asks for, within bounds.
fn limit_of(input: &Value) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_RESULTS, |n| {
            usize::try_from(n).unwrap_or(MAX_RESULTS)
        })
        .clamp(1, MAX_RESULTS)
}

/// `bytes` as a person reads a size.
fn size_text(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    }
}

/// The notes closing a result: links not followed, a walk cut short.
fn walk_notes(walked: &Walked, out: &mut String) {
    if !walked.links.is_empty() {
        let named: Vec<&str> = walked
            .links
            .iter()
            .take(MAX_LINKS_NAMED)
            .map(String::as_str)
            .collect();
        let more = walked.links.len().saturating_sub(MAX_LINKS_NAMED);
        out.push_str(&format!(
            "\n(links not followed: {}{})",
            named.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }
    if walked.guarded > 0 {
        out.push_str("\n(some files were left out: the agent's rules guard reading them)");
    }
    if walked.stopped {
        out.push_str(&format!(
            "\n(stopped after {MAX_WALK_ENTRIES} entries: give a path to look in)"
        ));
    }
}

pub struct FileFindTool;

#[async_trait]
impl Tool for FileFindTool {
    fn name(&self) -> &'static str {
        "file_find"
    }

    fn description(&self) -> &'static str {
        "Find files in the agent's workspace by name, with a glob: '*.xlsx' \
         (a name in any folder), 'reports/2026-*', '**/*.{md,txt}'. Case is \
         ignored. Returns paths with size and modified time, newest first. \
         Links are listed, not followed; .git and the agent's own database \
         are left out. Use file_search to find text in files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob for the files' paths or names." },
                "path": { "type": "string", "description": "Folder to look in, relative to the workspace; the whole workspace when absent." },
                "limit": { "type": "integer", "description": "Most files listed (default 100, at most 500)." }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(pattern) = input.get("pattern").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: pattern".into());
        };
        let glob = match glob_regex(pattern) {
            Ok(g) => g,
            Err(e) => return ToolResult::Error(e),
        };
        let (start, rel) = match start_of(
            &ctx.workspace_dir,
            input.get("path").and_then(Value::as_str),
        ) {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(e),
        };
        let limit = limit_of(&input);
        let guards = guards_of(&input);
        let found = tokio::task::spawn_blocking(move || {
            let mut matches: Vec<Found> = Vec::new();
            let mut guarded = 0usize;
            let mut walked = walk(&start, &rel, &mut |f| {
                if glob.is_match(under(&f.rel, &rel)) {
                    if guards.iter().any(|g| g.is_match(&f.rel)) {
                        guarded += 1;
                    } else {
                        matches.push(f);
                    }
                }
                true
            });
            walked.guarded = guarded;
            (matches, walked)
        })
        .await;
        let Ok((mut matches, walked)) = found else {
            return ToolResult::Error("the search stopped unexpectedly".into());
        };
        matches.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.rel.cmp(&b.rel)));
        let total = matches.len();
        let mut out = if total == 0 {
            format!("no files match {pattern:?}")
        } else {
            let mut listed = String::new();
            for f in matches.iter().take(limit) {
                let when = f.modified.map_or_else(String::new, |m| {
                    chrono::DateTime::<chrono::Utc>::from(m)
                        .format("%Y-%m-%d %H:%M UTC")
                        .to_string()
                });
                let line = format!("{}  {}  {}", f.rel, size_text(f.size), when);
                if listed.len() + line.len() + 1 > MAX_OUTPUT_BYTES {
                    listed.push_str("\n(the list is cut short: narrow the pattern or the path)");
                    break;
                }
                if !listed.is_empty() {
                    listed.push('\n');
                }
                listed.push_str(&line);
            }
            listed
        };
        if total > limit {
            out.push_str(&format!(
                "\n({} more not shown: narrow the pattern or the path)",
                total - limit
            ));
        }
        walk_notes(&walked, &mut out);
        ToolResult::Ok(out)
    }
}

pub struct FileSearchTool;

#[async_trait]
impl Tool for FileSearchTool {
    fn name(&self) -> &'static str {
        "file_search"
    }

    fn description(&self) -> &'static str {
        "Find text in the files of the agent's workspace: a literal string, \
         or a regular expression with regex set. Look in one folder or file \
         with path, and only in files whose names match a glob with glob \
         ('*.md'). Returns path:line: text for each matching line, with \
         context lines when asked. Binary files, files over 2 MB, links, \
         .git and the agent's own database are skipped, and the result says \
         so. The text beside a document sent to the agent (inbox/*.txt) is \
         searched like any file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Text to find; a regular expression when regex is true." },
                "regex": { "type": "boolean", "description": "Treat pattern as a regular expression." },
                "ignore_case": { "type": "boolean", "description": "Ignore letter case." },
                "path": { "type": "string", "description": "Folder or file to look in, relative to the workspace; the whole workspace when absent." },
                "glob": { "type": "string", "description": "Only files whose paths or names match this glob." },
                "context": { "type": "integer", "description": "Lines shown before and after each match (0 to 3, default 0)." },
                "limit": { "type": "integer", "description": "Most matching lines listed (default 100, at most 500)." }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, ctx: &ToolCtx, input: Value) -> ToolResult {
        let Some(pattern) = input.get("pattern").and_then(Value::as_str) else {
            return ToolResult::Error("missing required field: pattern".into());
        };
        if pattern.is_empty() {
            return ToolResult::Error("pattern must be non-empty".into());
        }
        let flag = |k: &str| input.get(k).and_then(Value::as_bool).unwrap_or(false);
        let source = if flag("regex") {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let needle = match regex::RegexBuilder::new(&source)
            .case_insensitive(flag("ignore_case"))
            .size_limit(1 << 20)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolResult::Error(format!("bad regex: {e}")),
        };
        let only = match input.get("glob").and_then(Value::as_str) {
            Some(g) if !g.is_empty() => match glob_regex(g) {
                Ok(r) => Some(r),
                Err(e) => return ToolResult::Error(e),
            },
            _ => None,
        };
        let (start, rel) = match start_of(
            &ctx.workspace_dir,
            input.get("path").and_then(Value::as_str),
        ) {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(e),
        };
        let limit = limit_of(&input);
        let context = input
            .get("context")
            .and_then(Value::as_u64)
            .map_or(0, |n| usize::try_from(n).unwrap_or(MAX_CONTEXT_LINES))
            .min(MAX_CONTEXT_LINES);
        let guards = guards_of(&input);
        let searched = tokio::task::spawn_blocking(move || {
            let scope = Scope {
                start: &start,
                rel: &rel,
                guards: &guards,
                only: only.as_ref(),
            };
            search(&scope, &needle, context, limit)
        })
        .await;
        match searched {
            Ok(out) => ToolResult::Ok(out),
            Err(_) => ToolResult::Error("the search stopped unexpectedly".into()),
        }
    }
}

/// `text` within [`MAX_LINE_CHARS`], around the match at byte `at` when
/// there is one.
fn line_text(text: &str, at: Option<usize>) -> String {
    let text = text.trim_end_matches('\r');
    let count = text.chars().count();
    if count <= MAX_LINE_CHARS {
        return text.to_string();
    }
    let lead = MAX_LINE_CHARS / 5;
    let from = at
        .and_then(|b| text.get(..b))
        .map_or(0, |before| before.chars().count().saturating_sub(lead))
        .min(count - MAX_LINE_CHARS);
    let mut out = String::new();
    if from > 0 {
        out.push('\u{2026}');
    }
    out.extend(text.chars().skip(from).take(MAX_LINE_CHARS));
    if from + MAX_LINE_CHARS < count {
        out.push('\u{2026}');
    }
    out
}

/// Where a search looks.
struct Scope<'a> {
    start: &'a Path,
    rel: &'a str,
    guards: &'a [regex::Regex],
    /// Only files whose paths match this glob.
    only: Option<&'a regex::Regex>,
}

/// The start of the file at `path`, then the rest unless the start shows
/// it binary (a NUL byte); text that is not UTF-8 is read with
/// replacement characters.
fn read_text(path: &Path) -> Option<Result<String, ()>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut bytes = vec![0u8; BINARY_SNIFF_BYTES];
    let mut got = 0;
    while got < bytes.len() {
        match file.read(&mut bytes[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return None,
        }
    }
    bytes.truncate(got);
    if bytes.contains(&0) {
        return Some(Err(()));
    }
    file.take(MAX_SEARCH_FILE_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(Ok(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }))
}

/// The lines matching `needle` in the files of `scope`, as
/// `path:line: text`, context lines as `path-line- text`, a `--` between
/// groups that are apart.
fn search(scope: &Scope<'_>, needle: &regex::Regex, context: usize, limit: usize) -> String {
    let only = scope.only;
    let mut out: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut matched = 0usize;
    let (mut cut, mut full) = (false, false);
    let (mut binary, mut large, mut files) = (0usize, 0usize, 0usize);
    let mut guarded = 0usize;
    let mut walked = walk(scope.start, scope.rel, &mut |f| {
        if only.is_some_and(|g| !g.is_match(under(&f.rel, scope.rel))) {
            return true;
        }
        if scope.guards.iter().any(|g| g.is_match(&f.rel)) {
            guarded += 1;
            return true;
        }
        if f.size > MAX_SEARCH_FILE_BYTES {
            large += 1;
            return true;
        }
        let text = match read_text(&f.path) {
            None => return true,
            Some(Err(())) => {
                binary += 1;
                return true;
            }
            Some(Ok(text)) => text,
        };
        files += 1;
        let lines: Vec<&str> = text.lines().collect();
        let mut shown_to: Option<usize> = None;
        let mut push = |out: &mut Vec<String>, line: String| -> bool {
            if bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
                return false;
            }
            bytes += line.len() + 1;
            out.push(line);
            true
        };
        for (i, line) in lines.iter().enumerate() {
            let Some(hit) = needle.find(line) else {
                continue;
            };
            if matched == limit {
                cut = true;
                return false;
            }
            matched += 1;
            let from = i.saturating_sub(context).max(shown_to.unwrap_or(0));
            // A group apart from the one before it, in this file or another.
            if context > 0 && !out.is_empty() && shown_to.is_none_or(|to| from > to) {
                if !push(&mut out, "--".into()) {
                    full = true;
                    return false;
                }
            }
            let mut group: Vec<String> = Vec::new();
            for (j, l) in lines.iter().enumerate().take(i).skip(from) {
                group.push(format!("{}-{}- {}", f.rel, j + 1, line_text(l, None)));
            }
            group.push(format!(
                "{}:{}: {}",
                f.rel,
                i + 1,
                line_text(line, Some(hit.start()))
            ));
            let mut to = i + 1;
            for (j, l) in lines
                .iter()
                .enumerate()
                .take((i + 1 + context).min(lines.len()))
                .skip(i + 1)
            {
                if needle.is_match(l) {
                    break;
                }
                group.push(format!("{}-{}- {}", f.rel, j + 1, line_text(l, None)));
                to = j + 1;
            }
            for line in group {
                if !push(&mut out, line) {
                    full = true;
                    return false;
                }
            }
            shown_to = Some(to);
        }
        true
    });
    walked.guarded = guarded;
    let mut text = if matched == 0 {
        format!("no matches in {files} file(s)")
    } else {
        out.join("\n")
    };
    if cut {
        text.push_str(&format!(
            "\n(stopped at {limit} matching lines: narrow the pattern, the path or the glob)"
        ));
    }
    if full {
        text.push_str(&format!(
            "\n(stopped at {} KB of results: narrow the pattern, the path or the glob)",
            MAX_OUTPUT_BYTES / 1024
        ));
    }
    if binary > 0 || large > 0 {
        let mut skipped = Vec::new();
        if binary > 0 {
            skipped.push(format!("{binary} binary file(s)"));
        }
        if large > 0 {
            skipped.push(format!("{large} file(s) over 2 MB"));
        }
        text.push_str(&format!("\n(skipped {})", skipped.join(" and ")));
    }
    walk_notes(&walked, &mut text);
    text
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
            policy: Arc::new(Policy::default()),
            embedder: None,
        }
    }

    async fn put(ctx: &ToolCtx, rel: &str, content: &[u8]) {
        let path = ctx.workspace_dir.join(rel);
        tokio::fs::create_dir_all(path.parent().expect("parent"))
            .await
            .expect("dir");
        tokio::fs::write(path, content).await.expect("write");
    }

    fn text_of(r: ToolResult) -> String {
        match r {
            ToolResult::Ok(s) | ToolResult::Image { text: s, .. } => s,
            ToolResult::Error(e) => panic!("tool error: {e}"),
        }
    }

    #[tokio::test]
    async fn an_image_of_the_workspace_is_shown_by_its_path() {
        let ctx = ctx().await;
        put(
            &ctx,
            "shots/page.png",
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0],
        )
        .await;
        put(&ctx, "notes.txt", b"just text").await;
        let view = |path: &str| ViewImageTool.execute(&ctx, json!({"path": path}));
        match view("./shots//page.png").await {
            ToolResult::Image { path, .. } => assert_eq!(path, "shots/page.png"),
            other => panic!("{other:?}"),
        }
        for (path, why) in [
            ("notes.txt", "not a PNG"),
            ("missing.png", "not found"),
            ("shots", "not a file"),
            ("../x.png", ".."),
            ("/etc/x.png", "absolute"),
        ] {
            match view(path).await {
                ToolResult::Error(e) => assert!(e.contains(why), "{path}: {e}"),
                other => panic!("{path}: {other:?}"),
            }
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                ctx.workspace_dir.join("shots/page.png"),
                ctx.workspace_dir.join("link.png"),
            )
            .expect("link");
            assert!(view("link.png").await.is_error(), "a link is not followed");
        }
    }

    #[test]
    fn globs_match_names_anywhere_and_paths_from_the_root() {
        let m = |g: &str, p: &str| glob_regex(g).expect("glob").is_match(p);
        assert!(m("*.xlsx", "a/b/Report.XLSX"));
        assert!(m("*.xlsx", "q.xlsx"));
        assert!(!m("*.xlsx", "q.xlsx.txt"));
        assert!(m("reports/2026-*", "reports/2026-03.md"));
        assert!(!m("reports/2026-*", "old/reports/2026-03.md"));
        assert!(!m("reports/*", "reports/a/b.md"));
        assert!(m("**/*.{md,txt}", "notes.md"));
        assert!(m("**/*.{md,txt}", "a/b/notes.txt"));
        assert!(m("data/[!x]?.csv", "data/ab.csv"));
        assert!(!m("data/[!x]?.csv", "data/xb.csv"));
        assert!(m("报告*", "资料/报告-三月.docx"));
        assert!(glob_regex("a{b").is_err());
        assert!(glob_regex("a[b").is_err());
    }

    #[tokio::test]
    async fn file_find_lists_newest_first_and_leaves_out_internal_files() {
        let ctx = ctx().await;
        put(&ctx, "old.md", b"old").await;
        std::thread::sleep(std::time::Duration::from_millis(20));
        put(&ctx, "notes/new.md", b"new").await;
        put(&ctx, "agent.db", b"db").await;
        put(&ctx, "agent.db-wal", b"db").await;
        put(&ctx, ".git/HEAD.md", b"x").await;
        put(&ctx, ".havn/home/secret.md", b"x").await;
        let out = text_of(FileFindTool.execute(&ctx, json!({"pattern": "*.md"})).await);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(lines[0].starts_with("notes/new.md  3 B  "), "{out}");
        assert!(lines[1].starts_with("old.md  "), "{out}");
        let all = text_of(FileFindTool.execute(&ctx, json!({"pattern": "**"})).await);
        assert!(
            !all.contains("agent.db") && !all.contains(".havn") && !all.contains(".git"),
            "{all}"
        );
        let within = text_of(
            FileFindTool
                .execute(&ctx, json!({"pattern": "*", "path": "notes"}))
                .await,
        );
        assert_eq!(within.lines().count(), 1, "{within}");
        let few = text_of(
            FileFindTool
                .execute(&ctx, json!({"pattern": "*.md", "limit": 1}))
                .await,
        );
        assert!(few.contains("(1 more not shown"), "{few}");
        for bad in ["../x", "/etc", ".havn", "agent.db"] {
            assert!(
                matches!(
                    FileFindTool
                        .execute(&ctx, json!({"pattern": "*", "path": bad}))
                        .await,
                    ToolResult::Error(_)
                ),
                "{bad}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn links_are_named_and_never_followed() {
        let ctx = ctx().await;
        let outside = tempfile::tempdir().expect("dir");
        std::fs::write(outside.path().join("secret.md"), "password hunter2").expect("write");
        std::os::unix::fs::symlink(outside.path(), ctx.workspace_dir.join("out")).expect("link");
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            ctx.workspace_dir.join("s.md"),
        )
        .expect("link");
        put(&ctx, "in.md", b"hunter2 is here too").await;
        let found = text_of(FileFindTool.execute(&ctx, json!({"pattern": "*.md"})).await);
        assert!(!found.contains("secret.md"), "{found}");
        assert!(found.contains("links not followed: out, s.md"), "{found}");
        let hits = text_of(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "hunter2"}))
                .await,
        );
        assert_eq!(
            hits.lines().next(),
            Some("in.md:1: hunter2 is here too"),
            "{hits}"
        );
        assert!(!hits.contains("password"), "{hits}");
        for path in ["out", "out/secret.md", "s.md"] {
            assert!(
                matches!(
                    FileSearchTool
                        .execute(&ctx, json!({"pattern": "x", "path": path}))
                        .await,
                    ToolResult::Error(_)
                ),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn files_the_rules_guard_from_reading_are_left_out() {
        let ctx = ctx().await;
        put(&ctx, "secrets/key.md", b"token abc").await;
        put(&ctx, "notes.md", b"token def").await;
        let guarded = |mut input: Value| {
            input[GUARDED_READS] = json!(["^secrets/"]);
            input
        };
        let found = text_of(
            FileFindTool
                .execute(&ctx, guarded(json!({"pattern": "*.md"})))
                .await,
        );
        assert!(found.starts_with("notes.md  "), "{found}");
        assert!(!found.contains("secrets/key.md"), "{found}");
        assert!(
            found.contains("(some files were left out: the agent's rules guard reading them)"),
            "{found}"
        );
        let other = text_of(
            FileFindTool
                .execute(&ctx, guarded(json!({"pattern": "notes.*"})))
                .await,
        );
        assert!(
            !other.contains("left out"),
            "a guarded file the pattern misses is not counted: {other}"
        );
        let hits = text_of(
            FileSearchTool
                .execute(&ctx, guarded(json!({"pattern": "token"})))
                .await,
        );
        assert_eq!(hits.lines().next(), Some("notes.md:1: token def"), "{hits}");
        assert!(!hits.contains("abc"), "{hits}");
        let unchecked = text_of(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "token", GUARDED_READS: ["("]}))
                .await,
        );
        assert!(
            unchecked.starts_with("no matches"),
            "a rule that does not compile guards all: {unchecked}"
        );
    }

    #[test]
    fn glob_sets_take_their_characters_as_written() {
        let m = |g: &str, p: &str| glob_regex(g).expect("glob").is_match(p);
        assert!(m("[]a]x", "]x") && m("[]a]x", "ax"));
        assert!(m("[!]a]x", "bx") && !m("[!]a]x", "]x"));
        assert!(m("[a-c]", "b") && !m("[a-c]", "-"));
        assert!(m("[-a]", "-"));
        assert!(m("[&&]", "&") && m("[~~]", "~"));
        assert!(m("[\\]]", "\\]"));
        assert!(m("[一二]月.md", "二月.md"));
        assert!(m("[<>]x", "<x") && m("[<>]x", ">x"));
    }

    #[tokio::test]
    async fn any_way_of_writing_a_path_is_the_same_path() {
        let ctx = ctx().await;
        put(&ctx, "secrets/key.md", b"token abc").await;
        put(&ctx, ".havn/hooks.yaml", b"token def").await;
        put(&ctx, "a/b/c.md", b"token ghi").await;
        for path in [".//.havn", "./.havn/", ".havn/.", "agent.db", ".//agent.db"] {
            assert!(
                matches!(
                    FileSearchTool
                        .execute(&ctx, json!({"pattern": "token", "path": path}))
                        .await,
                    ToolResult::Error(_)
                ),
                "{path}"
            );
        }
        for path in [".//secrets", "secrets/.", "./secrets//", "secrets"] {
            let mut input = json!({"pattern": "token", "path": path});
            input[GUARDED_READS] = json!(["^secrets/"]);
            let out = text_of(FileSearchTool.execute(&ctx, input).await);
            assert!(!out.contains("abc"), "{path}: {out}");
        }
        let found = text_of(
            FileFindTool
                .execute(&ctx, json!({"pattern": "b/*.md", "path": "a//"}))
                .await,
        );
        assert!(
            found.starts_with("a/b/c.md  "),
            "a glob with / matches from the path looked in: {found}"
        );
        let all = text_of(
            FileFindTool
                .execute(&ctx, json!({"pattern": "**", "path": "."}))
                .await,
        );
        assert!(!all.contains(".havn"), "{all}");
    }

    #[tokio::test]
    async fn results_stop_at_a_size_and_long_lines_show_their_match() {
        let ctx = ctx().await;
        let line = format!("{}needle{}", "长".repeat(400), "尾".repeat(400));
        put(&ctx, "long.txt", line.as_bytes()).await;
        let out = text_of(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "needle"}))
                .await,
        );
        let first = out.lines().next().expect("a line");
        assert!(
            first.starts_with("long.txt:1: \u{2026}") && first.contains("needle"),
            "{first}"
        );
        assert!(first.chars().count() < MAX_LINE_CHARS + 20, "{first}");
        let many: String = (0..3000)
            .map(|i| format!("match {i} {}\n", "x".repeat(200)))
            .collect();
        put(&ctx, "many.txt", many.as_bytes()).await;
        let out = text_of(
            FileSearchTool
                .execute(
                    &ctx,
                    json!({"pattern": "match", "path": "many.txt", "limit": 500}),
                )
                .await,
        );
        assert!(out.len() < MAX_OUTPUT_BYTES + 512, "{}", out.len());
        assert!(out.contains("KB of results"), "{}", &out[out.len() - 200..]);
        put(&ctx, "latin1.txt", b"caf\xe9 needle here").await;
        let out = text_of(
            FileSearchTool
                .execute(
                    &ctx,
                    json!({"pattern": "needle here", "path": "latin1.txt"}),
                )
                .await,
        );
        assert!(
            out.starts_with("latin1.txt:1: caf\u{fffd} needle here"),
            "text not UTF-8 is still searched: {out}"
        );
    }

    #[tokio::test]
    async fn groups_in_different_files_are_set_apart() {
        let ctx = ctx().await;
        put(&ctx, "a.md", b"x\nhit\n").await;
        put(&ctx, "b.md", b"hit\ny\n").await;
        let out = text_of(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "hit", "context": 1}))
                .await,
        );
        assert_eq!(out, "a.md-1- x\na.md:2: hit\n--\nb.md:1: hit\nb.md-2- y");
    }

    #[tokio::test]
    async fn file_search_finds_text_with_context_and_says_what_it_skipped() {
        let ctx = ctx().await;
        put(
            &ctx,
            "inbox/合同.txt",
            "甲方：张三\n乙方：李四\n金额：一百万\n签署日期：三月\n".as_bytes(),
        )
        .await;
        put(&ctx, "notes.md", b"alpha\nBeta\ngamma\n").await;
        put(&ctx, "pic.png", b"\x89PNG\0\0binary").await;
        put(&ctx, "big.log", &vec![b'a'; 2 * 1024 * 1024 + 1]).await;
        let out = text_of(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "乙方", "context": 1}))
                .await,
        );
        assert!(out.starts_with("inbox/合同.txt-1- 甲方：张三\ninbox/合同.txt:2: 乙方：李四\ninbox/合同.txt-3- 金额：一百万"), "{out}");
        assert!(
            out.contains("(skipped 1 binary file(s) and 1 file(s) over 2 MB)"),
            "{out}"
        );
        let none = text_of(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "beta"}))
                .await,
        );
        assert!(none.starts_with("no matches in 2 file(s)"), "{none}");
        let folded = text_of(
            FileSearchTool
                .execute(
                    &ctx,
                    json!({"pattern": "beta", "ignore_case": true, "glob": "*.md"}),
                )
                .await,
        );
        assert_eq!(folded.lines().next(), Some("notes.md:2: Beta"), "{folded}");
        let re = text_of(
            FileSearchTool
                .execute(
                    &ctx,
                    json!({"pattern": "^(alpha|gamma)$", "regex": true, "path": "notes.md"}),
                )
                .await,
        );
        assert_eq!(re, "notes.md:1: alpha\nnotes.md:3: gamma");
        let literal = text_of(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "a|b"}))
                .await,
        );
        assert!(
            literal.starts_with("no matches"),
            "a literal, not a regex: {literal}"
        );
        let cut = text_of(
            FileSearchTool
                .execute(
                    &ctx,
                    json!({"pattern": "a", "path": "notes.md", "limit": 1}),
                )
                .await,
        );
        assert!(cut.contains("(stopped at 1 matching lines"), "{cut}");
        assert!(matches!(
            FileSearchTool
                .execute(&ctx, json!({"pattern": "(", "regex": true}))
                .await,
            ToolResult::Error(_)
        ));
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
        let result = try_auto_commit(&ws, &p, "a.txt", "file_write").await;
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
        let h = try_auto_commit(&ws, &p, "a.txt", "file_write").await;
        assert!(h.is_some(), "expected a commit hash on first write");
        // Identical re-write → "nothing to commit" → returns None.
        let h2 = try_auto_commit(&ws, &p, "a.txt", "file_write").await;
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

    #[test]
    fn crlf_files_stay_crlf_and_growth_is_capped() {
        let one = |old: &str, new: &str| Edit {
            old: old.into(),
            new: new.into(),
            all: false,
        };
        let (out, _) = apply_edits("a\r\nb\r\n", &[one("a", "a\nx")]).unwrap();
        assert_eq!(out, "a\r\nx\r\nb\r\n");
        assert!(apply_edits("a\r\nb\r\n", &[one("a\nb", "a\r\nb")]).is_err());
        let (mixed, _) = apply_edits("a\r\nb\nc", &[one("b\nc", "B\nC")]).unwrap();
        assert_eq!(mixed, "a\r\nB\nC", "a mixed file is matched as given");
        let all = Edit {
            old: "a".into(),
            new: "x".repeat(10_000),
            all: true,
        };
        let err = apply_edits(&"a".repeat(100_000), &[all]).unwrap_err();
        assert!(err.contains("would be"), "{err}");
        let (_, report) = apply_edits(
            "x\nfoo\nx\n",
            &[Edit {
                old: "x".into(),
                new: "y".into(),
                all: true,
            }],
        )
        .unwrap();
        assert_eq!(report, "2 replacements (at line 1, 3)");
    }

    #[test]
    fn null_fields_count_as_absent() {
        let edits =
            edits_of(&json!({"edits": null, "old_string": "a", "new_string": "b"})).unwrap();
        assert_eq!(edits.len(), 1);
        assert!(
            edits_of(
                &json!({"edits": [{"old_string": "a", "new_string": "b"}], "old_string": null})
            )
            .is_ok()
        );
        assert!(
            edits_of(
                &json!({"edits": [{"old_string": "a", "new_string": "b"}], "replace_all": true})
            )
            .is_err()
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
