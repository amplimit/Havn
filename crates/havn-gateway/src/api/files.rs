//! `POST /agents/:id/files` — a file the owner sends in the dashboard
//! chat (issue #106). It is saved in the agent's workspace under
//! `inbox/`, through [`crate::agent_files`] so that nothing the agent put
//! there is followed, and a PDF's text is saved next to it. The answer is
//! the [`Attachment`] the chat message then carries.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use chrono::Utc;
use havn_core::{Attachment, INBOX_DIR};
use serde::Deserialize;

use crate::AppState;
use crate::agent_files::WorkspaceDir;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

/// Largest file the chat takes.
pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
/// What an agent's inbox may hold in all, and in entries.
const MAX_INBOX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_INBOX_FILES: usize = 5000;
/// Bytes of a sent file's name kept in the saved one: with the stamp, a
/// number and `.txt` it stays under the 255 bytes a name can have.
const MAX_NAME_BYTES: usize = 200;
/// Room the inbox check keeps for a PDF's text, written after it.
const PDF_TEXT_BYTES: u64 = 8 * 1024 * 1024;
/// Free space a write leaves on the workspace's filesystem, at least,
/// counting only what an unprivileged user may use: the root gateway
/// does not eat into the blocks the filesystem reserves for root.
const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;
/// Header the dashboard sends with an upload. A page on another site
/// cannot set it without a preflight the gateway does not answer, so it
/// cannot post files here in the owner's name.
pub const UPLOAD_HEADER: &str = "x-havn-upload";

/// Uploads read and saved at once, across all agents.
static UPLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
/// Uploads of one user at once, so one user cannot hold all of them.
const PER_USER_UPLOADS: usize = 2;
/// How long an upload's body may take to arrive.
const UPLOAD_SECONDS: std::time::Duration = std::time::Duration::from_secs(120);

/// One of a user's [`PER_USER_UPLOADS`], given back when dropped.
struct UserUploads(havn_core::UserId);

impl UserUploads {
    fn running() -> &'static std::sync::Mutex<std::collections::HashMap<havn_core::UserId, usize>> {
        static RUNNING: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<havn_core::UserId, usize>>,
        > = std::sync::OnceLock::new();
        RUNNING.get_or_init(Default::default)
    }

    fn take(user: havn_core::UserId) -> Option<Self> {
        let mut running = Self::running()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = running.entry(user).or_insert(0);
        if *n >= PER_USER_UPLOADS {
            return None;
        }
        *n += 1;
        Some(Self(user))
    }
}

impl Drop for UserUploads {
    fn drop(&mut self) {
        let mut running = Self::running()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = running.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                running.remove(&self.0);
            }
        }
    }
}
/// One check-and-write at a time, so concurrent uploads cannot each find
/// room the others then take.
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    /// The file's name on the sender's side.
    pub name: String,
}

pub async fn upload(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Attachment>, ApiError> {
    if headers.get(UPLOAD_HEADER).is_none() {
        return Err(ApiError::Forbidden(format!(
            "an upload carries the {UPLOAD_HEADER} header"
        )));
    }
    // Before the body is read: whoever may not send files here does not
    // get 25 MB buffered.
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let Some(_mine) = UserUploads::take(user.id) else {
        return Err(ApiError::Conflict(format!(
            "{PER_USER_UPLOADS} uploads of yours are still running; wait for one"
        )));
    };
    let slot = UPLOADS
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // A body sent slowly gives its slot up after a while.
    let body =
        match tokio::time::timeout(UPLOAD_SECONDS, axum::body::to_bytes(body, MAX_UPLOAD_BYTES))
            .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(_)) => {
                return Err(ApiError::BadRequest(format!(
                    "the upload broke off, or the file is over {} MB",
                    MAX_UPLOAD_BYTES / (1024 * 1024)
                )));
            }
            Err(_) => {
                return Err(ApiError::BadRequest(format!(
                    "the upload took longer than {}s",
                    UPLOAD_SECONDS.as_secs()
                )));
            }
        };
    if body.is_empty() {
        return Err(ApiError::BadRequest("the file is empty".into()));
    }
    let name = clean_name(&q.name);
    let media_type = media_type(&headers, &name);
    let workspace = crate::api::agents::workspace_for(&state, agent.id);
    crate::workspace::ensure(&workspace)
        .await
        .map_err(|e| ApiError::Internal(format!("workspace: {e}")))?;

    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let bytes = body.clone();
    let reserve = if media_type == "application/pdf" {
        PDF_TEXT_BYTES
    } else {
        0
    };
    let file = name.clone();
    let stored =
        tokio::task::spawn_blocking(move || save(&workspace, &stamp, &file, &bytes, reserve))
            .await
            .map_err(|e| ApiError::Internal(format!("saving the file: {e}")))??;
    // Saved: reading a PDF waits for its own converters, not for this.
    drop(slot);

    // A PDF's text, for the agent to read with file_read. A file whose
    // text cannot be read still arrives.
    let mut text_path = None;
    if media_type == "application/pdf" {
        match crate::web_fetch::pdf_text(&body).await {
            Ok((text, _)) if !text.trim().is_empty() => {
                let workspace = crate::api::agents::workspace_for(&state, agent.id);
                let text_name = format!("{}.txt", stored.file);
                let written = text_name.clone();
                let saved = tokio::task::spawn_blocking(move || {
                    let inbox = inbox(&workspace)?;
                    inbox.require_free(text.len() as u64 + MIN_FREE_BYTES)?;
                    inbox.create_new(&written, text.as_bytes())
                })
                .await;
                match saved {
                    Ok(Ok(true)) => text_path = Some(format!("{INBOX_DIR}/{text_name}")),
                    Ok(Ok(false)) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(agent_id = %agent.id, error = %e, "could not save a PDF's text")
                    }
                    Err(e) => {
                        tracing::warn!(agent_id = %agent.id, error = %e, "could not save a PDF's text")
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::info!(agent_id = %agent.id, error = %e, "no text read from an uploaded PDF")
            }
        }
    }

    let attachment = Attachment {
        path: format!("{INBOX_DIR}/{}", stored.file),
        name,
        media_type,
        size: body.len() as u64,
        text_path,
    };
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "file.uploaded",
        serde_json::json!({"path": attachment.path, "size": attachment.size, "media_type": attachment.media_type}),
    )
    .await;
    Ok(Json(attachment))
}

struct Stored {
    file: String,
}

fn inbox(workspace: &std::path::Path) -> std::io::Result<WorkspaceDir> {
    let root = WorkspaceDir::open(workspace)?;
    root.create_subdir_if_missing(INBOX_DIR)?;
    root.subdir(INBOX_DIR)?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "the inbox is not a directory")
    })
}

/// Save `bytes` in the inbox as `<stamp>-<name>`, numbered when that is
/// taken, unless the inbox would then hold more than its caps (keeping
/// `reserve` bytes for what goes with the file) or the filesystem too
/// little free space.
fn save(
    workspace: &std::path::Path,
    stamp: &str,
    name: &str,
    bytes: &[u8],
    reserve: u64,
) -> Result<Stored, ApiError> {
    let internal = |e: std::io::Error| ApiError::Internal(format!("saving the file: {e}"));
    let _saving = SAVING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inbox = inbox(workspace).map_err(internal)?;
    let entries = inbox.entry_count(MAX_INBOX_FILES + 1).map_err(internal)?;
    let held = inbox
        .regular_files(|_| true, MAX_INBOX_FILES + 1)
        .map_err(internal)?;
    let used: u64 = held.iter().map(|(_, meta)| meta.len()).sum();
    if entries >= MAX_INBOX_FILES || used + bytes.len() as u64 + reserve > MAX_INBOX_BYTES {
        return Err(ApiError::Conflict(
            "the agent's inbox is full; remove files from its inbox/ first".into(),
        ));
    }
    match inbox.require_free(bytes.len() as u64 + reserve + MIN_FREE_BYTES) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::StorageFull => {
            return Err(ApiError::Conflict(
                "the host is running out of disk space; the file was not saved".into(),
            ));
        }
        Err(e) => return Err(internal(e)),
    }
    for n in 0..100 {
        let file = if n == 0 {
            format!("{stamp}-{name}")
        } else {
            format!("{stamp}-{n}-{name}")
        };
        if inbox.create_new(&file, bytes).map_err(internal)? {
            return Ok(Stored { file });
        }
    }
    Err(ApiError::Conflict(
        "too many files of that name at once".into(),
    ))
}

/// `name` as a file name the inbox can hold: its last path component,
/// without control characters or a leading dot, at most
/// [`MAX_NAME_BYTES`] bytes, cut at a character in the part before its
/// extension, which is kept; `file` when nothing is left.
fn clean_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let whole: String = last.chars().filter(|c| !c.is_control()).collect();
    let (stem, ext) = match whole.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, format!(".{ext}")),
        _ => (whole.as_str(), String::new()),
    };
    let mut cleaned = String::new();
    for c in stem.chars() {
        if cleaned.len() + c.len_utf8() + ext.len() > MAX_NAME_BYTES {
            break;
        }
        cleaned.push(c);
    }
    cleaned.push_str(&ext);
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

/// The file's media type: what the browser said, unless that is generic,
/// then what its extension says.
fn media_type(headers: &HeaderMap, name: &str) -> String {
    let sent = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|v| {
            !v.is_empty()
                && v != "application/octet-stream"
                && v.len() <= 100
                && v.bytes().all(|b| b.is_ascii_graphic())
        });
    if let Some(sent) = sent {
        return sent;
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
    .into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_sent_name_becomes_one_the_inbox_can_hold() {
        assert_eq!(clean_name("report.pdf"), "report.pdf");
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(
            clean_name("C:\\Users\\me\\报告 2026.xlsx"),
            "报告 2026.xlsx"
        );
        assert_eq!(clean_name(".bashrc"), "bashrc");
        assert_eq!(clean_name("a\nb.txt"), "ab.txt");
        assert_eq!(clean_name(""), "file");
        assert_eq!(clean_name("..."), "file");
        assert_eq!(clean_name(&"x".repeat(300)).len(), MAX_NAME_BYTES);
        let long = clean_name(&"报".repeat(100));
        assert!(
            long.len() <= MAX_NAME_BYTES && long.chars().all(|c| c == '报'),
            "{long}"
        );
        let kept = clean_name(&format!("{}.pdf", "报".repeat(120)));
        assert!(
            kept.ends_with("报.pdf") && kept.len() <= MAX_NAME_BYTES,
            "{kept}"
        );
    }

    #[test]
    fn the_media_type_falls_back_to_the_extension() {
        let mut headers = HeaderMap::new();
        assert_eq!(media_type(&headers, "a.PDF"), "application/pdf");
        assert_eq!(media_type(&headers, "noext"), "application/octet-stream");
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        assert_eq!(media_type(&headers, "a.png"), "image/png");
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "Text/CSV; charset=utf-8".parse().unwrap(),
        );
        assert_eq!(media_type(&headers, "a.bin"), "text/csv");
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "text/\tplain".parse().unwrap(),
        );
        assert_eq!(media_type(&headers, "a.md"), "text/markdown");
    }

    #[test]
    fn a_user_runs_a_few_uploads_at_once() {
        let user = havn_core::UserId::new();
        let first = UserUploads::take(user).expect("first");
        let _second = UserUploads::take(user).expect("second");
        assert!(UserUploads::take(user).is_none());
        assert!(
            UserUploads::take(havn_core::UserId::new()).is_some(),
            "others are not held up"
        );
        drop(first);
        assert!(UserUploads::take(user).is_some());
    }

    #[test]
    fn files_are_saved_in_the_inbox_without_following_links() {
        let dir = tempfile::tempdir().expect("tmp");
        let first = save(dir.path(), "20261115-120000", "a.txt", b"one", 0).expect("save");
        assert_eq!(first.file, "20261115-120000-a.txt");
        let second = save(dir.path(), "20261115-120000", "a.txt", b"two", 0).expect("save");
        assert_eq!(second.file, "20261115-120000-1-a.txt");
        assert_eq!(
            std::fs::read(dir.path().join("inbox").join(&first.file)).unwrap(),
            b"one"
        );

        // An inbox the agent replaced with a link is not written through.
        let other = tempfile::tempdir().expect("tmp");
        let ws = tempfile::tempdir().expect("tmp");
        std::os::unix::fs::symlink(other.path(), ws.path().join("inbox")).expect("link");
        assert!(save(ws.path(), "20261115-120000", "a.txt", b"x", 0).is_err());
        assert_eq!(std::fs::read_dir(other.path()).unwrap().count(), 0);
    }
}
