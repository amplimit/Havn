//! `POST /agents/:id/files` — a file the owner sends in the dashboard
//! chat (issue #106). It is saved in the agent's workspace under
//! `inbox/`, through [`crate::agent_files`] so that nothing the agent put
//! there is followed, and the text of a PDF, Word document or spreadsheet
//! is saved next to it. The answer is
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
    let reserve = if text_kind.is_some() { TEXT_BYTES } else { 0 };
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
    if let Some(kind) = text_kind {
        match crate::web_fetch::document_text(kind, &body).await {
            Ok((text, _)) if !text.trim().is_empty() => {
                let workspace = crate::api::agents::workspace_for(state, agent_id);
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
                        tracing::warn!(agent_id = %agent_id, error = %e, "could not save a document's text")
                    }
                    Err(e) => {
                        tracing::warn!(agent_id = %agent_id, error = %e, "could not save a document's text")
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::info!(agent_id = %agent_id, error = %e, "no text read from an uploaded document")
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
    Ok(attachment)
}

/// Remove a received file no message carried, and its text, from the
/// inbox of `workspace`.
pub fn discard(workspace: &std::path::Path, attachment: &Attachment) {
    let Ok(inbox) = inbox(workspace) else {
        return;
    };
    for path in std::iter::once(&attachment.path).chain(attachment.text_path.as_ref()) {
        if let Some(name) = path
            .strip_prefix(INBOX_DIR)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            let _ = inbox.remove(name);
        }
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

/// `GET /agents/:id/outbox` — the files the agent sent (issue #106),
/// oldest first, for the chat that shows them.
pub async fn sent(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<OutboxQuery>,
) -> Result<Json<Vec<crate::outbox::Sent>>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let outbox = crate::outbox::dir(&state.workspace_root, agent.id);
    let sent = tokio::task::spawn_blocking(move || {
        crate::outbox::list(&outbox, q.conversation.as_deref(), 200)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(sent))
}

/// `GET /agents/:id/outbox/:file` — a file the agent sent, as a download.
pub async fn download(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, file)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let outbox = crate::outbox::dir(&state.workspace_root, agent.id);
    let found = tokio::task::spawn_blocking(move || crate::outbox::read(&outbox, &file))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
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
}

/// `GET /agents/:id/files?path=inbox/<name>` — a file in the agent's inbox
/// (one sent in a chat, or its text), for the owner's chat to show: a
/// PNG, JPEG, GIF or WebP image in place, anything else as a download.
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
    let bytes = tokio::task::spawn_blocking(move || {
        inbox(&workspace)
            .and_then(|dir| dir.read_bytes(&read, MAX_UPLOAD_BYTES as u64 + TEXT_BYTES))
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(|e| ApiError::Internal(format!("reading the inbox: {e}")))?
    .ok_or(ApiError::NotFound)?;
    let image = crate::llm_proxy::image_type(&bytes);
    let bytes = axum::body::Bytes::from(bytes);
    let pieces: Vec<axum::body::Bytes> = (0..bytes.len())
        .step_by(64 * 1024)
        .map(|at| bytes.slice(at..(at + 64 * 1024).min(bytes.len())))
        .collect();
    let mut response = file_response(&name, Vec::new(), image);
    // The slot goes with the last piece.
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
    Ok(response)
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
    let own = agent
        .config
        .get(AGENT_SWITCH)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
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
        if name == "." || name == ".." || name.contains('\0') || name.contains('\\') {
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
    let _reading = READING
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
    Ok(file_response(&name, bytes, image))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

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
