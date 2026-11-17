//! `/api/v1/channel` — bidirectional WebSocket for external channel
//! adapter daemons (spec §3.1).
//!
//! One open WebSocket per (adapter daemon, channel account). Adapter
//! authenticates with `X-Adapter-Token`; gateway resolves
//! (channel, account) from query params, looks up the credential row
//! by handle, decrypts, and compares against the supplied token.
//!
//! Frames in both directions are typed JSON via [`havn_proto::channel::
//! ChannelFrame`]. Inbound messages are routed to the bound agent,
//! ping gets pong, and agent replies are pushed back as `Outbound`
//! frames through [`crate::channel_router`], whose replay buffer serves
//! `resume`.
//!
//! Security: webchat is **not** a channel adapter (spec §3.6); this
//! endpoint is for external daemons only. webchat continues to use
//! `/ws/chat/:agent_id`.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt as _};
use havn_core::{InboundMessage, MessageContent};
use havn_db::repo::agents as agent_repo;
use havn_db::repo::channel_access;
use havn_db::repo::credentials::{self as creds, CredentialScope};
use havn_proto::GatewayToAgent;
use havn_proto::channel::{ChannelFrame, ChannelInbound, ChannelMessageContent};
use serde::Deserialize;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::AppState;
use crate::agent_registry::SendError;
use crate::api::{ApiError, WS_SEND_TIMEOUT};
use crate::channel_id_codec;
use crate::channel_router::{self, AccountKey};
use crate::config::DmPolicy;
use crate::secret_ref;

/// Header carrying the operator-generated adapter authentication
/// token (spec §3.3). The matching credential row is keyed by
/// `(provider = "channel:<channel>", name = <account_id>)`; the gateway
/// finds it by handle and constant-time-compares the decrypted bytes
/// against this header.
const ADAPTER_TOKEN_HEADER: &str = "x-adapter-token";

/// Answers to one sender who is not paired, per account, within
/// [`DM_REPLY_WINDOW`]: every message is answered, but a stranger cannot
/// make the bot send more than this.
const DM_REPLY_LIMIT: u32 = 3;
const DM_REPLY_WINDOW: Duration = Duration::from_secs(60);
/// Senders the reply limiter tracks at once; past it, new senders get no
/// answer until older windows end.
const DM_REPLY_MAX_KEYS: usize = 10_000;

/// What an `allowlist` bot tells a sender who is not paired (issue #183),
/// unless `[channel_texts]` says otherwise.
const PAIR_WELCOME: &str = "Hello! This bot needs pairing before you can use it. Send the pairing \
                            code you were given (it looks like 7K3M-QX9P-2HWD-RT4B), or open the \
                            pairing link you got. No code yet? Ask whoever runs this bot.";
const PAIR_UNPAIRED: &str = "Not paired yet. Send your pairing code, or /help.";
const PAIR_INVALID_CODE: &str = "That pairing code is not valid or has expired.";
const PAIR_HELP: &str = "This bot answers only people who have paired with it. To pair, send the \
                         pairing code you were given (it looks like 7K3M-QX9P-2HWD-RT4B), or open \
                         the pairing link you got. Codes come from whoever runs this bot.";

/// Sent back when an inbound message could not be handed to the agent.
const AGENT_UNREACHABLE_MESSAGE: &str =
    "The agent could not receive your message right now. Please try again later.";

/// Sent back when the gateway is restarting and takes no new messages.
const GATEWAY_RESTARTING_MESSAGE: &str =
    "The service is restarting and did not take your message. Please send it again in a minute.";

const INVITE_WELCOME_MESSAGE: &str = "Paired. You can now message this bot.";

/// Invite-code attempts allowed per sender and account within
/// [`INVITE_ATTEMPT_WINDOW`]. Codes carry 80 bits, so this is not what
/// keeps them unguessable; it stops one sender from hammering the DB.
const INVITE_ATTEMPT_LIMIT: u32 = 5;
const INVITE_ATTEMPT_WINDOW: Duration = Duration::from_secs(900);
/// Senders tracked at once. Once full, the sender whose window started
/// first is forgotten to make room, so memory stays bounded and a flood
/// of fresh sender ids cannot lock everyone else out of redeeming. The
/// limiter only bounds DB load; the codes' 80 bits keep them secret.
const INVITE_LIMITER_MAX_KEYS: usize = 10_000;

/// Rate-limits the answers to senders who are not paired: at most
/// [`DM_REPLY_LIMIT`] per sender within [`DM_REPLY_WINDOW`]. Without it,
/// someone flooding a bot would get the bot to send as many answers
/// (reflector amplification).
///
/// Key: `"{channel}|{account_id}|{sender_id}"`.
#[derive(Debug, Clone, Default)]
pub struct DmRejectLimiter {
    windows: Arc<std::sync::Mutex<HashMap<String, (Instant, u32)>>>,
}

impl DmRejectLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether to answer this message from `key`'s sender.
    pub fn should_send(&self, key: &str) -> bool {
        self.should_send_at(key, Instant::now())
    }

    fn should_send_at(&self, key: &str, now: Instant) -> bool {
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= DM_REPLY_MAX_KEYS && !map.contains_key(key) {
            map.retain(|_, (start, _)| now.duration_since(*start) < DM_REPLY_WINDOW);
            if map.len() >= DM_REPLY_MAX_KEYS {
                return false;
            }
        }
        let (start, count) = map.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(*start) >= DM_REPLY_WINDOW {
            *start = now;
            *count = 0;
        }
        if *count >= DM_REPLY_LIMIT {
            return false;
        }
        *count += 1;
        true
    }
}

/// Outcome of [`InviteAttemptLimiter::check`]. `first` is true once per
/// window, so the caller logs a refusal once instead of per message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteAttempt {
    Allowed,
    /// This sender used up its attempts for the window.
    Limited {
        first: bool,
    },
}

/// Counts invite-code attempts from unauthorized senders in a fixed
/// window per key. Same key shape as [`DmRejectLimiter`]. Expired
/// windows are pruned once per [`INVITE_ATTEMPT_WINDOW`].
#[derive(Debug, Clone, Default)]
pub struct InviteAttemptLimiter {
    state: Arc<std::sync::Mutex<AttemptState>>,
}

#[derive(Debug, Default)]
struct AttemptState {
    windows: HashMap<String, AttemptWindow>,
    /// Keys in the order their windows started, oldest first. An entry
    /// goes stale when its key's window is evicted or restarts; stale
    /// entries are skipped on eviction and dropped on prune.
    order: VecDeque<(String, Instant)>,
    last_prune: Option<Instant>,
    evict_logged: bool,
}

impl AttemptState {
    fn is_live(&self, key: &str, start: Instant) -> bool {
        self.windows.get(key).is_some_and(|w| w.start == start)
    }

    fn evict_oldest(&mut self) {
        while let Some((key, start)) = self.order.pop_front() {
            if self.is_live(&key, start) {
                self.windows.remove(&key);
                return;
            }
        }
    }
}

#[derive(Debug)]
struct AttemptWindow {
    start: Instant,
    count: u32,
    logged: bool,
}

impl AttemptWindow {
    fn at(start: Instant) -> Self {
        Self {
            start,
            count: 0,
            logged: false,
        }
    }
}

impl InviteAttemptLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts the attempt and returns `Allowed` while the key is under
    /// [`INVITE_ATTEMPT_LIMIT`] for the current window.
    pub fn check(&self, key: &str) -> InviteAttempt {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &str, now: Instant) -> InviteAttempt {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        if state
            .last_prune
            .is_none_or(|t| now.duration_since(t) >= INVITE_ATTEMPT_WINDOW)
        {
            state
                .windows
                .retain(|_, w| now.duration_since(w.start) < INVITE_ATTEMPT_WINDOW);
            let windows = &state.windows;
            state
                .order
                .retain(|(k, start)| windows.get(k).is_some_and(|w| w.start == *start));
            state.last_prune = Some(now);
            state.evict_logged = false;
        }
        if !state.windows.contains_key(key) && state.windows.len() >= INVITE_LIMITER_MAX_KEYS {
            state.evict_oldest();
            if !std::mem::replace(&mut state.evict_logged, true) {
                warn!(
                    limit = INVITE_LIMITER_MAX_KEYS,
                    "invite: attempt limiter is full; forgetting the oldest senders"
                );
            }
        }
        let window = match state.windows.entry(key.to_string()) {
            Entry::Occupied(e) => {
                let window = e.into_mut();
                if now.duration_since(window.start) >= INVITE_ATTEMPT_WINDOW {
                    *window = AttemptWindow::at(now);
                    state.order.push_back((key.to_string(), now));
                }
                window
            }
            Entry::Vacant(e) => {
                state.order.push_back((key.to_string(), now));
                e.insert(AttemptWindow::at(now))
            }
        };
        if window.count >= INVITE_ATTEMPT_LIMIT {
            let first = !std::mem::replace(&mut window.logged, true);
            return InviteAttempt::Limited { first };
        }
        window.count += 1;
        InviteAttempt::Allowed
    }
}

#[derive(Debug, Deserialize)]
pub struct ChannelQuery {
    /// Channel id — matches the `<channel>` in the credential's
    /// `channel:<channel>` provider value (e.g. "telegram", "slack").
    pub channel: String,
    /// Operator-supplied account id within that channel; matches the
    /// credential row's `name` column and the `[[bindings]] account_id`
    /// references in gateway config (spec §3.4 / §3.5).
    pub account: String,
}

/// HTTP handler for the WebSocket upgrade. Authenticates the request
/// then hands the upgraded socket to [`handle_session`].
pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<ChannelQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    authenticate(&state, &q, &headers).await?;
    let account_id = q.account.clone();
    let channel_id = q.channel.clone();
    Ok(ws.on_upgrade(move |socket| handle_session(socket, state, channel_id, account_id)))
}

/// `GET /api/v1/channel/files/:agent/:file?channel=&account=` — a file an
/// agent sent to one of the account's senders (issue #106), for the
/// adapter to post the way its platform does. Authenticated like the
/// adapter's WebSocket; an adapter gets only the files sent through its
/// own account.
pub async fn file(
    State(state): State<AppState>,
    Query(q): Query<ChannelQuery>,
    axum::extract::Path((agent, file)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authenticate(&state, &q, &headers).await?;
    let agent = havn_core::AgentId::from_str(&agent).map_err(|_| ApiError::NotFound)?;
    let outbox = crate::outbox::dir(&state.workspace_root, agent);
    let (channel, account) = (q.channel.clone(), q.account.clone());
    // Its metadata first: an adapter gets only files sent through its
    // own account, and reads nothing of the others'.
    let found = tokio::task::spawn_blocking(move || {
        let sent = crate::outbox::meta(&outbox, &file)?;
        match channel_id_codec::decode(&sent.channel_id) {
            Some((c, a, _)) if c == channel && a == account => crate::outbox::read(&outbox, &file),
            _ => None,
        }
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .ok_or(ApiError::NotFound)?;
    Ok(crate::api::files::attachment_response(&found.0, found.1))
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub channel: String,
    pub account: String,
    /// The sender who sent the file, as its messages name them.
    pub sender: String,
    /// The file's name on the platform.
    pub name: String,
}

/// A file a channel's sender sent, waiting for the message that carries it.
struct Received {
    channel: String,
    account: String,
    sender: String,
    attachment: havn_core::Attachment,
    at: Instant,
}

/// How long a file sent waits for its message.
const RECEIVED_FOR: Duration = Duration::from_secs(3600);
/// Files waiting at once, across all accounts.
const MAX_RECEIVED: usize = 2000;
/// Files one message carries at most.
const MAX_MESSAGE_FILES: usize = 10;
/// Files one sender may send in [`RECEIVED_FOR`].
const MAX_SENDER_FILES: usize = 60;
/// Bytes of a sender id, at most.
const MAX_SENDER_BYTES: usize = 128;
const FILE_LOST_MESSAGE: &str =
    "The file did not reach the agent (it may have restarted meanwhile). Please send it again.";

/// The files channel senders sent, by agent and inbox path.
fn received() -> &'static std::sync::Mutex<HashMap<(havn_core::AgentId, String), Received>> {
    static RECEIVED: std::sync::OnceLock<
        std::sync::Mutex<HashMap<(havn_core::AgentId, String), Received>>,
    > = std::sync::OnceLock::new();
    RECEIVED.get_or_init(Default::default)
}

/// `POST /api/v1/channel/files?channel=&account=&sender=&name=` — a file
/// one of the account's senders sent (issue #106), saved in the bound
/// agent's inbox as the dashboard's uploads are, under the same limits,
/// with its text when it is a document. Authenticated like the adapter's
/// WebSocket, and only for a sender the account answers. The answer is
/// the attachment the sender's message then names; only that sender's
/// message, through this account, can.
pub async fn upload(
    State(state): State<AppState>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<axum::Json<havn_core::Attachment>, ApiError> {
    let account = ChannelQuery {
        channel: q.channel.clone(),
        account: q.account.clone(),
    };
    authenticate(&state, &account, &headers).await?;
    channel_id_codec::validate_channel_id_part("sender_id", &q.sender)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if q.sender.len() > MAX_SENDER_BYTES {
        return Err(ApiError::BadRequest("the sender id is too long".into()));
    }
    let agent_id = channel_router::agent_for_account(&state.bindings, &q.channel, &q.account)
        .and_then(|a| havn_core::AgentId::from_str(a).ok())
        .ok_or(ApiError::NotFound)?;
    // Before the body is read: only a sender the account answers may put
    // files in the agent's inbox.
    let probe = ChannelInbound {
        seq: 0,
        account_id: q.account.clone(),
        channel_message_id: String::new(),
        sender_id: q.sender.clone(),
        content: ChannelMessageContent::Text {
            text: String::new(),
        },
        timestamp: chrono::Utc::now(),
        raw: None,
        attachments: Vec::new(),
    };
    if sender_access(&state, &q.channel, agent_id, &probe).await != SenderAccess::Allowed {
        return Err(ApiError::Forbidden(
            "this sender may not send the agent files".into(),
        ));
    }
    let agent = agent_repo::find_by_id(&state.db, agent_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    if !sender_may_send(&q.channel, &q.account, &q.sender) {
        return Err(ApiError::Conflict(format!(
            "this sender sent {MAX_SENDER_FILES} files in the last hour; wait a while"
        )));
    }
    let uploader = crate::api::files::Uploader::Account {
        channel: q.channel.clone(),
        account: q.account.clone(),
    };
    let attachment =
        crate::api::files::receive(&state, agent_id, uploader, &q.name, &headers, body).await?;
    crate::audit::record_agent_action(
        &state.db,
        agent.owner_id,
        agent_id,
        "file.received",
        serde_json::json!({
            "channel": q.channel,
            "account": q.account,
            "sender": q.sender,
            "path": attachment.path,
            "size": attachment.size,
            "media_type": attachment.media_type,
        }),
    )
    .await;
    let forgotten = {
        let mut waiting = received()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut forgotten: Vec<(havn_core::AgentId, havn_core::Attachment)> = Vec::new();
        waiting.retain(|(agent, _), r| {
            let keep = r.at.elapsed() < RECEIVED_FOR;
            if !keep {
                forgotten.push((*agent, r.attachment.clone()));
            }
            keep
        });
        while waiting.len() >= MAX_RECEIVED {
            let Some(oldest) = waiting
                .iter()
                .min_by_key(|(_, r)| r.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(r) = waiting.remove(&oldest) {
                forgotten.push((oldest.0, r.attachment));
            }
        }
        waiting.insert(
            (agent_id, attachment.path.clone()),
            Received {
                channel: q.channel,
                account: q.account,
                sender: q.sender,
                attachment: attachment.clone(),
                at: Instant::now(),
            },
        );
        forgotten
    };
    // Files no message carried leave the inbox with their record.
    if !forgotten.is_empty() {
        let workspaces: Vec<(std::path::PathBuf, havn_core::Attachment)> = forgotten
            .into_iter()
            .map(|(agent, a)| (crate::api::agents::workspace_for(&state, agent), a))
            .collect();
        tokio::task::spawn_blocking(move || {
            for (workspace, a) in &workspaces {
                crate::api::files::discard(workspace, a);
            }
        });
    }
    Ok(axum::Json(attachment))
}

/// Whether `sender` may send another file now, counting it if so.
fn sender_may_send(channel: &str, account: &str, sender: &str) -> bool {
    static SENT: std::sync::OnceLock<
        std::sync::Mutex<HashMap<(String, String, String), VecDeque<Instant>>>,
    > = std::sync::OnceLock::new();
    let mut sent = SENT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sent.retain(|_, times| {
        while times.front().is_some_and(|t| t.elapsed() >= RECEIVED_FOR) {
            times.pop_front();
        }
        !times.is_empty()
    });
    let times = sent
        .entry((channel.to_string(), account.to_string(), sender.to_string()))
        .or_default();
    if times.len() >= MAX_SENDER_FILES {
        return false;
    }
    times.push_back(Instant::now());
    true
}

/// Put back attachments [`take_received`] took for a message that then
/// did not reach the agent, so the adapter can send it again.
fn restore_received(
    agent_id: havn_core::AgentId,
    channel: &str,
    account: &str,
    sender: &str,
    attachments: Vec<havn_core::Attachment>,
) {
    let mut waiting = received()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for attachment in attachments {
        waiting.insert(
            (agent_id, attachment.path.clone()),
            Received {
                channel: channel.to_string(),
                account: account.to_string(),
                sender: sender.to_string(),
                attachment,
                at: Instant::now(),
            },
        );
    }
}

/// The attachments of a message from `sender`: of the files it names,
/// those this sender sent through this account, each for one message. A
/// file the agent's owner sent, or another sender, cannot be named.
fn take_received(
    agent_id: havn_core::AgentId,
    channel: &str,
    account: &str,
    sender: &str,
    named: &[havn_core::Attachment],
) -> Vec<havn_core::Attachment> {
    let mut waiting = received()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if named.len() > MAX_MESSAGE_FILES {
        warn!(
            channel,
            account,
            sender,
            files = named.len(),
            "a message names more files than it may carry; keeping the first"
        );
    }
    named
        .iter()
        .take(MAX_MESSAGE_FILES)
        .filter_map(|a| {
            let key = (agent_id, a.path.clone());
            let theirs = waiting.get(&key).is_some_and(|r| {
                r.channel == channel
                    && r.account == account
                    && r.sender == sender
                    && r.at.elapsed() < RECEIVED_FOR
            });
            if !theirs {
                warn!(channel, account, sender, path = ?a.path, "dropping an attachment the sender did not send");
                return None;
            }
            waiting.remove(&key).map(|r| r.attachment)
        })
        .collect()
}

/// Check that the request carries the adapter token of the configured
/// `(channel, account)` it names.
async fn authenticate(
    state: &AppState,
    q: &ChannelQuery,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let presented = headers
        .get(ADAPTER_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            warn!(
                channel = %q.channel,
                account = %q.account,
                "rejecting channel WS: missing X-Adapter-Token header"
            );
            ApiError::BadRequest("missing X-Adapter-Token header".into())
        })?
        .to_string();

    // Resolve the configured channel account. The query params name a
    // (channel, account_id) pair; that pair MUST be declared in
    // `[[channels.<channel>.accounts]]` (spec §3.4) — operators
    // explicitly enrol each bot identity. Connections for unknown
    // accounts get 401 even if the right token would match a stray
    // credential row (defence against an operator who manually
    // inserted a `channel:*` row without binding it to a config-
    // declared account).
    let account = state
        .channels
        .get(&q.channel)
        .and_then(|entry| entry.accounts.iter().find(|a| a.id == q.account))
        .ok_or_else(|| {
            warn!(
                channel = %q.channel,
                account = %q.account,
                "channel WS: no [[channels.<channel>.accounts]] entry matches"
            );
            ApiError::Unauthorized
        })?;

    // Parse the operator-supplied `secret:<provider>:<name>` reference
    // per spec §7.3. If the operator wrote `adapter_token_ref =
    // "secret:channel:telegram:alice-tg-bot"` we look up the
    // credentials row by exactly (provider="channel:telegram",
    // name="alice-tg-bot"). The reference parser is strict; malformed
    // refs at upgrade time are 401 (not 500) — operator config error
    // shouldn't expose internals, just refuse the connection.
    let parsed_ref = secret_ref::parse(&account.adapter_token_ref).map_err(|e| {
        warn!(
            channel = %q.channel,
            account = %q.account,
            error = %e,
            "channel WS: malformed adapter_token_ref"
        );
        ApiError::Unauthorized
    })?;

    // v0.2 single-user channel adapters live under the bootstrap
    // user's scope. Team-scope lookup is intentionally deferred to
    // v0.3 — single-host single-tenant is the only configuration
    // havn-channel-telegram targets in v0.2.
    let row = creds::find_by_handle(
        &state.db,
        CredentialScope::User,
        &state.bootstrap_user_id.to_string(),
        &parsed_ref.provider,
        &parsed_ref.name,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("credential lookup: {e}")))?
    .ok_or_else(|| {
        warn!(
            channel = %q.channel,
            account = %q.account,
            ref_provider = %parsed_ref.provider,
            ref_name = %parsed_ref.name,
            "channel WS: adapter_token_ref points at a credential row that doesn't exist (run `havn credential add`)"
        );
        ApiError::Unauthorized
    })?;

    if !row.enabled {
        warn!(
            channel = %q.channel,
            account = %q.account,
            "channel WS: credential row exists but is disabled"
        );
        return Err(ApiError::Unauthorized);
    }

    let ring = state.keyring.as_ref().ok_or_else(|| {
        warn!("channel WS: no keyring configured (HAVN_AGE_KEY) — cannot verify adapter token");
        ApiError::Internal("credential decryption unavailable".into())
    })?;

    let plaintext = ring.decrypt(&row.api_key_ciphertext).map_err(|e| {
        warn!(error = %e, "channel WS: failed to decrypt adapter token credential");
        ApiError::Unauthorized
    })?;

    if !tokens_match(&plaintext, presented.as_bytes()) {
        warn!(
            channel = %q.channel,
            account = %q.account,
            "channel WS: token mismatch"
        );
        return Err(ApiError::Unauthorized);
    }

    info!(
        channel = %q.channel,
        account = %q.account,
        "channel adapter authenticated"
    );
    Ok(())
}

/// Best-effort constant-time byte comparison for adapter tokens.
/// Returns false on length mismatch, then xor-folds the matching
/// portion. For v0.2 this is sufficient against remote adversaries
/// (network jitter dominates per-byte timing). NOT a robust CT
/// primitive against in-host side channels — the optimizer is
/// permitted under aggressive LTO to vectorize this into early-exit
/// code. If the threat model later includes co-located adversaries,
/// swap for `subtle::ConstantTimeEq` (already in `Cargo.lock`
/// transitively).
fn tokens_match(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Cap on consecutive `account_id`-mismatched inbound frames before
/// the gateway drops the WS. A misbehaving (or malicious) adapter
/// shouldn't be allowed to spam the gateway forever. Spec §3.1 says
/// the gateway "rejects frames whose account_id doesn't match" but
/// doesn't bound retries; this is the bound.
const MAX_ACCOUNT_MISMATCHES: u32 = 10;

/// mpsc capacity for outbound frames queued toward this WS. 64 is
/// generous; once it is full `channel_router::deliver` refuses further
/// frames (they stay in the replay buffer) instead of waiting.
const ADAPTER_OUTBOUND_CAPACITY: usize = 64;

async fn handle_session(
    socket: WebSocket,
    state: AppState,
    channel_id: String,
    account_id: String,
) {
    info!(channel = %channel_id, account = %account_id, "channel WS opened");

    let key = AccountKey::new(&channel_id, &account_id);
    let (frame_tx, mut frame_rx) = mpsc::channel::<ChannelFrame>(ADAPTER_OUTBOUND_CAPACITY);
    // The router holds the only lasting sender: when it drops a stalled
    // adapter, the outbound task below ends and so does this session.
    let registered = frame_tx.downgrade();
    // The commands, for the platform's command menu.
    let commands = ChannelFrame::Commands(havn_proto::channel::ChannelCommands {
        account_id: account_id.clone(),
        commands: crate::commands::menu(),
    });
    if let Err(e) = frame_tx.try_send(commands) {
        warn!(channel = %channel_id, account = %account_id, error = %e, "could not send the command menu");
    }
    state.channel_router.register(key.clone(), frame_tx).await;

    // Separate mpsc for raw WS control frames (Pong replies to
    // protocol-level Pings). Keeps `ChannelRouter` typed on
    // `ChannelFrame` (clean abstraction) while still letting the
    // gateway honour RFC 6455 control-frame liveness via the same
    // single ws_sink owner.
    let (raw_tx, mut raw_rx) = mpsc::channel::<Message>(8);

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Outbound task: drain both mpscs, write to WS. tokio::select!
    // gives equal priority. Closes WS when either mpsc is dropped or
    // any write fails or stalls.
    let mut outbound = tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                maybe_frame = frame_rx.recv() => {
                    let Some(frame) = maybe_frame else { break };
                    match serde_json::to_string(&frame) {
                        Ok(s) => Message::Text(s.into()),
                        Err(e) => {
                            warn!(error = %e, "channel WS: failed to serialize outbound frame");
                            break;
                        }
                    }
                }
                maybe_raw = raw_rx.recv() => {
                    let Some(raw) = maybe_raw else { break };
                    raw
                }
            };
            match tokio::time::timeout(WS_SEND_TIMEOUT, ws_sink.send(message)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    warn!("channel adapter is not reading its WS; closing it");
                    break;
                }
            }
        }
        let _ = tokio::time::timeout(WS_SEND_TIMEOUT, ws_sink.send(Message::Close(None))).await;
    });

    let mut mismatch_count: u32 = 0;
    let mut outbound_done = false;
    loop {
        let frame = tokio::select! {
            frame = ws_stream.next() => frame,
            _ = &mut outbound => {
                outbound_done = true;
                None
            }
        };
        let Some(frame) = frame else { break };
        let msg = match frame {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "channel WS read error");
                break;
            }
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            Message::Ping(p) => {
                // Auto-respond to control-frame pings via the raw_tx
                // sink-control mpsc; channel adapters can also use the
                // typed `ChannelFrame::Ping` for application-level
                // liveness with seq tracking. Note ws_sink is owned by
                // the outbound task — direct sends from here would
                // race; the mpsc serialises.
                if raw_tx.send(Message::Pong(p)).await.is_err() {
                    break;
                }
                continue;
            }
            Message::Pong(_) | Message::Binary(_) => continue,
        };

        let parsed: ChannelFrame = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, channel = %channel_id, account = %account_id, "discarding malformed channel frame");
                continue;
            }
        };

        match parsed {
            ChannelFrame::Inbound(inb) => {
                if inb.account_id != account_id {
                    // Per spec §3.1: gateway rejects frames whose
                    // account_id doesn't match the WS upgrade's account.
                    mismatch_count = mismatch_count.saturating_add(1);
                    warn!(
                        ws_account = %account_id,
                        frame_account = %inb.account_id,
                        mismatches = mismatch_count,
                        "rejecting Inbound: account_id mismatch"
                    );
                    if mismatch_count >= MAX_ACCOUNT_MISMATCHES {
                        warn!(
                            channel = %channel_id,
                            account = %account_id,
                            limit = MAX_ACCOUNT_MISMATCHES,
                            "closing channel WS: too many account_id mismatches"
                        );
                        break;
                    }
                    continue;
                }
                handle_inbound(&state, &channel_id, inb).await;
            }
            ChannelFrame::Outbound(_) => {
                // Outbound flows gateway → adapter only; an adapter
                // sending Outbound is a protocol violation. Log + drop.
                warn!(channel = %channel_id, account = %account_id, "dropping unexpected Outbound frame from adapter");
            }
            ChannelFrame::Resume(r) => {
                // Spec §3.2: replay buffered outbound frames with
                // seq > since_seq through the adapter's mpsc. Empty
                // result is the cold-start case (no state, e.g.
                // gateway restarted between disconnect and reconnect).
                let Some(frame_tx) = registered.upgrade() else {
                    break;
                };
                // Deliveries arriving meanwhile are only buffered; keep
                // replaying until the buffer has nothing newer.
                let mut frames = state
                    .channel_router
                    .replay_since(&key, r.since_seq, &frame_tx)
                    .await;
                let mut replayed = 0usize;
                let mut cursor = r.since_seq;
                'replay: loop {
                    for frame in frames {
                        if let ChannelFrame::Outbound(o) = &frame {
                            cursor = cursor.max(o.seq);
                        }
                        if frame_tx.send(frame).await.is_err() {
                            warn!(
                                channel = %channel_id,
                                account = %account_id,
                                since_seq = r.since_seq,
                                "channel WS: outbound mpsc closed mid-replay"
                            );
                            break 'replay;
                        }
                        replayed += 1;
                    }
                    frames = state
                        .channel_router
                        .replay_more(&key, cursor, &frame_tx)
                        .await;
                    if frames.is_empty() {
                        break;
                    }
                }
                info!(
                    channel = %channel_id,
                    account = %account_id,
                    since_seq = r.since_seq,
                    replayed,
                    "channel WS: resume processed"
                );
            }
            ChannelFrame::Ping => {
                // Application-level Pong via the raw mpsc — bypasses
                // the channel_router's seq numbering since Pong isn't
                // a numbered outbound frame.
                // Serialising a trivial unit-variant enum is infallible;
                // serde_json only errors on map-key or io issues.
                #[allow(clippy::expect_used)]
                let pong_json =
                    serde_json::to_string(&ChannelFrame::Pong).expect("Pong serializes");
                if raw_tx.send(Message::Text(pong_json.into())).await.is_err() {
                    warn!(channel = %channel_id, account = %account_id, "channel WS: outbound mpsc closed during Ping");
                    break;
                }
            }
            ChannelFrame::Pong => {
                // Server-initiated keepalive lands later. Silently accept.
            }
            ChannelFrame::Commands(_) => {
                warn!(channel = %channel_id, account = %account_id, "dropping Commands frame from adapter (wrong direction)");
            }
            // ChannelFrame is non_exhaustive; what an adapter sends is
            // never a reason to stop the gateway.
            #[allow(
                unreachable_patterns,
                reason = "compiler requires arm for non_exhaustive enum"
            )]
            _ => {
                warn!(channel = %channel_id, account = %account_id, "dropping a channel frame of a kind the gateway does not take");
            }
        }
    }

    if let Some(frame_tx) = registered.upgrade() {
        state.channel_router.unregister(&key, &frame_tx).await;
    }
    if !outbound_done {
        outbound.abort();
        let _ = outbound.await;
    }
    info!(channel = %channel_id, account = %account_id, "channel WS closed");
}

/// Route an inbound frame to the agent bound to `(channel,
/// account_id)` via gateway config `[[bindings]]` (spec §3.5). When
/// no binding exists, log + drop — the user gets no reply, which is
/// the operator-visible signal that they need to add a binding (the
/// startup `dangling_bindings` warning catches the inverse case).
async fn handle_inbound(state: &AppState, channel: &str, mut inb: ChannelInbound) {
    let agent_id_str =
        match channel_router::agent_for_account(&state.bindings, channel, &inb.account_id) {
            Some(s) => s,
            None => {
                warn!(
                    channel,
                    account = %inb.account_id,
                    seq = inb.seq,
                    sender = %inb.sender_id,
                    "dropping inbound: no [[bindings]] entry for this (channel, account)"
                );
                return;
            }
        };
    let agent_id = match havn_core::AgentId::from_str(agent_id_str) {
        Ok(a) => a,
        Err(_) => {
            warn!(
                channel,
                account = %inb.account_id,
                bound_to = agent_id_str,
                "dropping inbound: binding's agent_id is not a valid AgentId"
            );
            return;
        }
    };

    // Defence-in-depth: spec §3 requires sender_id to use a
    // platform-prefixed format that doesn't include the channel_id
    // codec separator. Reject anything containing `|`.
    if let Err(e) = channel_id_codec::validate_channel_id_part("sender_id", &inb.sender_id) {
        warn!(
            channel,
            account = %inb.account_id,
            error = %e,
            "dropping inbound: sender_id violates channel_id codec invariant"
        );
        return;
    }

    // --- dm_policy enforcement (spec §3.4, issues #23 and #25) ---
    //
    // The adapter-daemon WS connection is already authenticated (token
    // match above), but the *individual sender* must also be authorized
    // by the account's dm_policy, allow_from and invite allowlist. This
    // is the gate the audit found missing: without it, any platform user
    // who can reach the bot gets the full agent toolset.
    let access = sender_access(state, channel, agent_id, &inb).await;
    if access == SenderAccess::Admitted {
        let welcome = ChannelMessageContent::Text {
            text: pairing_text(state, channel, &inb.account_id, Refusal::Paired),
        };
        if let Err(e) = state
            .channel_router
            .deliver(
                channel,
                &inb.account_id,
                &inb.sender_id,
                Some(inb.channel_message_id.clone()),
                welcome,
            )
            .await
        {
            warn!(error = %e, "invite: failed to deliver welcome notice");
        }
        // The code itself is not a message for the agent.
        return;
    }

    if let SenderAccess::Denied(why) = access {
        warn!(
            channel,
            account = %inb.account_id,
            sender = %inb.sender_id,
            ?why,
            "dm_policy: not forwarding a message from a sender who is not paired"
        );
        let limiter_key = format!("{channel}|{}|{}", inb.account_id, inb.sender_id);
        if state.dm_reject_limiter.should_send(&limiter_key) {
            let rejection = ChannelMessageContent::Text {
                text: pairing_text(state, channel, &inb.account_id, why),
            };
            if let Err(e) = state
                .channel_router
                .deliver(
                    channel,
                    &inb.account_id,
                    &inb.sender_id,
                    Some(inb.channel_message_id.clone()),
                    rejection,
                )
                .await
            {
                warn!(error = %e, "dm_policy: failed to deliver rejection notice");
            }
        }
        return;
    }

    let encoded_channel_id = channel_id_codec::encode(channel, &inb.account_id, &inb.sender_id);
    let named = std::mem::take(&mut inb.attachments);
    // A message with files goes as text: what an adapter sent as a file
    // or image with them keeps its caption.
    if !named.is_empty() {
        match &inb.content {
            ChannelMessageContent::Image { caption, .. }
            | ChannelMessageContent::File { caption, .. } => {
                inb.content = ChannelMessageContent::Text {
                    text: caption.clone().unwrap_or_default(),
                };
            }
            _ => {}
        }
    }
    let content = match inb.content {
        ChannelMessageContent::Text { text } => MessageContent::Text { text },
        ChannelMessageContent::Image { url, caption } => MessageContent::Image { url, caption },
        ChannelMessageContent::File {
            url,
            filename,
            caption,
        } => MessageContent::File {
            url,
            filename,
            caption,
        },
        ChannelMessageContent::Audio {
            transcript,
            duration_seconds,
            url,
        } => MessageContent::Audio {
            transcript,
            duration_seconds,
            url,
        },
        ChannelMessageContent::Location {
            latitude,
            longitude,
            accuracy_meters,
            name,
            address,
        } => MessageContent::Location {
            latitude,
            longitude,
            accuracy_meters,
            name,
            address,
        },
        ChannelMessageContent::Contact {
            display_name,
            phone_number,
            vcard,
        } => MessageContent::Contact {
            display_name,
            phone_number,
            vcard,
        },
        // havn-proto's ChannelMessageContent is non_exhaustive; future
        // variants land here. Drop with a warn rather than refusing
        // the message — partial delivery is better than zero delivery
        // when the new variant doesn't have a havn-core counterpart.
        _ => {
            warn!(
                channel,
                account = %inb.account_id,
                "dropping inbound: unsupported ChannelMessageContent variant"
            );
            return;
        }
    };
    // A chat command is answered by the gateway or the agent, never by
    // the model (issues #173, #174). A message with files is not one.
    let parsed = match &content {
        MessageContent::Text { text } if named.is_empty() => crate::commands::parse(text),
        _ => None,
    };
    let mut new_chat = false;
    let mut branch = None;
    let mut command_frame = None;
    match parsed {
        None => {}
        Some(
            refused @ (crate::commands::Parsed::Unknown(_) | crate::commands::Parsed::Misused(_)),
        ) => {
            let text = refused.refusal().unwrap_or_default();
            tell_sender(
                state,
                channel,
                &inb.account_id,
                &inb.sender_id,
                &inb.channel_message_id,
                &text,
            )
            .await;
            return;
        }
        Some(crate::commands::Parsed::Known(command, args)) => match command.name {
            // Stops the reply running in this conversation.
            "stop" => {
                let stop = GatewayToAgent::StopTurn(havn_proto::StopTurn {
                    conversation: None,
                    channel_id: encoded_channel_id,
                });
                if let Err(e) = state.registry.send(agent_id, stop).await {
                    warn!(channel, %agent_id, error = ?e, "failed to forward StopTurn");
                }
                return;
            }
            // A new chat; the runtime confirms. It goes the way of a
            // message, in order with the ones after it.
            "new" => new_chat = true,
            // The last message answered again, as a new version of the
            // reply.
            "retry" => branch = Some(havn_core::Branch::Regenerate { turn: None }),
            _ => {
                let asker = crate::commands::Asker {
                    agent_id,
                    scope: encoded_channel_id.clone(),
                    conversation: None,
                    who: format!("{} on {channel} account {}", inb.sender_id, inb.account_id),
                    admin: is_admin(state, channel, &inb.account_id, &inb.sender_id),
                    admin_hint: "this bot's admins (listed under the account's `admins` in the \
                                 gateway's config.toml)",
                };
                match crate::commands::run(state, &asker, command, args).await {
                    crate::commands::Action::Reply(text) => {
                        tell_sender(
                            state,
                            channel,
                            &inb.account_id,
                            &inb.sender_id,
                            &inb.channel_message_id,
                            &text,
                        )
                        .await;
                        return;
                    }
                    crate::commands::Action::Agent(command) => {
                        command_frame = Some(GatewayToAgent::ChatCommand(command));
                    }
                }
            }
        },
    }
    let msg = InboundMessage {
        conversation: None,
        branch,
        channel_id: encoded_channel_id,
        sender_id: inb.sender_id.clone(),
        channel: channel.to_string(),
        content,
        timestamp: inb.timestamp,
        attachments: Vec::new(),
        raw: inb.raw.unwrap_or(serde_json::Value::Null),
    };

    // Parity with WebChat: a message to a stopped agent should wake it, not
    // vanish. WebChat lazy-spawns on its WS upgrade (api::webchat_ws); channel
    // inbound had no equivalent, so a message to an offline bound agent used to
    // be dropped with only a warn. The binding is the routing decision; per-sender
    // authorization is handled by dm_policy above. Attribute the lazy start to
    // the agent's owner for the audit log.
    //
    // This `ensure_running` is awaited inline in the per-account WS read loop,
    // so a cold spawn blocks this one connection (including its ping/pong) for
    // up to ensure_running's handshake timeout. That head-of-line blocking is
    // intentional: the stall is bounded to that timeout and confined to
    // accounts bound to this same agent (a concurrent inbound for the same
    // agent waits on ensure_running's per-agent start lock) — unrelated agents
    // and their accounts are unaffected. The inline await also preserves
    // inbound ordering across a cold-start burst (message N+1 isn't read until
    // N is forwarded). Do NOT move this to a detached task without restoring an
    // ordering guarantee. The hot path (already connected) skips it via
    // is_connected.
    if state.supervisor.shutting_down() {
        // An agent that has already drained is no longer connected and
        // cannot be started now; tell the sender instead of dropping it.
        tell_sender(
            state,
            channel,
            &inb.account_id,
            &inb.sender_id,
            &inb.channel_message_id,
            GATEWAY_RESTARTING_MESSAGE,
        )
        .await;
        return;
    }
    if !state.registry.is_connected(agent_id).await {
        match agent_repo::find_by_id(&state.db, agent_id).await {
            Ok(Some(agent_row)) => {
                if let Err(e) =
                    crate::api::agents::ensure_running(state, agent_row.owner_id, agent_id).await
                {
                    warn!(
                        channel,
                        account = %inb.account_id,
                        %agent_id,
                        error = ?e,
                        "channel inbound: failed to lazy-spawn bound agent; dropping"
                    );
                    return;
                }
            }
            Ok(None) => {
                warn!(
                    channel,
                    account = %inb.account_id,
                    %agent_id,
                    "channel inbound: binding points to a non-existent agent; dropping"
                );
                return;
            }
            Err(e) => {
                warn!(
                    channel,
                    account = %inb.account_id,
                    %agent_id,
                    error = %e,
                    "channel inbound: db error resolving bound agent; dropping"
                );
                return;
            }
        }
    }

    // A command for the agent goes now that it runs.
    if let Some(frame) = command_frame {
        let saved = crate::commands::saved_already(&frame);
        if let Err(e) = state.registry.send(agent_id, frame).await {
            warn!(channel, %agent_id, error = ?e, "failed to forward a chat command");
            let text = match (saved, e) {
                (Some(saved), _) => saved,
                (None, SendError::Draining) => GATEWAY_RESTARTING_MESSAGE,
                (None, _) => AGENT_UNREACHABLE_MESSAGE,
            };
            tell_sender(
                state,
                channel,
                &inb.account_id,
                &inb.sender_id,
                &inb.channel_message_id,
                text,
            )
            .await;
        }
        return;
    }

    // The files go with the message once it can reach the agent; a
    // command does not carry them.
    let mut msg = msg;
    if !new_chat && msg.branch.is_none() && !named.is_empty() {
        msg.attachments = take_received(agent_id, channel, &inb.account_id, &inb.sender_id, &named);
        let empty = matches!(&msg.content, MessageContent::Text { text } if text.trim().is_empty());
        if msg.attachments.is_empty() && empty {
            tell_sender(
                state,
                channel,
                &inb.account_id,
                &inb.sender_id,
                &inb.channel_message_id,
                FILE_LOST_MESSAGE,
            )
            .await;
            return;
        }
    }
    let carried = msg.attachments.clone();
    let frame = if new_chat {
        GatewayToAgent::NewChat(havn_proto::NewChat {
            channel_id: msg.channel_id,
        })
    } else {
        GatewayToAgent::InboundMessage(msg)
    };
    if let Err(e) = state.registry.send(agent_id, frame).await {
        restore_received(agent_id, channel, &inb.account_id, &inb.sender_id, carried);
        warn!(
            channel,
            account = %inb.account_id,
            %agent_id,
            error = ?e,
            "channel inbound: failed to forward to agent (offline?)"
        );
        let text = match e {
            SendError::Draining => GATEWAY_RESTARTING_MESSAGE,
            _ => AGENT_UNREACHABLE_MESSAGE,
        };
        tell_sender(
            state,
            channel,
            &inb.account_id,
            &inb.sender_id,
            &inb.channel_message_id,
            text,
        )
        .await;
    }
}

/// Whether `sender_id` is listed among the admins of `(channel,
/// account_id)` in the gateway's config.
fn is_admin(state: &AppState, channel: &str, account_id: &str, sender_id: &str) -> bool {
    state
        .channels
        .get(channel)
        .and_then(|entry| entry.accounts.iter().find(|a| a.id == account_id))
        .is_some_and(|acct| acct.admins.iter().any(|id| id == sender_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SenderAccess {
    /// Forward the message to the agent.
    Allowed,
    /// The message redeemed an invite: welcome the sender, do not forward.
    Admitted,
    /// Not forwarded: the sender is not paired; how it is answered.
    Denied(Refusal),
}

/// What a sender who is not paired is told (issue #183).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// `/start` without a code.
    Welcome,
    /// `/help`.
    Help,
    /// A code that is wrong, used, expired or for another bot, or one
    /// past the attempts allowed.
    InvalidCode,
    /// Anything else.
    Unpaired,
    /// Not a refusal: the code was accepted.
    Paired,
}

/// The text `why` gets on account `account_id` of `channel`: the one in
/// `[channel_texts]`, or the default.
fn pairing_text(state: &AppState, channel: &str, account_id: &str, why: Refusal) -> String {
    let texts = state.channel_texts.load();
    let own = texts
        .get(channel)
        .and_then(|accounts| accounts.get(account_id));
    let (set, default) = match why {
        Refusal::Welcome => (own.and_then(|t| t.welcome.clone()), PAIR_WELCOME),
        Refusal::Help => (own.and_then(|t| t.help.clone()), PAIR_HELP),
        Refusal::InvalidCode => (own.and_then(|t| t.invalid_code.clone()), PAIR_INVALID_CODE),
        Refusal::Unpaired => (own.and_then(|t| t.unpaired.clone()), PAIR_UNPAIRED),
        Refusal::Paired => (own.and_then(|t| t.paired.clone()), INVITE_WELCOME_MESSAGE),
    };
    set.filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// `text` as a command to a bot: `/name` or `/name@bot`, then the rest.
fn bot_command(text: &str) -> Option<(&str, &str)> {
    let text = text.trim();
    let word = text.split_whitespace().next()?;
    let name = word.strip_prefix('/')?;
    let name = name.split_once('@').map_or(name, |(n, _)| n);
    Some((name, text[word.len()..].trim()))
}

/// Reply to the sender's message `message_id` with `text`.
async fn tell_sender(
    state: &AppState,
    channel: &str,
    account_id: &str,
    sender_id: &str,
    message_id: &str,
    text: &str,
) {
    let notice = ChannelMessageContent::Text { text: text.into() };
    if let Err(e) = state
        .channel_router
        .deliver(
            channel,
            account_id,
            sender_id,
            Some(message_id.to_owned()),
            notice,
        )
        .await
    {
        warn!(error = %e, "channel inbound: failed to tell the sender why the message was not delivered");
    }
}

/// `dm_policy = "open"` admits everyone. `allowlist` admits TOML
/// `allow_from` and the DB `channel_allowlist` (read per message, so a
/// revocation applies to the next one). Anyone else may redeem an invite
/// code for this account, sent as it is or as `/start <code>`; a wrong,
/// used, expired or rate-limited one gets the same answer, so the sender
/// cannot tell which.
async fn sender_access(
    state: &AppState,
    channel: &str,
    agent_id: havn_core::AgentId,
    inb: &ChannelInbound,
) -> SenderAccess {
    let Some(acct) = state
        .channels
        .get(channel)
        .and_then(|entry| entry.accounts.iter().find(|a| a.id == inb.account_id))
    else {
        return SenderAccess::Denied(Refusal::Unpaired);
    };
    match acct.dm_policy {
        DmPolicy::Open => return SenderAccess::Allowed,
        DmPolicy::Allowlist => {}
    }
    if acct.allow_from.iter().any(|id| id == &inb.sender_id) {
        return SenderAccess::Allowed;
    }
    match channel_access::is_allowed(&state.db, channel, &inb.account_id, &inb.sender_id).await {
        Ok(true) => return SenderAccess::Allowed,
        Ok(false) => {}
        Err(e) => {
            warn!(channel, account = %inb.account_id, error = %e, "dm_policy: allowlist lookup failed; rejecting");
            return SenderAccess::Denied(Refusal::Unpaired);
        }
    }

    let ChannelMessageContent::Text { text } = &inb.content else {
        return SenderAccess::Denied(Refusal::Unpaired);
    };
    // `/start` alone welcomes; `/start <code>`, from a pairing link,
    // pairs; `/help` explains. Any other text that reads as a code pairs.
    let candidate = match bot_command(text) {
        Some(("start", "")) => return SenderAccess::Denied(Refusal::Welcome),
        Some(("start", payload)) => payload,
        Some(("help", _)) => return SenderAccess::Denied(Refusal::Help),
        Some(_) => return SenderAccess::Denied(Refusal::Unpaired),
        None => text.as_str(),
    };
    let Some(code) = channel_access::normalize_code(candidate) else {
        return SenderAccess::Denied(if candidate == text.as_str() {
            Refusal::Unpaired
        } else {
            Refusal::InvalidCode
        });
    };
    let limiter_key = format!("{channel}|{}|{}", inb.account_id, inb.sender_id);
    match state.invite_attempt_limiter.check(&limiter_key) {
        InviteAttempt::Allowed => {}
        InviteAttempt::Limited { first } => {
            if first {
                warn!(
                    channel,
                    account = %inb.account_id,
                    sender = %inb.sender_id,
                    "invite: too many attempts from this sender; ignoring codes for the rest of the window"
                );
            }
            return SenderAccess::Denied(Refusal::InvalidCode);
        }
    }
    let invite = match channel_access::redeem(
        &state.db,
        channel,
        &inb.account_id,
        &inb.sender_id,
        &code,
        chrono::Utc::now(),
    )
    .await
    {
        Ok(Some(invite)) => invite,
        Ok(None) => {
            warn!(
                channel,
                account = %inb.account_id,
                sender = %inb.sender_id,
                "invite: failed attempt (unknown, used, revoked, expired or other-account code)"
            );
            return SenderAccess::Denied(Refusal::InvalidCode);
        }
        Err(e) => {
            warn!(channel, account = %inb.account_id, error = %e, "invite: redeem failed; rejecting");
            return SenderAccess::Denied(Refusal::InvalidCode);
        }
    };
    info!(
        channel,
        account = %inb.account_id,
        sender = %inb.sender_id,
        invite = %invite.id,
        "invite: sender admitted"
    );
    let details = serde_json::json!({
        "channel": channel,
        "account_id": inb.account_id,
        "sender_id": inb.sender_id,
        "invite_id": invite.id.to_string(),
    });
    match agent_repo::find_by_id(&state.db, agent_id).await {
        Ok(Some(agent)) => {
            crate::audit::record(
                &state.db,
                agent.owner_id,
                agent.team_id,
                Some(agent_id),
                "channel_access.granted",
                details,
            )
            .await;
        }
        _ => {
            crate::audit::record_user_action(
                &state.db,
                state.bootstrap_user_id,
                "channel_access.granted",
                details,
            )
            .await;
        }
    }
    SenderAccess::Admitted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_constant_time_basic() {
        assert!(tokens_match(b"abc123", b"abc123"));
        assert!(!tokens_match(b"abc123", b"abc124"));
        assert!(!tokens_match(b"abc", b"abcd"));
        assert!(tokens_match(b"", b""));
    }

    #[test]
    fn tokens_match_rejects_length_mismatch_fast() {
        // Different lengths short-circuit (we don't hash-extend); the
        // intent is "no early per-byte exit on equal-length inputs",
        // which the all-equal-length tests above pin.
        assert!(!tokens_match(b"x", b"xxxx"));
    }

    #[test]
    fn a_sender_gets_a_few_answers_a_minute() {
        let limiter = DmRejectLimiter::new();
        let start = Instant::now();
        for _ in 0..DM_REPLY_LIMIT {
            assert!(limiter.should_send_at("telegram|bot|tg:1", start));
        }
        assert!(!limiter.should_send_at("telegram|bot|tg:1", start));
        assert!(
            limiter.should_send_at("telegram|bot|tg:2", start),
            "senders are counted apart"
        );
        assert!(limiter.should_send_at("telegram|bot|tg:1", start + DM_REPLY_WINDOW));
    }

    #[test]
    fn a_flood_of_new_senders_is_bounded() {
        let limiter = DmRejectLimiter::new();
        let start = Instant::now();
        for i in 0..DM_REPLY_MAX_KEYS {
            assert!(limiter.should_send_at(&format!("telegram|bot|flood:{i}"), start));
        }
        assert!(!limiter.should_send_at("telegram|bot|tg:new", start));
        assert!(limiter.should_send_at("telegram|bot|tg:new", start + DM_REPLY_WINDOW));
    }

    #[test]
    fn bot_commands_are_read_with_or_without_the_bot_name() {
        assert_eq!(bot_command("/start"), Some(("start", "")));
        assert_eq!(
            bot_command(" /start@acme_bot  7K3M-QX9P "),
            Some(("start", "7K3M-QX9P"))
        );
        assert_eq!(bot_command("/help@acme_bot"), Some(("help", "")));
        assert_eq!(bot_command("hello /start"), None);
        assert_eq!(bot_command(""), None);
    }

    #[test]
    fn invite_attempt_limiter_caps_each_sender_per_window() {
        let limiter = InviteAttemptLimiter::new();
        let start = Instant::now();
        for _ in 0..INVITE_ATTEMPT_LIMIT {
            assert_eq!(
                limiter.check_at("telegram|bot|tg:1", start),
                InviteAttempt::Allowed
            );
        }
        assert_eq!(
            limiter.check_at("telegram|bot|tg:1", start),
            InviteAttempt::Limited { first: true }
        );
        assert_eq!(
            limiter.check_at("telegram|bot|tg:1", start),
            InviteAttempt::Limited { first: false },
            "a refused sender is logged once per window"
        );
        assert_eq!(
            limiter.check_at("telegram|bot|tg:2", start),
            InviteAttempt::Allowed
        );
        let later = start + INVITE_ATTEMPT_WINDOW;
        assert_eq!(
            limiter.check_at("telegram|bot|tg:1", later),
            InviteAttempt::Allowed
        );
    }

    #[test]
    fn invite_attempt_limiter_evicts_the_oldest_sender_when_full() {
        let limiter = InviteAttemptLimiter::new();
        let start = Instant::now();
        for i in 0..INVITE_LIMITER_MAX_KEYS {
            assert_eq!(
                limiter.check_at(&format!("telegram|bot|tg:{i}"), start),
                InviteAttempt::Allowed
            );
        }
        for _ in 0..INVITE_ATTEMPT_LIMIT {
            limiter.check_at("telegram|bot|tg:1", start);
        }
        assert_eq!(
            limiter.check_at("telegram|bot|tg:new", start),
            InviteAttempt::Allowed,
            "a new sender is admitted while the limiter is full"
        );
        {
            let state = limiter.state.lock().unwrap();
            assert_eq!(state.windows.len(), INVITE_LIMITER_MAX_KEYS);
            assert!(!state.windows.contains_key("telegram|bot|tg:0"));
        }
        assert!(
            matches!(
                limiter.check_at("telegram|bot|tg:1", start),
                InviteAttempt::Limited { .. }
            ),
            "senders still tracked keep their count"
        );
        for i in 0..INVITE_LIMITER_MAX_KEYS {
            limiter.check_at(&format!("telegram|bot|flood:{i}"), start);
        }
        let later = start + INVITE_ATTEMPT_WINDOW;
        assert_eq!(
            limiter.check_at("telegram|bot|tg:late", later),
            InviteAttempt::Allowed
        );
        let state = limiter.state.lock().unwrap();
        assert_eq!(state.windows.len(), 1, "the prune drops expired windows");
        assert_eq!(state.order.len(), 1, "and their stale order entries");
    }

    mod invites {
        #![allow(clippy::expect_used, clippy::unwrap_used)]

        use super::super::*;
        use crate::config::{ChannelAccount, ChannelBindingConfig, ChannelEntry};
        use havn_db::repo::audit;

        struct Fixture {
            state: AppState,
            agent: mpsc::Receiver<GatewayToAgent>,
            adapter: mpsc::Receiver<ChannelFrame>,
        }

        async fn fixture(dm_policy: DmPolicy, allow_from: &[&str]) -> Fixture {
            let (mut state, _) = crate::auth::tests::state_with(false).await;
            let agent_id = havn_core::AgentId::new();
            state.bindings = Arc::new(vec![ChannelBindingConfig {
                agent_id: agent_id.to_string(),
                channel: "telegram".into(),
                account_id: "bot".into(),
            }]);
            let mut channels = std::collections::BTreeMap::new();
            channels.insert(
                "telegram".to_string(),
                ChannelEntry {
                    accounts: ["bot", "other-bot"]
                        .into_iter()
                        .map(|id| ChannelAccount {
                            id: id.into(),
                            adapter_token_ref: format!("secret:channel:telegram:{id}"),
                            dm_policy,
                            allow_from: allow_from.iter().map(|s| (*s).to_string()).collect(),
                            admins: Vec::new(),
                        })
                        .collect(),
                },
            );
            state.channels = Arc::new(channels);
            state
                .registry
                .register(havn_spawner::AgentHandle::new(agent_id, 1))
                .await;
            let (agent_tx, agent) = mpsc::channel(16);
            state
                .registry
                .attach(agent_id, agent_tx)
                .await
                .expect("attach");
            let (adapter_tx, adapter) = mpsc::channel(16);
            state
                .channel_router
                .register(AccountKey::new("telegram", "bot"), adapter_tx)
                .await;
            Fixture {
                state,
                agent,
                adapter,
            }
        }

        #[test]
        fn a_message_carries_only_files_its_sender_sent() {
            let agent = havn_core::AgentId::new();
            let file = |path: &str| havn_core::Attachment {
                path: path.into(),
                name: "a.png".into(),
                media_type: "image/png".into(),
                size: 3,
                text_path: None,
            };
            {
                let mut waiting = received().lock().expect("lock");
                for (path, sender) in [("inbox/1-a.png", "tg:1"), ("inbox/2-a.png", "tg:2")] {
                    waiting.insert(
                        (agent, path.into()),
                        Received {
                            channel: "telegram".into(),
                            account: "bot".into(),
                            sender: sender.into(),
                            attachment: file(path),
                            at: Instant::now(),
                        },
                    );
                }
            }
            let named = [
                file("inbox/1-a.png"),
                file("inbox/2-a.png"),
                file("inbox/owner-passport.png"),
            ];
            let kept = take_received(agent, "telegram", "bot", "tg:1", &named);
            assert_eq!(kept, [file("inbox/1-a.png")]);
            // Once: a second message cannot name it again.
            assert!(take_received(agent, "telegram", "bot", "tg:1", &named[..1]).is_empty());
            // Another account of the channel cannot name tg:2's either.
            assert!(take_received(agent, "telegram", "other", "tg:2", &named[1..2]).is_empty());
            assert_eq!(
                take_received(agent, "telegram", "bot", "tg:2", &named[1..2]),
                [file("inbox/2-a.png")]
            );
        }

        #[test]
        fn files_of_a_message_that_did_not_go_are_put_back() {
            let agent = havn_core::AgentId::new();
            let file = havn_core::Attachment {
                path: "inbox/3-b.pdf".into(),
                name: "b.pdf".into(),
                media_type: "application/pdf".into(),
                size: 9,
                text_path: Some("inbox/3-b.pdf.txt".into()),
            };
            restore_received(agent, "telegram", "bot", "tg:7", vec![file.clone()]);
            let taken = take_received(
                agent,
                "telegram",
                "bot",
                "tg:7",
                std::slice::from_ref(&file),
            );
            assert_eq!(taken, std::slice::from_ref(&file));
            restore_received(agent, "telegram", "bot", "tg:7", taken);
            assert_eq!(
                take_received(
                    agent,
                    "telegram",
                    "bot",
                    "tg:7",
                    std::slice::from_ref(&file)
                ),
                [file]
            );
        }

        #[test]
        fn a_sender_may_send_so_many_files_an_hour() {
            let sender = format!("tg:{}", uuid::Uuid::now_v7().simple());
            for _ in 0..MAX_SENDER_FILES {
                assert!(sender_may_send("telegram", "bot", &sender));
            }
            assert!(!sender_may_send("telegram", "bot", &sender));
            assert!(sender_may_send("telegram", "other", &sender), "per account");
        }

        fn text_from(sender: &str, text: &str) -> ChannelInbound {
            ChannelInbound {
                seq: 1,
                account_id: "bot".into(),
                channel_message_id: "m1".into(),
                sender_id: sender.into(),
                content: ChannelMessageContent::Text { text: text.into() },
                timestamp: chrono::Utc::now(),
                raw: None,
                attachments: Vec::new(),
            }
        }

        impl Fixture {
            async fn send(&mut self, sender: &str, text: &str) {
                handle_inbound(&self.state, "telegram", text_from(sender, text)).await;
            }

            fn forwarded(&mut self) -> Vec<String> {
                let mut out = Vec::new();
                while let Ok(frame) = self.agent.try_recv() {
                    if let GatewayToAgent::InboundMessage(m) = frame {
                        out.push(m.sender_id);
                    }
                }
                out
            }

            fn replies(&mut self) -> Vec<(String, String)> {
                let mut out = Vec::new();
                while let Ok(frame) = self.adapter.try_recv() {
                    if let ChannelFrame::Outbound(o) = frame
                        && let ChannelMessageContent::Text { text } = o.content
                    {
                        out.push((o.to, text));
                    }
                }
                out
            }

            async fn invite(&self, account: &str) -> String {
                channel_access::create_invite(
                    &self.state.db,
                    "telegram",
                    account,
                    channel_access::DEFAULT_TTL,
                )
                .await
                .expect("invite")
                .code
            }

            async fn allowed(&self, sender: &str) -> bool {
                channel_access::is_allowed(&self.state.db, "telegram", "bot", sender)
                    .await
                    .unwrap()
            }
        }

        #[tokio::test]
        async fn a_stopping_gateway_tells_the_sender_instead_of_forwarding() {
            let mut f = fixture(DmPolicy::Open, &[]).await;
            crate::supervisor::drain(&f.state, std::time::Duration::ZERO, std::future::pending())
                .await;
            f.send("tg:1", "hello").await;
            assert!(f.forwarded().is_empty());
            assert_eq!(
                f.replies(),
                vec![("tg:1".to_string(), GATEWAY_RESTARTING_MESSAGE.to_string())]
            );
        }

        #[tokio::test]
        async fn valid_code_admits_without_forwarding() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = f.invite("bot").await;
            f.send("tg:1", &code.to_lowercase()).await;
            assert!(
                f.forwarded().is_empty(),
                "the code must not reach the agent"
            );
            assert_eq!(
                f.replies(),
                vec![("tg:1".to_string(), INVITE_WELCOME_MESSAGE.to_string())]
            );
            assert!(f.allowed("tg:1").await);
            let entries = audit::list(
                &f.state.db,
                audit::ListFilter {
                    action_prefix: Some("channel_access.".into()),
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].details["sender_id"], "tg:1");

            f.send("tg:1", "hello").await;
            assert_eq!(f.forwarded(), vec!["tg:1".to_string()]);
            assert!(f.replies().is_empty());
        }

        #[tokio::test]
        async fn reused_and_other_account_codes_are_not_valid() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = f.invite("bot").await;
            f.send("tg:1", &code).await;
            f.replies();
            f.send("tg:2", &code).await;
            let other = f.invite("other-bot").await;
            f.send("tg:3", &other).await;
            f.send("tg:4", "AAAA-AAAA-AAAA-AAAA").await;
            assert!(f.forwarded().is_empty());
            let replies = f.replies();
            assert_eq!(replies.len(), 3);
            assert!(
                replies.iter().all(|(_, text)| text == PAIR_INVALID_CODE),
                "a used, other-account or unknown code gets the same answer: {replies:?}"
            );
            assert!(!f.allowed("tg:2").await);
            assert!(!f.allowed("tg:3").await);
        }

        #[tokio::test]
        async fn a_sender_who_is_not_paired_is_guided() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            f.send("tg:1", "/start").await;
            f.send("tg:2", "/help@bot").await;
            f.send("tg:3", "hello there").await;
            f.send("tg:4", "/start not-a-code").await;
            f.send("tg:5", "/model").await;
            assert!(f.forwarded().is_empty());
            assert_eq!(
                f.replies(),
                vec![
                    ("tg:1".to_string(), PAIR_WELCOME.to_string()),
                    ("tg:2".to_string(), PAIR_HELP.to_string()),
                    ("tg:3".to_string(), PAIR_UNPAIRED.to_string()),
                    ("tg:4".to_string(), PAIR_INVALID_CODE.to_string()),
                    ("tg:5".to_string(), PAIR_UNPAIRED.to_string()),
                ]
            );
            // Every message is answered, a few a minute.
            for _ in 0..DM_REPLY_LIMIT + 2 {
                f.send("tg:6", "hi").await;
            }
            assert_eq!(f.replies().len(), DM_REPLY_LIMIT as usize);
        }

        #[tokio::test]
        async fn a_pairing_link_pairs_with_start_and_its_code() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = f.invite("bot").await;
            f.send("tg:1", &format!("/start {}", code.replace('-', "")))
                .await;
            assert!(f.allowed("tg:1").await);
            assert_eq!(
                f.replies(),
                vec![("tg:1".to_string(), INVITE_WELCOME_MESSAGE.to_string())]
            );
            assert!(f.forwarded().is_empty());
            f.send("tg:1", "/start").await;
            assert_eq!(
                f.forwarded(),
                vec!["tg:1".to_string()],
                "a paired /start goes to the agent"
            );
        }

        #[tokio::test]
        async fn texts_set_in_config_are_used_and_follow_a_reload() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let texts: crate::config::GatewayConfig = toml::from_str(
                "[channel_texts.telegram.bot]\nwelcome = \"欢迎！请发送配对码。\"\nunpaired = \"   \"\n",
            )
            .expect("config");
            f.state.channel_texts.store(Arc::new(texts.channel_texts));
            f.send("tg:1", "/start").await;
            f.send("tg:2", "hi").await;
            assert_eq!(
                f.replies(),
                vec![
                    ("tg:1".to_string(), "欢迎！请发送配对码。".to_string()),
                    ("tg:2".to_string(), PAIR_UNPAIRED.to_string()),
                ],
                "a blank text keeps the default"
            );
            f.state.channel_texts.store(Arc::new(Default::default()));
            f.send("tg:3", "/start").await;
            assert_eq!(
                f.replies(),
                vec![("tg:3".to_string(), PAIR_WELCOME.to_string())]
            );
        }

        #[tokio::test]
        async fn an_expired_code_is_not_valid() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = channel_access::create_invite(
                &f.state.db,
                "telegram",
                "bot",
                chrono::Duration::seconds(-60),
            )
            .await
            .expect("invite")
            .code;
            f.send("tg:1", &code).await;
            assert!(f.forwarded().is_empty());
            assert_eq!(
                f.replies(),
                vec![("tg:1".to_string(), PAIR_INVALID_CODE.to_string())]
            );
            assert!(!f.allowed("tg:1").await);
        }

        #[tokio::test]
        async fn revocation_applies_to_the_next_message() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = f.invite("bot").await;
            f.send("tg:1", &code).await;
            f.send("tg:1", "first").await;
            assert_eq!(f.forwarded(), vec!["tg:1".to_string()]);
            channel_access::revoke_sender(&f.state.db, "telegram", "bot", "tg:1")
                .await
                .expect("revoke");
            f.send("tg:1", "second").await;
            assert!(f.forwarded().is_empty());
        }

        #[tokio::test]
        async fn rate_limited_sender_cannot_redeem_even_a_valid_code() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            for _ in 0..INVITE_ATTEMPT_LIMIT {
                f.send("tg:1", "AAAA-AAAA-AAAA-AAAA").await;
            }
            let code = f.invite("bot").await;
            f.send("tg:1", &code).await;
            assert!(!f.allowed("tg:1").await);
            assert!(f.forwarded().is_empty());
            f.send("tg:2", &code).await;
            assert!(f.allowed("tg:2").await, "other senders keep their budget");
        }

        #[tokio::test]
        async fn new_sender_redeems_while_the_limiter_is_full() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            for i in 0..INVITE_LIMITER_MAX_KEYS {
                f.state
                    .invite_attempt_limiter
                    .check(&format!("telegram|bot|flood:{i}"));
            }
            let code = f.invite("bot").await;
            f.send("tg:new", &code).await;
            assert!(f.allowed("tg:new").await);
            assert_eq!(
                f.replies(),
                vec![("tg:new".to_string(), INVITE_WELCOME_MESSAGE.to_string())]
            );
        }

        #[tokio::test]
        async fn toml_allow_from_and_invited_senders_are_both_admitted() {
            let mut f = fixture(DmPolicy::Allowlist, &["tg:toml"]).await;
            let code = f.invite("bot").await;
            f.send("tg:db", &code).await;
            f.send("tg:toml", "hi").await;
            f.send("tg:db", "hi").await;
            f.send("tg:nobody", "hi").await;
            assert_eq!(
                f.forwarded(),
                vec!["tg:toml".to_string(), "tg:db".to_string()]
            );
        }

        #[tokio::test]
        async fn open_policy_forwards_codes_unchanged() {
            let mut f = fixture(DmPolicy::Open, &[]).await;
            let code = f.invite("bot").await;
            f.send("tg:1", &code).await;
            assert_eq!(f.forwarded(), vec!["tg:1".to_string()]);
            assert!(!f.allowed("tg:1").await);
        }
    }
}
