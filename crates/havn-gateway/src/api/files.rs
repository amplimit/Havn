//! `POST /agents/:id/files` — a file the owner sends in the dashboard
//! chat (issue #106). It is saved in the agent's workspace under
//! `inbox/`, through [`crate::agent_files`] so that nothing the agent put
//! there is followed, and the text of a PDF, Word document or spreadsheet
//! is saved next to it, as is the copy of a picture a model cannot be
//! shown as it is (issue #225), and the pages of a PDF without a text
//! layer as images (issue #230). The answer is
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
/// Room the inbox check keeps for a document's text, written after it.
const TEXT_BYTES: u64 = 8 * 1024 * 1024;
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

/// Whose uploads one counts against: a user's in the dashboard, a channel
/// account's for the files its senders send, so those never take the
/// owner's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Uploader {
    User(havn_core::UserId),
    Account { channel: String, account: String },
}

/// One of an uploader's [`PER_USER_UPLOADS`], given back when dropped.
struct UserUploads(Uploader);

impl UserUploads {
    fn running() -> &'static std::sync::Mutex<std::collections::HashMap<Uploader, usize>> {
        static RUNNING: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<Uploader, usize>>,
        > = std::sync::OnceLock::new();
        RUNNING.get_or_init(Default::default)
    }

    fn take(uploader: Uploader) -> Option<Self> {
        let mut running = Self::running()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = running.entry(uploader.clone()).or_insert(0);
        if *n >= PER_USER_UPLOADS {
            return None;
        }
        *n += 1;
        Some(Self(uploader))
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
    let attachment = receive(
        &state,
        agent.id,
        Uploader::User(user.id),
        &q.name,
        &headers,
        body,
    )
    .await?;
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

/// Take file `name`, sent to agent `agent_id` and counted against
/// `quota`'s uploads at once, into the agent's inbox, with its text saved
/// next to it when it is a document; the attachment a message then names.
pub async fn receive(
    state: &AppState,
    agent_id: havn_core::AgentId,
    quota: Uploader,
    name: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<Attachment, ApiError> {
    let Some(_mine) = UserUploads::take(quota) else {
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
    let name = clean_name(name);
    let media_type = media_type(headers, &name);
    let workspace = crate::api::agents::workspace_for(state, agent_id);
    crate::workspace::ensure(&workspace)
        .await
        .map_err(|e| ApiError::Internal(format!("workspace: {e}")))?;

    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let bytes = body.clone();
    let text_kind = crate::web_fetch::Kind::of_file(&media_type, &name);
    // A picture a model cannot be shown as it is, too large or a HEIC
    // photo, gets a copy it can, shown in its place (issue #225).
    let copied = media_type.starts_with("image/") && crate::llm_proxy::needs_copy(&body);
    let reserve = if text_kind == Some(crate::web_fetch::Kind::Pdf) {
        TEXT_BYTES + crate::web_fetch::MAX_PAGES_BYTES as u64
    } else if text_kind.is_some() {
        TEXT_BYTES
    } else if copied {
        crate::llm_proxy::MAX_IMAGE_BYTES
    } else {
        0
    };
    let file = name.clone();
    let stored =
        tokio::task::spawn_blocking(move || save(&workspace, &stamp, &file, &bytes, reserve))
            .await
            .map_err(|e| ApiError::Internal(format!("saving the file: {e}")))??;
    // Saved: reading its text waits for its own converters, not for this.
    drop(slot);

    // A document's text, for the agent to read with file_read. A file
    // whose text cannot be read still arrives.
    let mut text_path = None;
    let mut scan = None;
    if let Some(kind) = text_kind {
        let text = match crate::web_fetch::document_text(kind, &body).await {
            Ok((text, _)) => text,
            Err(e) => {
                tracing::info!(agent_id = %agent_id, error = %e, "no text read from an uploaded document");
                String::new()
            }
        };
        // A PDF with too little text for its pages, a scan, is told as one,
        // with its pages as images (issue #230).
        if kind == crate::web_fetch::Kind::Pdf {
            let chars = text.chars().filter(|c| !c.is_whitespace()).count();
            scan = keep_pages(state, agent_id, &stored.file, &body, chars).await;
        }
        if !text.trim().is_empty() {
            text_path = keep(state, agent_id, text_name(&stored.file), text.into_bytes()).await;
        }
    }
    // An audio file's transcript (issue #164), kept as its text and shown
    // to the agent with it.
    let mut transcript = None;
    let how = (**state.transcription.load()).clone();
    if text_path.is_none()
        && body.len() <= crate::transcribe::MAX_AUDIO_BYTES
        && let Some(how) = how
        && let Some(format) = crate::transcribe::audio_format(&media_type, &name)
    {
        match transcribe_for(state, agent_id, &how, body.to_vec(), format).await {
            Ok(text) => {
                text_path = keep(
                    state,
                    agent_id,
                    text_name(&stored.file),
                    text.clone().into_bytes(),
                )
                .await;
                transcript = Some(text);
            }
            Err(why) => {
                tracing::info!(agent_id = %agent_id, why, "an audio file was not transcribed");
            }
        }
    }
    if copied {
        match crate::web_fetch::image_to_show(&body).await {
            Ok(image) => {
                let copy = crate::llm_proxy::shown_copy(&stored.file);
                keep(state, agent_id, copy, image).await;
            }
            Err(why) => {
                tracing::info!(agent_id = %agent_id, why, "no copy made of an image sent");
            }
        }
    }

    let attachment = Attachment {
        path: format!("{INBOX_DIR}/{}", stored.file),
        name,
        media_type,
        size: body.len() as u64,
        text_path,
        transcript,
        scan,
    };
    Ok(attachment)
}

/// The pages of PDF `body`, kept in the inbox as `file`, made images when
/// its text, of `chars` characters other than white space, is too little
/// for them (issue #230): how many it has and how many were kept. `None`
/// for a PDF with a text layer; with none kept when they could not be
/// made, and no text could be read either.
async fn keep_pages(
    state: &AppState,
    agent_id: havn_core::AgentId,
    file: &str,
    body: &[u8],
    chars: usize,
) -> Option<havn_core::Scan> {
    let pages = match crate::web_fetch::scanned_pages(body, chars).await {
        Ok(Some(pages)) => pages,
        Ok(None) => return None,
        Err(why) => {
            tracing::info!(agent_id = %agent_id, why, "the pages of a PDF were not made images");
            return (chars == 0).then_some(havn_core::Scan { pages: 0, shown: 0 });
        }
    };
    let mut shown = 0;
    for (n, image) in (1..).zip(pages.images) {
        let name = Attachment::page_image(file, n);
        if keep(state, agent_id, name, image).await.is_none() {
            break;
        }
        shown = n;
    }
    Some(havn_core::Scan {
        pages: pages.count,
        shown,
    })
}

/// Where the text of inbox file `file` is kept, next to it.
fn text_name(file: &str) -> String {
    format!("{file}.txt")
}

/// Keep `bytes`, made of an inbox file (its text, the copy of an image
/// shown), in the inbox as `name`: its path, or `None` when it could not
/// be saved.
async fn keep(
    state: &AppState,
    agent_id: havn_core::AgentId,
    name: String,
    bytes: Vec<u8>,
) -> Option<String> {
    let workspace = crate::api::agents::workspace_for(state, agent_id);
    let path = format!("{INBOX_DIR}/{name}");
    let saved = tokio::task::spawn_blocking(move || {
        let inbox = inbox(&workspace)?;
        inbox.require_free(bytes.len() as u64 + MIN_FREE_BYTES)?;
        inbox.create_new(&name, &bytes)
    })
    .await;
    match saved {
        Ok(Ok(true)) => Some(path),
        Ok(Ok(false)) => None,
        Ok(Err(e)) => {
            tracing::warn!(agent_id = %agent_id, error = %e, path, "could not save a file made of one sent");
            None
        }
        Err(e) => {
            tracing::warn!(agent_id = %agent_id, error = %e, path, "could not save a file made of one sent");
            None
        }
    }
}

/// Transcribe audio of agent `agent_id` with `how`: a model
/// is asked on the agent owner's credentials, within the agent's policy
/// and budget.
async fn transcribe_for(
    state: &AppState,
    agent_id: havn_core::AgentId,
    how: &crate::transcribe::Transcription,
    bytes: Vec<u8>,
    format: &str,
) -> Result<String, String> {
    let agent = havn_db::repo::agents::find_by_id(&state.db, agent_id)
        .await
        .map_err(|e| format!("agent lookup failed: {e}"))?
        .ok_or("the agent is gone")?;
    let models = state.models.load();
    let gateway = crate::transcribe::Gateway {
        db: &state.db,
        http: &state.http,
        keyring: state.keyring.as_ref(),
        llm: &state.llm,
        models: &models,
        multi_user: state.trusted_proxies.is_some(),
    };
    crate::transcribe::for_agent(gateway, &agent, how, bytes, format).await
}

/// `attachment`, a file of agent `agent_id`'s inbox a web chat message
/// names, with what the gateway kept for it: its transcript, none that the
/// page sent being taken (issue #164), and the pages of a scan kept as
/// images, as many as there are (issue #230).
pub async fn as_kept(
    state: &AppState,
    agent_id: havn_core::AgentId,
    mut attachment: Attachment,
) -> Attachment {
    attachment.transcript = None;
    let sent_scan = attachment.scan.take();
    let Some(file) = attachment
        .path
        .strip_prefix(INBOX_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_string)
    else {
        return attachment;
    };
    let workspace = crate::api::agents::workspace_for(state, agent_id);
    if crate::web_fetch::Kind::of_file(&attachment.media_type, &attachment.name)
        == Some(crate::web_fetch::Kind::Pdf)
    {
        let file = file.clone();
        let kept = tokio::task::spawn_blocking(move || {
            let inbox = inbox(&workspace).ok()?;
            let shown = (1..=havn_core::MAX_SCAN_PAGES)
                .take_while(|n| {
                    inbox
                        .has(&Attachment::page_image(&file, *n))
                        .unwrap_or(false)
                })
                .count() as u32;
            Some(shown)
        })
        .await
        .ok()
        .flatten();
        attachment.scan = match (sent_scan, kept) {
            // What the page sent, as far as the pages are there.
            (Some(sent), Some(kept)) => Some(havn_core::Scan {
                pages: sent.pages.max(kept),
                shown: sent.shown.min(kept),
            }),
            // A page that did not send it: the pages that are there.
            (None, Some(kept)) if kept > 0 => Some(havn_core::Scan {
                pages: kept,
                shown: kept,
            }),
            _ => None,
        };
        return attachment;
    }
    if crate::transcribe::audio_format(&attachment.media_type, &attachment.name).is_none() {
        return attachment;
    }
    // Where the gateway kept it, next to the file: not a path the page
    // names.
    let name = text_name(&file);
    let limit = (crate::transcribe::MAX_TRANSCRIPT_CHARS * 4) as u64;
    let read =
        tokio::task::spawn_blocking(move || inbox(&workspace)?.read_bytes(&name, limit)).await;
    if let Ok(Ok(Some(bytes))) = read {
        attachment.transcript = Some(
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(crate::transcribe::MAX_TRANSCRIPT_CHARS)
                .collect(),
        );
    }
    attachment
}

/// Remove a received file no message carried, its text and the copy shown
/// of it, from the inbox of `workspace`.
pub fn discard(workspace: &std::path::Path, attachment: &Attachment) {
    discard_path(workspace, &attachment.path, attachment.text_path.as_deref());
}

/// Remove inbox file `path` of `workspace`, its text at `text_path` and
/// the copy shown of it: a file sent unused. Nothing outside the inbox is
/// touched.
pub fn discard_path(workspace: &std::path::Path, path: &str, text_path: Option<&str>) {
    let Ok(inbox) = inbox(workspace) else {
        return;
    };
    let name = |path: &str| {
        path.strip_prefix(INBOX_DIR)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(str::to_string)
    };
    let file = name(path);
    let copy = file.as_deref().map(crate::llm_proxy::shown_copy);
    // A scan's pages, kept as images (issue #230).
    let pages = file.iter().flat_map(|file| {
        (1..=havn_core::MAX_SCAN_PAGES).map(move |n| Attachment::page_image(file, n))
    });
    let names = std::iter::once(path)
        .chain(text_path)
        .filter_map(name)
        .chain(copy)
        .chain(pages);
    for name in names {
        let _ = inbox.remove(&name);
    }
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
    type_of_name(name).into()
}

/// The media type a file name's extension says.
pub fn type_of_name(name: &str) -> &'static str {
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
        "heic" => "image/heic",
        "heif" => "image/heif",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsm" => "application/vnd.ms-excel.sheet.macroEnabled.12",
        "xls" => "application/vnd.ms-excel",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Deserialize)]
pub struct OutboxQuery {
    /// Only the files sent in this chat.
    #[serde(default)]
    pub conversation: Option<String>,
}

/// Where the caller's own files went: their web chats (and the results
/// tasks deliver there) and their linked chat-app chats (issue #184). The
/// files the agent sent other people in a chat app are theirs, not the
/// agent owner's.
async fn own_scopes(
    state: &AppState,
    user: &AuthedUser,
    agent: havn_core::AgentId,
) -> Result<Vec<String>, ApiError> {
    let mut scopes = vec![
        crate::webchat::session_for(&user.id.to_string(), agent).to_string(),
        "broadcast".to_string(),
    ];
    scopes.extend(
        crate::api::conversation::linked_scopes(&state.db, &state.bindings, user.id, agent)
            .await?
            .into_iter()
            .map(|l| l.scope),
    );
    Ok(scopes)
}

/// `GET /agents/:id/outbox` — the files the agent sent (issue #106) to
/// the caller, oldest first, for the chat that shows them.
pub async fn sent(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<OutboxQuery>,
) -> Result<Json<Vec<crate::outbox::Sent>>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let own = own_scopes(&state, &user, agent.id).await?;
    let outbox = crate::outbox::dir(&state.workspace_root, agent.id);
    let sent = tokio::task::spawn_blocking(move || {
        crate::outbox::list(&outbox, q.conversation.as_deref(), 200)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        sent.into_iter()
            .filter(|s| own.contains(&s.channel_id))
            .collect(),
    ))
}

/// `GET /agents/:id/outbox/:file` — a file the agent sent, as a download.
pub async fn download(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, file)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let own = own_scopes(&state, &user, agent.id).await?;
    let outbox = crate::outbox::dir(&state.workspace_root, agent.id);
    let found = tokio::task::spawn_blocking(move || crate::outbox::read(&outbox, &file))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .filter(|(sent, _)| own.contains(&sent.channel_id))
        .ok_or(ApiError::NotFound)?;
    Ok(attachment_response(&found.0, found.1))
}

/// `sent`'s bytes as a download the browser saves rather than shows: an
/// HTML or SVG file the agent made must not run as the dashboard's page.
pub fn attachment_response(sent: &crate::outbox::Sent, bytes: Vec<u8>) -> axum::response::Response {
    file_response(&sent.name, bytes, None)
}

/// File `name` as a download, or with `image` (a type its bytes were
/// sniffed as) as that image, shown in place. Neither is ever run as a
/// page: no sniffing, and a sandbox for anything that would render.
fn file_response(
    name: &str,
    bytes: Vec<u8>,
    image: Option<&'static str>,
) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse as _;
    let (kind, disposition) = match image {
        Some(kind) => (kind, "inline"),
        None => ("application/octet-stream", "attachment"),
    };
    // An image saved from the page is saved as what it is.
    let lower = name.to_ascii_lowercase();
    let says = |ext: &str| {
        lower.ends_with(&format!(".{ext}")) || (ext == "jpeg" && lower.ends_with(".jpg"))
    };
    let name = match image.and_then(|kind| kind.strip_prefix("image/")) {
        Some(ext) if !says(ext) => format!("{name}.{ext}"),
        _ => name.to_string(),
    };
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    (
        [
            (header::CONTENT_TYPE, kind.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("{disposition}; filename=\"file\"; filename*=UTF-8''{encoded}"),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "sandbox".to_string()),
            (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct InboxFileQuery {
    /// `inbox/<name>`, as an attachment names it.
    pub path: String,
    /// A picture as the copy the model is shown, if it has one.
    #[serde(default)]
    pub shown: bool,
}

/// `GET /agents/:id/files?path=inbox/<name>` — a file in the agent's inbox
/// (one sent in a chat, or its text), for the owner's chat to show: a
/// PNG, JPEG, GIF or WebP image in place, anything else as a download.
/// With `shown=true`, a picture with a copy the model is shown is that
/// copy, for a thumbnail.
/// Read without following links, as everything there may be the agent's.
pub async fn inbox_file(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<InboxFileQuery>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    // Another site's page cannot even learn whether a file is there.
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes() == b"cross-site")
    {
        return Err(ApiError::Forbidden("not from another site".into()));
    }
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    // Files held at once: each is read whole, and its slot is given back
    // once it has been sent.
    static READING: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let reading = READING
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(8)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if !Attachment::in_inbox(&q.path) {
        return Err(ApiError::BadRequest("not a file in the inbox".into()));
    }
    let name = q.path[INBOX_DIR.len() + 1..].to_string();
    let workspace = crate::api::agents::workspace_for(&state, agent.id);
    let read = name.clone();
    let shown = q.shown;
    let bytes = tokio::task::spawn_blocking(move || {
        let dir = inbox(&workspace)?;
        // A picture as the copy the model is shown, when asked and there
        // is one (issue #225): a HEIC photo a browser cannot show, a large
        // one quicker.
        if shown
            && dir.has(&read)?
            && let Ok(Some(copy)) = dir.read_bytes(
                &crate::llm_proxy::shown_copy(&read),
                crate::llm_proxy::MAX_IMAGE_BYTES,
            )
        {
            return Ok(Some(copy));
        }
        dir.read_bytes(&read, MAX_UPLOAD_BYTES as u64 + TEXT_BYTES)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(|e| ApiError::Internal(format!("reading the inbox: {e}")))?
    .ok_or(ApiError::NotFound)?;
    let image = crate::llm_proxy::image_type(&bytes);
    let response = file_response(&name, Vec::new(), image);
    Ok(streamed(response, bytes, reading))
}

/// `response` with `bytes` as its body, sent in pieces; the reading slot
/// goes with the last piece, so the files held in memory stay bounded
/// while they are sent.
fn streamed(
    mut response: axum::response::Response,
    bytes: Vec<u8>,
    reading: tokio::sync::OwnedSemaphorePermit,
) -> axum::response::Response {
    let bytes = axum::body::Bytes::from(bytes);
    let pieces: Vec<axum::body::Bytes> = (0..bytes.len())
        .step_by(64 * 1024)
        .map(|at| bytes.slice(at..(at + 64 * 1024).min(bytes.len())))
        .collect();
    let body = futures_util::stream::unfold(
        (pieces.into_iter(), reading),
        |(mut pieces, reading)| async move {
            pieces
                .next()
                .map(|piece| (Ok::<_, std::convert::Infallible>(piece), (pieces, reading)))
        },
    );
    response
        .headers_mut()
        .remove(axum::http::header::CONTENT_LENGTH);
    *response.body_mut() = axum::body::Body::from_stream(body);
    response
}

// ---- Browsing an agent's workspace (issue #177) ----

/// Entries one listing shows.
const MAX_LISTED: usize = 2000;
/// Largest workspace file sent.
const MAX_BROWSE_BYTES: u64 = 50 * 1024 * 1024;
/// Folders deep a path may go.
const MAX_BROWSE_DEPTH: usize = 64;
/// Characters of a link's target shown.
const MAX_LINK_TARGET: usize = 200;
/// The agent config key of its owner's switch.
const AGENT_SWITCH: &str = "workspace_browse";

/// Whether `name`, at the workspace's top level, is the runtime's own:
/// the agent's database and `.havn`, never shown or sent.
fn internal(name: &str) -> bool {
    name == ".havn" || name == "agent.db" || name.starts_with("agent.db-")
}

/// Refuse a request another site's page made.
fn same_site(headers: &HeaderMap) -> Result<(), ApiError> {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes() == b"cross-site")
    {
        return Err(ApiError::Forbidden("not from another site".into()));
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct AccessView {
    /// `[workspace] browse` in config.toml.
    pub gateway: bool,
    /// The owner's switch for this agent (on unless turned off).
    pub agent: bool,
    /// Both: the workspace page is there.
    pub enabled: bool,
}

fn access_of(state: &AppState, agent: &havn_db::repo::agents::Agent) -> AccessView {
    let gateway = state
        .workspace_browse
        .load(std::sync::atomic::Ordering::Relaxed);
    // On unless set; a value that is not a boolean counts as off.
    let own = agent
        .config
        .get(AGENT_SWITCH)
        .is_none_or(|v| v.as_bool() == Some(true));
    AccessView {
        gateway,
        agent: own,
        enabled: gateway && own,
    }
}

/// `GET /agents/{id}/workspace/access`.
pub async fn access_get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<AccessView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    Ok(Json(access_of(&state, &agent)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessPut {
    pub enabled: bool,
}

/// `PUT /agents/{id}/workspace/access`: the owner turns the agent's
/// workspace page on or off; on only while the gateway allows it.
pub async fn access_put(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<AccessPut>,
) -> Result<Json<AccessView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    if req.enabled && !access_of(&state, &agent).gateway {
        return Err(ApiError::Forbidden(
            "workspace browsing is off for this gateway ([workspace] browse in config.toml)".into(),
        ));
    }
    let updated = havn_db::repo::agents::update_config(&state.db, agent.id, None, |current| {
        let mut config = current.as_object().cloned().unwrap_or_default();
        config.insert(AGENT_SWITCH.into(), serde_json::Value::Bool(req.enabled));
        Ok::<_, std::convert::Infallible>(serde_json::Value::Object(config))
    })
    .await?;
    let Ok(updated) = updated;
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.workspace_browse.changed",
        serde_json::json!({ "enabled": req.enabled }),
    )
    .await;
    Ok(Json(access_of(&state, &updated)))
}

#[derive(Debug, serde::Serialize)]
pub struct BrowseView {
    pub browse: bool,
    /// The dashboard may change it: not with `trust_header`, where
    /// config.toml is the operator's.
    pub editable: bool,
}

/// `GET /settings/workspace`.
pub async fn browse_get(State(state): State<AppState>, _user: AuthedUser) -> Json<BrowseView> {
    Json(BrowseView {
        browse: state
            .workspace_browse
            .load(std::sync::atomic::Ordering::Relaxed),
        editable: state.trusted_proxies.is_none(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowsePut {
    pub browse: bool,
}

/// `PUT /settings/workspace`: turn workspace browsing on or off for the
/// gateway, written to config.toml.
pub async fn browse_put(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<BrowsePut>,
) -> Result<Json<BrowseView>, ApiError> {
    if state.trusted_proxies.is_some() {
        return Err(ApiError::Forbidden(
            "with trust_header enabled, workspace browsing is set in config.toml".into(),
        ));
    }
    {
        let _writing = state.models_write.lock().await;
        let path = state.config_path.clone();
        tokio::task::spawn_blocking(move || {
            crate::config::write_workspace_browse(&path, req.browse)
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
        state
            .workspace_browse
            .store(req.browse, std::sync::atomic::Ordering::Relaxed);
    }
    audit::record_user_action(
        &state.db,
        user.id,
        "workspace.browse.changed",
        serde_json::json!({ "browse": req.browse }),
    )
    .await;
    Ok(browse_get(State(state), user).await)
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceQuery {
    /// A folder or file, `/`-separated, relative to the workspace; the
    /// workspace itself when empty.
    #[serde(default)]
    pub path: String,
}

/// The names of `path`, each one checked.
fn path_names(path: &str) -> Result<Vec<String>, ApiError> {
    let names: Vec<String> = path
        .split('/')
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    if names.len() > MAX_BROWSE_DEPTH {
        return Err(ApiError::BadRequest("path too deep".into()));
    }
    for (i, name) in names.iter().enumerate() {
        if name == "." || name == ".." || name.contains('\0') {
            return Err(ApiError::BadRequest(format!("bad path component {name:?}")));
        }
        if i == 0 && internal(name) {
            return Err(ApiError::NotFound);
        }
    }
    Ok(names)
}

/// The agent's workspace, with browsing allowed, opened.
async fn browsable(
    state: &AppState,
    user: &AuthedUser,
    id: &str,
    headers: &HeaderMap,
) -> Result<std::path::PathBuf, ApiError> {
    same_site(headers)?;
    let agent = crate::api::agents::resolve_agent(id, user, state).await?;
    if !access_of(state, &agent).enabled {
        return Err(ApiError::Forbidden(
            "workspace browsing is off for this agent".into(),
        ));
    }
    Ok(crate::api::agents::workspace_for(state, agent.id))
}

/// The folder `names` lead to, opened one folder at a time without
/// following a link; `None` when one is missing or not a folder.
fn open_folder(
    workspace: &std::path::Path,
    names: &[String],
) -> std::io::Result<Option<WorkspaceDir>> {
    let mut dir = WorkspaceDir::open(workspace)?;
    for name in names {
        match dir.subdir(name) {
            Ok(Some(next)) => dir = next,
            Ok(None) => return Ok(None),
            // A link or a file where a folder was named.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(Some(dir))
}

#[derive(Debug, serde::Serialize)]
pub struct WorkspaceEntry {
    pub name: String,
    /// `dir`, `file`, `link` or `other`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    /// Where a link points, as written; never followed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct WorkspaceListing {
    pub path: String,
    pub entries: Vec<WorkspaceEntry>,
    /// More entries than were listed.
    pub truncated: bool,
}

/// `GET /agents/{id}/workspace?path=` — a folder of the agent's workspace:
/// folders first, then files, by name.
pub async fn workspace_list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<WorkspaceQuery>,
    headers: HeaderMap,
) -> Result<Json<WorkspaceListing>, ApiError> {
    use std::os::unix::fs::MetadataExt as _;
    let workspace = browsable(&state, &user, &id, &headers).await?;
    let names = path_names(&q.path)?;
    let at_top = names.is_empty();
    let path = names.join("/");
    let listed = tokio::task::spawn_blocking(move || {
        let Some(dir) = open_folder(&workspace, &names)? else {
            return Ok(None);
        };
        let (found, truncated) = dir.entries(MAX_LISTED)?;
        let mut entries: Vec<WorkspaceEntry> = found
            .into_iter()
            .filter(|(name, _)| !(at_top && internal(name)))
            .map(|(name, meta)| {
                let kind = if meta.file_type().is_symlink() {
                    "link"
                } else if meta.is_dir() {
                    "dir"
                } else if meta.is_file() {
                    "file"
                } else {
                    "other"
                };
                let target = (kind == "link")
                    .then(|| dir.link_target(&name).ok())
                    .flatten()
                    .map(|t| t.chars().take(MAX_LINK_TARGET).collect());
                WorkspaceEntry {
                    size: (kind == "file").then_some(meta.len()),
                    modified: Some(meta.mtime()),
                    name,
                    kind,
                    target,
                }
            })
            .collect();
        entries.sort_by(|a, b| (a.kind != "dir", &a.name).cmp(&(b.kind != "dir", &b.name)));
        Ok::<_, std::io::Error>(Some((entries, truncated)))
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(|e| ApiError::Internal(format!("reading the workspace: {e}")))?;
    let (entries, truncated) = listed.ok_or(ApiError::NotFound)?;
    Ok(Json(WorkspaceListing {
        path,
        entries,
        truncated,
    }))
}

/// `GET /agents/{id}/workspace/file?path=` — a file of the agent's
/// workspace: a PNG, JPEG, GIF or WebP image in place, anything else as a
/// download; read without following links.
pub async fn workspace_file(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<WorkspaceQuery>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let workspace = browsable(&state, &user, &id, &headers).await?;
    let mut names = path_names(&q.path)?;
    let Some(name) = names.pop() else {
        return Err(ApiError::BadRequest("path names no file".into()));
    };
    // Files held at once: each is read whole.
    static READING: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let reading = READING
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let read = name.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let Some(dir) = open_folder(&workspace, &names)? else {
            return Ok(None);
        };
        match dir.read_bytes(&read, MAX_BROWSE_BYTES) {
            // A link, a folder or another kind of entry is not sent.
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => Ok(None),
            other => other,
        }
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    // Not a regular file, or over the size sent.
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidData => ApiError::BadRequest(e.to_string()),
        _ => ApiError::Internal(format!("reading the workspace: {e}")),
    })?
    .ok_or(ApiError::NotFound)?;
    let image = crate::llm_proxy::image_type(&bytes);
    let mut response = file_response(&name, Vec::new(), image);
    // The agent may change it at any moment, and browsing may be turned off.
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(streamed(response, bytes, reading))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn a_workspace_is_browsed_only_as_far_as_both_switches_and_its_own_files_allow() {
        use std::sync::atomic::Ordering;
        let (mut state, user_id) = crate::auth::tests::state_with(false).await;
        let root = tempfile::tempdir().unwrap();
        state.workspace_root = root.path().to_path_buf();
        let agent = havn_db::repo::agents::create(
            &state.db,
            havn_db::repo::agents::NewAgent {
                owner_id: user_id,
                team_id: None,
                name: "browsed",
                config: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        let ws = crate::api::agents::workspace_for(&state, agent.id);
        std::fs::create_dir_all(ws.join("notes")).unwrap();
        std::fs::create_dir_all(ws.join(".havn/home")).unwrap();
        std::fs::write(ws.join("notes/a.md"), "hello").unwrap();
        std::fs::write(ws.join("agent.db"), "db").unwrap();
        std::fs::write(ws.join("agent.db-wal"), "db").unwrap();
        std::fs::write(ws.join(".havn/home/token"), "secret").unwrap();
        std::fs::write(ws.join("pic.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.md"), ws.join("notes/s.md"))
            .unwrap();

        let id = agent.id.to_string();
        let user = || AuthedUser {
            id: user_id,
            display_name: "owner".into(),
        };
        let list = |state: &AppState, path: &str| {
            workspace_list(
                State(state.clone()),
                user(),
                Path(id.clone()),
                Query(WorkspaceQuery { path: path.into() }),
                HeaderMap::new(),
            )
        };
        let file = |state: &AppState, path: &str| {
            workspace_file(
                State(state.clone()),
                user(),
                Path(id.clone()),
                Query(WorkspaceQuery { path: path.into() }),
                HeaderMap::new(),
            )
        };

        // Off for the gateway: nothing, and the owner cannot turn it on.
        assert!(matches!(
            list(&state, "").await,
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            file(&state, "notes/a.md").await,
            Err(ApiError::Forbidden(_))
        ));
        let refused = access_put(
            State(state.clone()),
            user(),
            Path(id.clone()),
            Json(AccessPut { enabled: true }),
        )
        .await;
        assert!(matches!(refused, Err(ApiError::Forbidden(_))));

        state.workspace_browse.store(true, Ordering::Relaxed);
        let top = list(&state, "").await.unwrap().0;
        let names: Vec<(&str, &str)> = top
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.kind))
            .collect();
        assert_eq!(
            names,
            [("notes", "dir"), ("out", "link"), ("pic.png", "file")]
        );
        assert_eq!(
            top.entries[1].target.as_deref(),
            Some(outside.path().to_string_lossy().as_ref())
        );
        let notes = list(&state, "notes/").await.unwrap().0;
        assert_eq!(notes.entries.len(), 2, "{:?}", notes.entries);
        assert_eq!(notes.entries[1].kind, "link");

        for hidden in [".havn", ".havn/home", "agent.db", "out"] {
            assert!(
                matches!(list(&state, hidden).await, Err(ApiError::NotFound)),
                "{hidden}"
            );
        }
        for bad in ["notes/..", "./notes", "a\0b"] {
            assert!(
                matches!(list(&state, bad).await, Err(ApiError::BadRequest(_))),
                "{bad}"
            );
        }
        for unseen in [
            "out/secret.md",
            "notes/s.md",
            "agent.db",
            "agent.db-wal",
            ".havn/home/token",
            "nope.md",
        ] {
            assert!(
                matches!(file(&state, unseen).await, Err(ApiError::NotFound)),
                "{unseen}"
            );
        }
        assert!(matches!(
            file(&state, "notes").await,
            Err(ApiError::BadRequest(_))
        ));

        let header = |r: &axum::response::Response, h: axum::http::HeaderName| {
            r.headers()
                .get(h)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let md = file(&state, "notes/a.md").await.unwrap();
        assert!(header(&md, axum::http::header::CONTENT_DISPOSITION).starts_with("attachment;"));
        assert_eq!(header(&md, axum::http::header::CACHE_CONTROL), "no-store");
        let body = axum::body::to_bytes(md.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"hello");
        std::fs::write(ws.join("back\\slash.md"), "b").unwrap();
        assert!(
            file(&state, "back\\slash.md").await.is_ok(),
            "a backslash is part of a name"
        );
        nix_fifo(&ws.join("pipe"));
        assert!(
            matches!(file(&state, "pipe").await, Err(ApiError::BadRequest(_))),
            "a FIFO is refused without blocking"
        );
        let png = file(&state, "pic.png").await.unwrap();
        assert!(header(&png, axum::http::header::CONTENT_DISPOSITION).starts_with("inline;"));
        assert_eq!(header(&png, axum::http::header::CONTENT_TYPE), "image/png");

        let mut cross = HeaderMap::new();
        cross.insert("sec-fetch-site", "cross-site".parse().unwrap());
        let other_site = workspace_list(
            State(state.clone()),
            user(),
            Path(id.clone()),
            Query(WorkspaceQuery {
                path: String::new(),
            }),
            cross,
        )
        .await;
        assert!(matches!(other_site, Err(ApiError::Forbidden(_))));

        // The owner turns it off for this agent.
        let view = access_put(
            State(state.clone()),
            user(),
            Path(id.clone()),
            Json(AccessPut { enabled: false }),
        )
        .await
        .unwrap()
        .0;
        assert!(view.gateway && !view.agent && !view.enabled);
        assert!(matches!(
            list(&state, "").await,
            Err(ApiError::Forbidden(_))
        ));
    }

    fn nix_fifo(path: &std::path::Path) {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[tokio::test]
    async fn only_the_dashboard_of_a_gateway_without_trust_header_flips_the_gateway_switch() {
        let (state, user_id) = crate::auth::tests::state_with(true).await;
        let user = AuthedUser {
            id: user_id,
            display_name: "operator".into(),
        };
        let refused =
            browse_put(State(state.clone()), user, Json(BrowsePut { browse: true })).await;
        assert!(matches!(refused, Err(ApiError::Forbidden(_))));
        assert!(
            !state
                .workspace_browse
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        let (state, user_id) = crate::auth::tests::state_with(false).await;
        let user = AuthedUser {
            id: user_id,
            display_name: "operator".into(),
        };
        let view = browse_put(State(state.clone()), user, Json(BrowsePut { browse: true }))
            .await
            .unwrap()
            .0;
        assert!(view.browse && view.editable);
        assert!(
            state
                .workspace_browse
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        let _ = std::fs::remove_file(&state.config_path);
    }

    #[test]
    fn a_deleted_chat_s_file_goes_with_its_text_and_copy_and_nothing_else() {
        let ws = tempfile::tempdir().unwrap();
        let inbox = ws.path().join(INBOX_DIR);
        std::fs::create_dir_all(&inbox).unwrap();
        let copy = crate::llm_proxy::shown_copy("1-a.png");
        let page = Attachment::page_image("1-a.png", 3);
        let other = Attachment::page_image("2-b.png", 1);
        for name in [
            "1-a.png",
            "1-a.png.txt",
            copy.as_str(),
            &page,
            &other,
            "2-b.png",
        ] {
            std::fs::write(inbox.join(name), b"x").unwrap();
        }
        std::fs::write(ws.path().join("secret"), b"x").unwrap();
        discard_path(ws.path(), "inbox/1-a.png", Some("inbox/1-a.png.txt"));
        discard_path(ws.path(), "inbox/../secret", None);
        discard_path(ws.path(), "secret", None);
        let left: std::collections::BTreeSet<String> = std::fs::read_dir(&inbox)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["2-b.png".to_string(), other].into());
        assert!(
            ws.path().join("secret").exists(),
            "nothing outside the inbox"
        );
    }

    #[test]
    fn a_switch_that_is_not_a_boolean_is_off() {
        let on = |config: serde_json::Value| {
            config
                .get(AGENT_SWITCH)
                .is_none_or(|v| v.as_bool() == Some(true))
        };
        assert!(on(serde_json::json!({})));
        assert!(on(serde_json::json!({AGENT_SWITCH: true})));
        assert!(!on(serde_json::json!({AGENT_SWITCH: false})));
        assert!(!on(serde_json::json!({AGENT_SWITCH: "off"})));
    }

    #[test]
    fn the_gateway_switch_is_written_to_config_toml_keeping_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\nlisten = \"127.0.0.1:9000\"\n").unwrap();
        crate::config::write_workspace_browse(&path, true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# mine\nlisten = \"127.0.0.1:9000\"\n"),
            "{text}"
        );
        let parsed: crate::config::GatewayConfig = toml::from_str(&text).unwrap();
        assert!(parsed.workspace.browse);
        crate::config::write_workspace_browse(&path, false).unwrap();
        let parsed: crate::config::GatewayConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!parsed.workspace.browse);
        for written in [
            "workspace = { browse = false }\n",
            "workspace.browse = false\n",
        ] {
            std::fs::write(&path, written).unwrap();
            crate::config::write_workspace_browse(&path, true).unwrap();
            let parsed: crate::config::GatewayConfig =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(parsed.workspace.browse, "{written}");
        }
    }

    #[test]
    fn an_inbox_image_shows_in_place_and_anything_else_downloads() {
        let header = |r: &axum::response::Response, h: axum::http::HeaderName| {
            r.headers()
                .get(h)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let png = file_response(
            "a.png",
            b"\x89PNG\r\n\x1a\nrest".to_vec(),
            Some("image/png"),
        );
        assert!(header(&png, axum::http::header::CONTENT_DISPOSITION).ends_with("''a.png"));
        // A name the agent chose that says another type is saved as the image.
        let disguised = file_response("x.html", b"\x89PNG\r\n\x1a\n".to_vec(), Some("image/png"));
        assert!(
            header(&disguised, axum::http::header::CONTENT_DISPOSITION).ends_with("''x.html.png")
        );
        let jpg = file_response("p.JPG", b"\xff\xd8\xff".to_vec(), Some("image/jpeg"));
        assert!(header(&jpg, axum::http::header::CONTENT_DISPOSITION).ends_with("''p.JPG"));
        assert_eq!(header(&png, axum::http::header::CONTENT_TYPE), "image/png");
        assert!(header(&png, axum::http::header::CONTENT_DISPOSITION).starts_with("inline;"));
        let page = file_response("x.png.html", b"<script>".to_vec(), None);
        assert_eq!(
            header(&page, axum::http::header::CONTENT_TYPE),
            "application/octet-stream"
        );
        assert!(header(&page, axum::http::header::CONTENT_DISPOSITION).starts_with("attachment;"));
        for r in [&png, &page] {
            assert_eq!(
                header(r, axum::http::header::X_CONTENT_TYPE_OPTIONS),
                "nosniff"
            );
            assert_eq!(
                header(r, axum::http::header::CONTENT_SECURITY_POLICY),
                "sandbox"
            );
        }
    }

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
    fn a_sent_file_is_downloaded_never_shown() {
        let sent = crate::outbox::Sent {
            id: "x".into(),
            name: "报告 <b>.html".into(),
            media_type: "text/html".into(),
            size: 3,
            channel_id: "c".into(),
            conversation: None,
            caption: None,
            created_at: chrono::Utc::now(),
        };
        let response = attachment_response(&sent, b"<b>".to_vec());
        let header = |name: axum::http::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(
            header(axum::http::header::CONTENT_TYPE),
            "application/octet-stream"
        );
        assert_eq!(
            header(axum::http::header::CONTENT_SECURITY_POLICY),
            "sandbox"
        );
        assert_eq!(
            header(axum::http::header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        let disposition = header(axum::http::header::CONTENT_DISPOSITION);
        assert!(disposition.starts_with("attachment;"), "{disposition}");
        assert!(
            disposition.contains("%E6%8A%A5%E5%91%8A%20%3Cb%3E.html"),
            "{disposition}"
        );
    }

    #[test]
    fn a_user_runs_a_few_uploads_at_once() {
        let user = havn_core::UserId::new();
        let first = UserUploads::take(Uploader::User(user)).expect("first");
        let _second = UserUploads::take(Uploader::User(user)).expect("second");
        assert!(UserUploads::take(Uploader::User(user)).is_none());
        assert!(
            UserUploads::take(Uploader::User(havn_core::UserId::new())).is_some(),
            "others are not held up"
        );
        drop(first);
        assert!(UserUploads::take(Uploader::User(user)).is_some());
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
