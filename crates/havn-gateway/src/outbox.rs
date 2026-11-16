//! `send_file` — a file an agent sends to the conversation it answers
//! (issue #106). The agent only names a file of its workspace; the
//! gateway reads it there (through [`crate::agent_files`], following
//! nothing the agent controls), keeps a copy in the agent's outbox, which
//! the agent cannot write, and sends the conversation an
//! [`havn_core::MessageContent::File`] with where to get it. How the file
//! reaches the person is the channel's business: the dashboard shows a
//! download, a channel adapter fetches it with its token and posts it the
//! way its platform does.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use havn_core::AgentId;
use havn_proto::GatewayToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const TOOL_NAME: &str = "send_file";

/// Largest file sent.
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// What an agent's outbox keeps at most, in bytes and files, and for how
/// long.
const MAX_OUTBOX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_OUTBOX_FILES: usize = 2000;
const KEEP_DAYS: i64 = 30;
/// Directories a sent file's path may go through.
const MAX_DEPTH: usize = 16;
/// Characters of a caption kept.
const MAX_CAPTION_CHARS: usize = 1000;
/// Free space a send leaves on the filesystem at least.
const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

#[must_use]
pub fn spec() -> GatewayToolSpec {
    GatewayToolSpec {
        name: TOOL_NAME.into(),
        description: "Send a file of your workspace to the person you are talking to, \
                      in this conversation: a report you wrote, a chart you made, a file \
                      they asked for. Give its path relative to your workspace, such as \
                      out/report.pdf. It reaches them the way their app shows files (a \
                      download in the dashboard, a document in a messaging app). Up to 25 MB."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The file's path relative to your workspace."
                },
                "caption": {
                    "type": "string",
                    "description": "A line to go with it, if any."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

/// A file an agent sent, as its outbox keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    /// The conversation it was sent to, and its chat.
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Agent `agent`'s outbox: next to its workspace, in the directory only
/// the gateway enters.
#[must_use]
pub fn dir(workspace_root: &Path, agent: AgentId) -> PathBuf {
    workspace_root.join(agent.to_string()).join("outbox")
}

/// The components of `path`, a file of the workspace, or why it is not
/// one the tool sends.
fn components(path: &str) -> Result<Vec<&str>, String> {
    let path = path.trim().trim_start_matches("./");
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(
            "give the file's path relative to your workspace, such as out/report.pdf".into(),
        );
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > MAX_DEPTH + 1
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == ".." || p.chars().any(char::is_control))
    {
        return Err(format!("{path:?} is not a path inside your workspace"));
    }
    Ok(parts)
}

/// Copy the file at `path` of `workspace` into `outbox` as sent to
/// `channel_id` (chat `conversation`). Blocking: it reads and writes
/// files.
pub fn send(
    workspace: &Path,
    outbox: &Path,
    path: &str,
    caption: Option<&str>,
    channel_id: &str,
    conversation: Option<&str>,
) -> Result<Sent, String> {
    let parts = components(path)?;
    let (name, dirs) = parts.split_last().ok_or("no file named")?;
    let mut dir = crate::agent_files::WorkspaceDir::open(workspace)
        .map_err(|e| format!("the workspace cannot be read: {e}"))?;
    for part in dirs {
        dir = dir
            .subdir(part)
            .map_err(|e| format!("{path:?} cannot be read: {e}"))?
            .ok_or_else(|| format!("there is no {path:?} in your workspace"))?;
    }
    let bytes = match dir.read_bytes(name, MAX_FILE_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(format!("there is no {path:?} in your workspace")),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Err(format!(
                "{path:?} is not a regular file of at most {} MB",
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }
        Err(e) => return Err(format!("{path:?} cannot be read: {e}")),
    };
    if bytes.is_empty() {
        return Err(format!("{path:?} is empty"));
    }

    make_dir(outbox).map_err(|e| format!("the outbox cannot be written: {e}"))?;
    // The root gateway leaves alone what the filesystem keeps for root.
    crate::agent_files::WorkspaceDir::open(outbox)
        .and_then(|dir| dir.require_free(bytes.len() as u64 + MIN_FREE_BYTES))
        .map_err(|_| "the host is running out of disk space".to_string())?;
    let kept = prune(outbox);
    let used: u64 = kept.iter().map(|s| s.size).sum();
    if kept.len() >= MAX_OUTBOX_FILES || used + bytes.len() as u64 > MAX_OUTBOX_BYTES {
        return Err("too many files were sent lately; try again later".into());
    }
    let sent = Sent {
        id: uuid::Uuid::now_v7().to_string(),
        name: (*name).to_string(),
        media_type: crate::api::files::type_of_name(name).to_string(),
        size: bytes.len() as u64,
        channel_id: channel_id.to_string(),
        conversation: conversation.map(str::to_string),
        caption: caption
            .map(|c| c.trim().chars().take(MAX_CAPTION_CHARS).collect::<String>())
            .filter(|c| !c.is_empty()),
        created_at: Utc::now(),
    };
    let meta = serde_json::to_vec(&sent).map_err(|e| e.to_string())?;
    // Written under a temporary name and moved into place once its
    // metadata is written: a file half written, or without metadata, is
    // never served, and the next send sweeps it.
    let part = outbox.join(format!("{}.part", sent.id));
    let saved = write_new(&part, &bytes)
        .and_then(|()| write_new(&outbox.join(format!("{}.json", sent.id)), &meta))
        .and_then(|()| std::fs::rename(&part, outbox.join(&sent.id)));
    if let Err(e) = saved {
        let _ = std::fs::remove_file(&part);
        remove(outbox, &sent.id);
        return Err(format!("saving the file failed: {e}"));
    }
    Ok(sent)
}

fn make_dir(outbox: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(outbox) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

/// What `outbox` keeps: its files' metadata, oldest first.
fn kept(outbox: &Path) -> Vec<Sent> {
    let mut kept = Vec::new();
    let Ok(entries) = std::fs::read_dir(outbox) else {
        return kept;
    };
    for entry in entries.flatten().take(MAX_OUTBOX_FILES * 3 + 10) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(sent) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Sent>(&b).ok())
            .filter(|s| uuid::Uuid::parse_str(&s.id).is_ok())
        {
            kept.push(sent);
        }
    }
    kept.sort_by_key(|s| s.created_at);
    kept
}

/// The files `outbox` keeps, after removing those older than
/// [`KEEP_DAYS`] and what a failed send left.
fn prune(outbox: &Path) -> Vec<Sent> {
    let oldest = Utc::now() - chrono::Duration::days(KEEP_DAYS);
    let (expired, kept): (Vec<Sent>, Vec<Sent>) = kept(outbox)
        .into_iter()
        .partition(|s| s.created_at < oldest);
    for sent in &expired {
        remove(outbox, &sent.id);
    }
    if let Ok(entries) = std::fs::read_dir(outbox) {
        for entry in entries.flatten().take(MAX_OUTBOX_FILES * 3 + 10) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".part").unwrap_or(&name);
            // A part another send is still writing is left alone.
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(3600));
            // A file another send is still writing or has just moved into
            // place is left alone: only old ones are swept.
            let orphan = uuid::Uuid::parse_str(stem).is_ok()
                && stale
                && (name.ends_with(".part") || !kept.iter().any(|s| s.id == stem));
            if orphan {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    kept
}

/// Remove file `id` of `outbox`, with its metadata.
pub fn remove(outbox: &Path, id: &str) {
    if uuid::Uuid::parse_str(id).is_err() {
        return;
    }
    let _ = std::fs::remove_file(outbox.join(id));
    let _ = std::fs::remove_file(outbox.join(format!("{id}.json")));
}

/// The files `outbox` keeps that were sent in chat `conversation` (or
/// all), oldest first, at most `limit`; it changes nothing.
pub fn list(outbox: &Path, conversation: Option<&str>, limit: usize) -> Vec<Sent> {
    let mut sent: Vec<Sent> = kept(outbox)
        .into_iter()
        .filter(|s| conversation.is_none_or(|c| s.conversation.as_deref() == Some(c)))
        .collect();
    let skip = sent.len().saturating_sub(limit);
    sent.split_off(skip)
}

/// The metadata of file `id` of `outbox`, when it is there.
pub fn meta(outbox: &Path, id: &str) -> Option<Sent> {
    uuid::Uuid::parse_str(id).ok()?;
    let meta = std::fs::read(outbox.join(format!("{id}.json"))).ok()?;
    serde_json::from_slice(&meta).ok()
}

/// File `id` of `outbox` and its bytes, when it is there.
pub fn read(outbox: &Path, id: &str) -> Option<(Sent, Vec<u8>)> {
    let sent = meta(outbox, id)?;
    let bytes = std::fs::read(outbox.join(id)).ok()?;
    Some((sent, bytes))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn only_paths_inside_the_workspace_are_sent() {
        assert_eq!(
            components("out/report.pdf").unwrap(),
            vec!["out", "report.pdf"]
        );
        assert_eq!(components("./a.txt").unwrap(), vec!["a.txt"]);
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a\\b",
            "a/./b",
            "a/\nb",
        ] {
            assert!(components(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_file_is_copied_to_the_outbox_without_following_links() {
        let ws = tempfile::tempdir().expect("ws");
        let root = tempfile::tempdir().expect("root");
        let outbox = root.path().join("outbox");
        std::fs::create_dir(ws.path().join("out")).expect("out");
        std::fs::write(ws.path().join("out/report.pdf"), b"%PDF-1.4 x").expect("pdf");
        let sent = send(
            ws.path(),
            &outbox,
            "out/report.pdf",
            Some(" here "),
            "c1",
            Some("chat"),
        )
        .expect("send");
        assert_eq!(sent.name, "report.pdf");
        assert_eq!(sent.media_type, "application/pdf");
        assert_eq!(sent.caption.as_deref(), Some("here"));
        let (again, bytes) = read(&outbox, &sent.id).expect("read");
        assert_eq!(again, sent);
        assert_eq!(bytes, b"%PDF-1.4 x");
        assert_eq!(list(&outbox, Some("chat"), 10), vec![sent.clone()]);
        assert!(list(&outbox, Some("other"), 10).is_empty());

        // Links the agent made are not followed, at any level.
        let secret = tempfile::tempdir().expect("secret");
        std::fs::write(secret.path().join("key"), b"root only").expect("key");
        std::os::unix::fs::symlink(secret.path().join("key"), ws.path().join("key")).expect("link");
        std::os::unix::fs::symlink(secret.path(), ws.path().join("dir")).expect("dirlink");
        assert!(send(ws.path(), &outbox, "key", None, "c1", None).is_err());
        assert!(send(ws.path(), &outbox, "dir/key", None, "c1", None).is_err());
        assert!(read(&outbox, "../x").is_none());

        // What a failed send left is swept by the next one.
        let stray = uuid::Uuid::now_v7().to_string();
        let part = outbox.join(format!("{stray}.part"));
        std::fs::write(&part, b"half").expect("part");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(&part)
            .expect("open")
            .set_modified(old)
            .expect("age");
        std::fs::write(outbox.join(&stray), b"no meta").expect("orphan");
        std::fs::File::options()
            .write(true)
            .open(outbox.join(&stray))
            .expect("open")
            .set_modified(old)
            .expect("age");
        let fresh = uuid::Uuid::now_v7().to_string();
        std::fs::write(outbox.join(&fresh), b"just moved in").expect("fresh");
        send(ws.path(), &outbox, "out/report.pdf", None, "c1", None).expect("send");
        assert!(!part.exists());
        assert!(!outbox.join(&stray).exists());
        assert!(outbox.join(&sent.id).exists(), "a kept file stays");
        assert!(
            outbox.join(&fresh).exists(),
            "a file another send is writing stays"
        );
    }
}
