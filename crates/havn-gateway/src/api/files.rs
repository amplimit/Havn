//! `POST /agents/:id/files` — a file the owner sends in the dashboard
//! chat (issue #106). It is saved in the agent's workspace under
//! `inbox/`, through [`crate::agent_files`] so that nothing the agent put
//! there is followed, and a PDF's text is saved next to it. The answer is
//! the [`Attachment`] the chat message then carries.

use axum::Json;
use axum::body::Bytes;
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
/// What an agent's inbox may hold in all, and in files.
const MAX_INBOX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_INBOX_FILES: usize = 5000;
/// Characters of a sent file's name kept in the saved one.
const MAX_NAME_CHARS: usize = 100;

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
    body: Bytes,
) -> Result<Json<Attachment>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
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
    let stored = tokio::task::spawn_blocking(move || save(&workspace, &stamp, &name, &bytes))
        .await
        .map_err(|e| ApiError::Internal(format!("saving the file: {e}")))??;

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
                    inbox(&workspace)?.create_new(&written, text.as_bytes())
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
        name: q.name.chars().take(MAX_NAME_CHARS).collect(),
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
/// taken, unless the inbox is full.
fn save(
    workspace: &std::path::Path,
    stamp: &str,
    name: &str,
    bytes: &[u8],
) -> Result<Stored, ApiError> {
    let internal = |e: std::io::Error| ApiError::Internal(format!("saving the file: {e}"));
    let inbox = inbox(workspace).map_err(internal)?;
    let held = inbox
        .regular_files(|_| true, MAX_INBOX_FILES + 1)
        .map_err(internal)?;
    let used: u64 = held.iter().map(|(_, meta)| meta.len()).sum();
    if held.len() >= MAX_INBOX_FILES || used + bytes.len() as u64 > MAX_INBOX_BYTES {
        return Err(ApiError::Conflict(
            "the agent's inbox is full; remove files from its inbox/ first".into(),
        ));
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
/// [`MAX_NAME_CHARS`] characters; `file` when nothing is left.
fn clean_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
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
        .filter(|v| !v.is_empty() && v != "application/octet-stream" && v.len() <= 100);
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
        assert_eq!(clean_name(&"x".repeat(300)).chars().count(), MAX_NAME_CHARS);
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
    }

    #[test]
    fn files_are_saved_in_the_inbox_without_following_links() {
        let dir = tempfile::tempdir().expect("tmp");
        let first = save(dir.path(), "20261115-120000", "a.txt", b"one").expect("save");
        assert_eq!(first.file, "20261115-120000-a.txt");
        let second = save(dir.path(), "20261115-120000", "a.txt", b"two").expect("save");
        assert_eq!(second.file, "20261115-120000-1-a.txt");
        assert_eq!(
            std::fs::read(dir.path().join("inbox").join(&first.file)).unwrap(),
            b"one"
        );

        // An inbox the agent replaced with a link is not written through.
        let other = tempfile::tempdir().expect("tmp");
        let ws = tempfile::tempdir().expect("tmp");
        std::os::unix::fs::symlink(other.path(), ws.path().join("inbox")).expect("link");
        assert!(save(ws.path(), "20261115-120000", "a.txt", b"x").is_err());
        assert_eq!(std::fs::read_dir(other.path()).unwrap().count(), 0);
    }
}
