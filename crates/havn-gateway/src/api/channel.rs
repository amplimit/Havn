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
/// Senders the reply limiter tracks at once per account; past it, new
/// senders to that account get no answer until older windows end. Per
/// account, so a flood at one bot does not silence another's.
const DM_REPLY_MAX_KEYS: usize = 10_000;
/// Characters of a configured text sent: below every platform's message
/// limit (Telegram's is 4096).
const MAX_PAIRING_TEXT_CHARS: usize = 2000;

/// What an `allowlist` bot tells a sender who is not paired (issue #183),
/// unless `[channel_texts]` says otherwise.
const PAIR_WELCOME: &str = "Hello! This bot needs pairing before you can use it. Send the pairing \
                            code you were given (it looks like 7K3M-QX9P-2HWD-RT4B), or open the \
                            pairing link you got. No code yet? Ask whoever runs this bot.";
const PAIR_UNPAIRED: &str = "Not paired yet. Send your pairing code, or /help.";
const PAIR_INVALID_CODE: &str = "That pairing code is not valid or has expired.";
const PAIR_LINKED_ELSEWHERE: &str = "This chat account is already connected to another Havn \
                                     account. Send /disconnect to end that, then open the \
                                     pairing link again.";
/// What `/disconnect` says (issue #182); `{name}` is the account's name.
const DISCONNECTED_MESSAGE: &str = "Disconnected from Havn account {name}.";
const NOT_CONNECTED_MESSAGE: &str = "This chat account is not connected to a Havn account.";
const PAIR_TOO_MANY_TRIES: &str =
    "Too many tries. Please wait a few minutes, then send your pairing code again.";
const PAIR_HELP: &str = "This bot answers only people who have paired with it. To pair, send the \
                         pairing code you were given (it looks like 7K3M-QX9P-2HWD-RT4B), or open \
                         the pairing link you got. Codes come from whoever runs this bot.";

/// Sent back when an inbound message could not be handed to the agent.
const AGENT_UNREACHABLE_MESSAGE: [&str; 2] = [
    "The agent could not receive your message right now. Please try again later.",
    "agent 现在收不到你的消息，请稍后再试。",
];

/// Sent back when the gateway is restarting and takes no new messages.
const GATEWAY_RESTARTING_MESSAGE: [&str; 2] = [
    "The service is restarting and did not take your message. Please send it again in a minute.",
    "服务正在重启，没有收下你的消息，请过一分钟再发一次。",
];

const INVITE_WELCOME_MESSAGE: &str = "Paired. You can now message this bot.";
/// What a sender is told when a pairing code made on the web links them
/// to a Havn account (issue #182); `{name}` is the account's name.
const LINKED_MESSAGE: &str = "Connected to Havn account {name}. You can now message this bot.";

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
/// Key: `"{channel}|{account_id}|{sender_id}"`; the senders of each
/// `"{channel}|{account_id}"` are counted apart.
#[derive(Debug, Clone, Default)]
pub struct DmRejectLimiter {
    accounts: Arc<std::sync::Mutex<HashMap<String, HashMap<String, (Instant, u32)>>>>,
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
        let (account, sender) = key.rsplit_once('|').unwrap_or(("", key));
        let mut accounts = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
        let map = accounts.entry(account.to_string()).or_default();
        if map.len() >= DM_REPLY_MAX_KEYS && !map.contains_key(sender) {
            map.retain(|_, (start, _)| now.duration_since(*start) < DM_REPLY_WINDOW);
            if map.len() >= DM_REPLY_MAX_KEYS {
                return false;
            }
        }
        let (start, count) = map.entry(sender.to_string()).or_insert((now, 0));
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
    /// On the WebSocket upgrade: the last outbound seq the adapter had
    /// (havn-channel-telegram issue #23). The replay of what came after it
    /// starts as the adapter is registered, before any new reply goes to
    /// it, which a `Resume` frame read later cannot promise.
    #[serde(default)]
    pub since: Option<u64>,
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
    let since = q.since;
    Ok(ws.on_upgrade(move |socket| handle_session(socket, state, channel_id, account_id, since)))
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
const FILE_LOST_MESSAGE: [&str; 2] = [
    "The file did not reach the agent (it may have restarted meanwhile). Please send it again.",
    "文件没有送到 agent（它可能在这期间重启了），请再发一次。",
];

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
        since: None,
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

/// Send the adapter of `key` (through `frame_tx`) the outbound frames kept
/// after `since_seq`, and those delivered while it does, until the buffer
/// has nothing newer (spec §3.2).
async fn replay(
    state: &AppState,
    key: &AccountKey,
    since_seq: u64,
    frame_tx: &mpsc::Sender<ChannelFrame>,
) {
    // A seq of another gateway (one that ran before with a clock ahead,
    // or another instance) resumes from the start of what is kept.
    let since = state.channel_router.resume_point(key, since_seq).await;
    let mut frames = state
        .channel_router
        .replay_since(key, since, frame_tx)
        .await;
    let mut replayed = 0usize;
    let mut cursor = since;
    'replay: loop {
        for frame in frames {
            if let ChannelFrame::Outbound(o) = &frame {
                cursor = cursor.max(o.seq);
            }
            if frame_tx.send(frame).await.is_err() {
                warn!(
                    channel = %key.channel,
                    account = %key.account_id,
                    since_seq,
                    "channel WS: outbound mpsc closed mid-replay"
                );
                break 'replay;
            }
            replayed += 1;
        }
        frames = state
            .channel_router
            .replay_more(key, cursor, frame_tx)
            .await;
        if frames.is_empty() {
            break;
        }
    }
    info!(
        channel = %key.channel,
        account = %key.account_id,
        since_seq,
        from = since,
        replayed,
        "channel WS: resume processed"
    );
}

async fn handle_session(
    socket: WebSocket,
    state: AppState,
    channel_id: String,
    account_id: String,
    since: Option<u64>,
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
    // Resuming from `since`: new replies only wait in the buffer until the
    // replay below has sent what came before them.
    if since.is_some() {
        state
            .channel_router
            .register_resuming(key.clone(), frame_tx)
            .await;
    } else {
        state.channel_router.register(key.clone(), frame_tx).await;
    }

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

    if let Some(since) = since
        && let Some(frame_tx) = registered.upgrade()
    {
        replay(&state, &key, since, &frame_tx).await;
    }

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
                // seq > since_seq through the adapter's mpsc.
                let Some(frame_tx) = registered.upgrade() else {
                    break;
                };
                replay(&state, &key, r.since_seq, &frame_tx).await;
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
    if access == SenderAccess::Disconnect {
        disconnect(state, channel, &inb).await;
        return;
    }
    if matches!(access, SenderAccess::Admitted | SenderAccess::Linked(_)) {
        let text = match &access {
            SenderAccess::Linked(name) => linked_text(state, channel, &inb.account_id, name),
            _ => pairing_text(state, channel, &inb.account_id, Refusal::Paired),
        };
        let welcome = ChannelMessageContent::Text { text };
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
            // As often as the sender is answered: a flood does not write
            // on every message.
            refused(state, channel, agent_id, &inb, why).await;
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
    // A choice to send as the person's message, and the chat it goes to.
    let mut said = None;
    match parsed {
        None => {}
        Some(
            refused @ (crate::commands::Parsed::Unknown(_) | crate::commands::Parsed::Misused(_)),
        ) => {
            let language = crate::commands::language_of(state, agent_id).await;
            let text = refused.refusal(language).unwrap_or_default();
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
                let language = crate::commands::language_of(state, agent_id).await;
                let asker = crate::commands::Asker {
                    agent_id,
                    scope: encoded_channel_id.clone(),
                    conversation: None,
                    who: format!("{} on {channel} account {}", inb.sender_id, inb.account_id),
                    who_shown: language.pick(
                        format!("{} on {channel} account {}", inb.sender_id, inb.account_id),
                        format!("{channel} 账号 {} 上的 {}", inb.account_id, inb.sender_id),
                    ),
                    admin: is_admin(state, channel, &inb.account_id, &inb.sender_id, agent_id)
                        .await,
                    owner: owner_linked(state, channel, agent_id, &inb.sender_id).await,
                    admin_hint: language.pick(
                        "the agent's owner (after connecting this account on the agent's \
                         settings page) and the senders listed under the account's `admins` in \
                         the gateway's config.toml",
                        "agent 的所有者（在 agent 设置页关联这个账号之后），以及网关 config.toml \
                         里这个账号 `admins` 下列出的发送者",
                    ),
                    language,
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
                    // A choice: the person's message, in the question's chat.
                    crate::commands::Action::Say { text, question } => {
                        said = Some((text, question));
                    }
                }
            }
        },
    }
    // An answer goes to the chat in use: an earlier one it was asked in
    // does not come back with it. The answer names its question.
    let (content, answered) = match said {
        Some((text, question)) => (MessageContent::Text { text }, Some(question)),
        None => (content, None),
    };
    // Whether the owner writes, through a linked account (issue #221).
    let from_owner = owner_linked(state, channel, agent_id, &inb.sender_id).await;
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
        from_owner,
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
            crate::commands::language_of(state, agent_id)
                .await
                .of(GATEWAY_RESTARTING_MESSAGE),
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
        // Its language only when it is not sent, which is rare.
        let agent_model = crate::commands::saved_already(&frame, havn_core::Language::English)
            .is_some()
            .then(|| frame.clone());
        if let Err(e) = state.registry.send(agent_id, frame).await {
            warn!(channel, %agent_id, error = ?e, "failed to forward a chat command");
            let language = crate::commands::language_of(state, agent_id).await;
            let saved = agent_model
                .as_ref()
                .and_then(|frame| crate::commands::saved_already(frame, language));
            let text = match (saved, e) {
                (Some(saved), _) => saved,
                (None, SendError::Draining) => language.of(GATEWAY_RESTARTING_MESSAGE),
                (None, _) => language.of(AGENT_UNREACHABLE_MESSAGE),
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
                crate::commands::language_of(state, agent_id)
                    .await
                    .of(FILE_LOST_MESSAGE),
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
    let sent = state.registry.send(agent_id, frame).await;
    // An answer that reached the agent is not taken again.
    if sent.is_ok()
        && let Some(question) = answered
    {
        state.webchat_router.forget_choice(&question);
    }
    if let Err(e) = sent {
        restore_received(agent_id, channel, &inb.account_id, &inb.sender_id, carried);
        warn!(
            channel,
            account = %inb.account_id,
            %agent_id,
            error = ?e,
            "channel inbound: failed to forward to agent (offline?)"
        );
        let language = crate::commands::language_of(state, agent_id).await;
        let text = match e {
            SendError::Draining => language.of(GATEWAY_RESTARTING_MESSAGE),
            _ => language.of(AGENT_UNREACHABLE_MESSAGE),
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

/// Whether `sender_id` may use the admin commands on `(channel,
/// account_id)`: listed among the account's admins in the gateway's
/// config, or linked from the web to the agent's owner (issue #182).
async fn is_admin(
    state: &AppState,
    channel: &str,
    account_id: &str,
    sender_id: &str,
    agent_id: havn_core::AgentId,
) -> bool {
    let listed = state
        .channels
        .get(channel)
        .and_then(|entry| entry.accounts.iter().find(|a| a.id == account_id))
        .is_some_and(|acct| acct.admins.iter().any(|id| id == sender_id));
    listed || owner_linked(state, channel, agent_id, sender_id).await
}

/// Whether `sender_id` on `channel` is linked to the owner of `agent_id`.
async fn owner_linked(
    state: &AppState,
    channel: &str,
    agent_id: havn_core::AgentId,
    sender_id: &str,
) -> bool {
    let Ok(Some(link)) = channel_access::linked_user(&state.db, channel, sender_id).await else {
        return false;
    };
    matches!(
        agent_repo::find_by_id(&state.db, agent_id).await,
        Ok(Some(agent)) if agent.owner_id == link.user_id
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SenderAccess {
    /// Forward the message to the agent.
    Allowed,
    /// The message redeemed an invite: welcome the sender, do not forward.
    Admitted,
    /// The message redeemed a pairing code made on the web, which linked
    /// the sender to the Havn account named: say so, do not forward.
    Linked(String),
    /// Not forwarded: the sender is not paired; how it is answered.
    Denied(Refusal),
    /// `/disconnect`: end the sender's link to a Havn account, whoever's.
    Disconnect,
}

/// What a sender who is not paired is told (issue #183).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// `/start` without a code.
    Welcome,
    /// `/help`.
    Help,
    /// A code that is wrong, used, expired or for another bot.
    InvalidCode,
    /// A code past the attempts allowed, valid or not.
    TooManyTries,
    /// A code made on the web, for a sender linked to another Havn user.
    LinkedElsewhere,
    /// Anything else.
    Unpaired,
    /// Not a refusal: the code was accepted.
    Paired,
}

/// Whether `inb`'s sender is let in to its `allowlist` account all the
/// same: a refused code from someone admitted is not a stranger writing.
async fn admitted_anyway(
    state: &AppState,
    channel: &str,
    agent_id: havn_core::AgentId,
    inb: &ChannelInbound,
) -> bool {
    let Some(account) = state
        .channels
        .get(channel)
        .and_then(|entry| entry.accounts.iter().find(|a| a.id == inb.account_id))
    else {
        return true;
    };
    account.dm_policy != DmPolicy::Allowlist
        || account.allow_from.iter().any(|s| s == &inb.sender_id)
        || channel_access::is_allowed(&state.db, channel, &inb.account_id, &inb.sender_id)
            .await
            .unwrap_or(true)
        || owner_linked(state, channel, agent_id, &inb.sender_id).await
}

/// `said` with what reads as a pairing code in it left out: only codes'
/// hashes are kept.
fn without_codes(said: &str) -> String {
    static CODE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    let Some(code) = CODE.get_or_init(|| {
        // As codes are shown (four groups of four, with dashes), or written
        // in one: no words of a sentence run together into one.
        regex::Regex::new(
            r"\b(?:[0-9A-Za-z]{4}[\-\u{2010}-\u{2015}\u{2212}\u{ff0d}]){3}[0-9A-Za-z]{4}\b|\b[0-9A-Za-z]{16}\b",
        )
        .ok()
    }) else {
        return said.to_string();
    };
    code.replace_all(said, |m: &regex::Captures<'_>| {
        let written = &m[0];
        // A word of sixteen letters is a word; a code has a digit or dashes
        // nearly always.
        let shaped = written.len() > 16 || written.chars().any(|c| c.is_ascii_digit());
        if shaped && channel_access::normalize_code(written).is_some() {
            "(a code)".to_string()
        } else {
            m[0].to_string()
        }
    })
    .into_owned()
}

/// `text` on one line, with every ASCII punctuation mark escaped, so the
/// Markdown a chat app reads in it makes no link or format of it.
fn as_plain(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Keep that `inb`'s sender was turned away for `why` (issue #231), and
/// tell the agent's owner the first time in a day, in their own chat with
/// the bot, with a button that lets the sender in. A code sent is not
/// kept.
async fn refused(
    state: &AppState,
    channel: &str,
    agent_id: havn_core::AgentId,
    inb: &ChannelInbound,
    why: Refusal,
) {
    if admitted_anyway(state, channel, agent_id, inb).await {
        return;
    }
    let said = match (why, &inb.content) {
        (Refusal::InvalidCode | Refusal::TooManyTries | Refusal::LinkedElsewhere, _) => {
            "(a pairing code that did not work)".to_string()
        }
        (Refusal::Welcome, _) => "/start".to_string(),
        (Refusal::Help, _) => "/help".to_string(),
        (_, ChannelMessageContent::Text { text }) => text.clone(),
        (_, ChannelMessageContent::Image { caption, .. }) => {
            format!("(a picture) {}", caption.as_deref().unwrap_or_default())
        }
        (_, ChannelMessageContent::File { filename, .. }) => format!("(a file: {filename})"),
        (_, ChannelMessageContent::Audio { .. }) => "(a voice message)".to_string(),
        (_, _) => String::new(),
    };
    let said = without_codes(&said);
    let now = chrono::Utc::now();
    let notify = match channel_access::record_refusal(
        &state.db,
        channel,
        &inb.account_id,
        &inb.sender_id,
        &said,
        now,
    )
    .await
    {
        Ok(notify) => notify,
        Err(e) => {
            warn!(channel, account = %inb.account_id, error = %e, "could not keep a refused sender");
            return;
        }
    };
    if !notify {
        return;
    }
    let Ok(Some(agent)) = agent_repo::find_by_id(&state.db, agent_id).await else {
        return;
    };
    if agent
        .config
        .get("notify_unpaired")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        return;
    }
    let Ok(links) = channel_access::list_links(&state.db, Some(agent.owner_id)).await else {
        return;
    };
    let Some(owner) = links.iter().find(|l| l.channel == channel) else {
        return;
    };
    if owner.sender_id == inb.sender_id {
        return;
    }
    let language = havn_core::Language::of_config(&agent.config);
    let said = as_plain(
        &said
            .chars()
            .take(channel_access::REFUSAL_SAID_CHARS)
            .collect::<String>(),
    );
    let data = format!("/allow {}", inb.sender_id);
    let fits = data.len() <= havn_core::MAX_BUTTON_DATA;
    let text = language.pick(
        format!(
            "{} wrote to this bot and is not paired, so {} did not get it: \"{said}\". {}",
            inb.sender_id,
            agent.name,
            if fits {
                "Let them talk to it?"
            } else {
                "Let them in from the agent's settings page, under who can reach the bot."
            }
        ),
        format!(
            "{} 给这个机器人发了消息，但还没配对，所以 {} 没有收到：「{said}」。{}",
            inb.sender_id,
            agent.name,
            if fits {
                "要让对方和它对话吗？"
            } else {
                "可以在 agent 设置页的“谁能联系机器人”里放行。"
            }
        ),
    );
    let buttons = if fits {
        vec![vec![havn_core::Button {
            label: language.pick("Allow", "放行").into(),
            data,
        }]]
    } else {
        Vec::new()
    };
    if let Err(e) = state
        .channel_router
        .deliver_with(
            channel,
            &inb.account_id,
            &owner.sender_id,
            None,
            ChannelMessageContent::Text { text },
            buttons,
            None,
            false,
        )
        .await
    {
        warn!(channel, account = %inb.account_id, error = %e, "could not tell the owner about a refused sender");
        return;
    }
    if let Err(e) =
        channel_access::refusal_told(&state.db, channel, &inb.account_id, &inb.sender_id, now).await
    {
        warn!(channel, error = %e, "could not note that the owner was told");
    }
}

/// Admit `sender_id` to `(channel, account_id)`, bound to `agent`, as its
/// owner asked (issue #231): audited, and the sender is told they can
/// talk now. Whether they were not admitted yet.
pub(crate) async fn admit_sender(
    state: &AppState,
    agent: &agent_repo::Agent,
    channel: &str,
    account_id: &str,
    sender_id: &str,
    from: &str,
) -> Result<bool, havn_db::DbError> {
    let new = channel_access::admit(&state.db, channel, account_id, sender_id).await?;
    if new {
        crate::audit::record(
            &state.db,
            agent.owner_id,
            agent.team_id,
            Some(agent.id),
            "channel_access.granted",
            serde_json::json!({
                "channel": channel,
                "account_id": account_id,
                "sender_id": sender_id,
                "by": "owner",
                "from": from,
            }),
        )
        .await;
        let welcome = ChannelMessageContent::Text {
            text: pairing_text(state, channel, account_id, Refusal::Paired),
        };
        if let Err(e) = state
            .channel_router
            .deliver(channel, account_id, sender_id, None, welcome)
            .await
        {
            warn!(error = %e, "could not tell an admitted sender");
        }
    }
    Ok(new)
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
        Refusal::TooManyTries => (
            own.and_then(|t| t.too_many_tries.clone()),
            PAIR_TOO_MANY_TRIES,
        ),
        Refusal::LinkedElsewhere => (
            own.and_then(|t| t.linked_elsewhere.clone()),
            PAIR_LINKED_ELSEWHERE,
        ),
        Refusal::Unpaired => (own.and_then(|t| t.unpaired.clone()), PAIR_UNPAIRED),
        Refusal::Paired => (own.and_then(|t| t.paired.clone()), INVITE_WELCOME_MESSAGE),
    };
    set.filter(|t| !t.trim().is_empty()).map_or_else(
        || default.to_string(),
        |t| t.chars().take(MAX_PAIRING_TEXT_CHARS).collect(),
    )
}

/// What a sender linked to Havn account `name` is told: the account's
/// `linked` text, or the default, with `{name}` filled in.
fn linked_text(state: &AppState, channel: &str, account_id: &str, name: &str) -> String {
    let texts = state.channel_texts.load();
    let set = texts
        .get(channel)
        .and_then(|accounts| accounts.get(account_id))
        .and_then(|t| t.linked.clone())
        .filter(|t| !t.trim().is_empty());
    set.as_deref()
        .unwrap_or(LINKED_MESSAGE)
        .replace("{name}", name)
        .chars()
        .take(MAX_PAIRING_TEXT_CHARS)
        .collect()
}

/// `text` as a command to a bot: `/name` or `/name@bot` (its name in
/// lower case), then the rest.
fn bot_command(text: &str) -> Option<(String, &str)> {
    let text = text.trim();
    let word = text.split_whitespace().next()?;
    let name = word.strip_prefix('/')?;
    let name = name.split_once('@').map_or(name, |(n, _)| n);
    Some((name.to_ascii_lowercase(), text[word.len()..].trim()))
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
/// `allow_from`, the DB `channel_allowlist` (read per message, so a
/// revocation applies to the next one) and a sender linked to the agent's
/// owner. Anyone else may redeem an invite code for this account, sent as
/// it is or as `/start <code>`; a wrong, used, expired or rate-limited one
/// gets the same answer, so the sender cannot tell which. A pairing link
/// from the web (`/start <code>`) is redeemed for an admitted sender too:
/// it links them to the Havn user who made it (issue #182).
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
    let text = match &inb.content {
        ChannelMessageContent::Text { text } => Some(text.as_str()),
        _ => None,
    };
    let command = text.and_then(bot_command);
    // The person in the chat app may always end a link of theirs, so a
    // link made to the wrong account does not lock them out.
    if command
        .as_ref()
        .is_some_and(|(name, _)| name == "disconnect")
    {
        return SenderAccess::Disconnect;
    }
    let admitted = match acct.dm_policy {
        DmPolicy::Open => true,
        DmPolicy::Allowlist => {
            if acct.allow_from.iter().any(|id| id == &inb.sender_id) {
                true
            } else {
                match channel_access::is_allowed(
                    &state.db,
                    channel,
                    &inb.account_id,
                    &inb.sender_id,
                )
                .await
                {
                    Ok(true) => true,
                    Ok(false) => owner_linked(state, channel, agent_id, &inb.sender_id).await,
                    Err(e) => {
                        warn!(channel, account = %inb.account_id, error = %e, "dm_policy: allowlist lookup failed; rejecting");
                        return SenderAccess::Denied(Refusal::Unpaired);
                    }
                }
            }
        }
    };
    if admitted {
        // Only a `/start <code>` that reads as a code is redeemed; the rest
        // is the agent's.
        return match command
            .as_ref()
            .filter(|(name, payload)| name == "start" && !payload.is_empty())
            .and_then(|(_, payload)| channel_access::normalize_code(payload))
        {
            Some(code) => redeem_code(state, channel, agent_id, inb, &code, true).await,
            None => SenderAccess::Allowed,
        };
    }

    let Some(text) = text else {
        return SenderAccess::Denied(Refusal::Unpaired);
    };
    // `/start` alone welcomes; `/start <code>`, from a pairing link,
    // pairs; `/help` explains. Any other text that reads as a code pairs.
    let candidate = match &command {
        Some((name, "")) if name == "start" => return SenderAccess::Denied(Refusal::Welcome),
        Some((name, payload)) if name == "start" => *payload,
        Some((name, _)) if name == "help" => return SenderAccess::Denied(Refusal::Help),
        Some(_) => return SenderAccess::Denied(Refusal::Unpaired),
        None => text,
    };
    let Some(code) = channel_access::normalize_code(candidate) else {
        return SenderAccess::Denied(if candidate == text {
            Refusal::Unpaired
        } else {
            Refusal::InvalidCode
        });
    };
    redeem_code(state, channel, agent_id, inb, &code, false).await
}

/// Redeem `code` for the sender of `inb`, within their attempts: they are
/// admitted, and linked when it was made on the web. With `links_only`
/// (a sender already admitted), only a code made on the web is taken, so
/// an invite meant for someone else is not used up.
async fn redeem_code(
    state: &AppState,
    channel: &str,
    agent_id: havn_core::AgentId,
    inb: &ChannelInbound,
    code: &str,
    links_only: bool,
) -> SenderAccess {
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
            return SenderAccess::Denied(Refusal::TooManyTries);
        }
    }
    let invite = match channel_access::redeem(
        &state.db,
        channel,
        &inb.account_id,
        &inb.sender_id,
        code,
        chrono::Utc::now(),
        links_only,
    )
    .await
    {
        Ok(Some(invite)) => invite,
        Err(havn_db::DbError::Conflict(_)) => {
            return SenderAccess::Denied(Refusal::LinkedElsewhere);
        }
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
        linked = invite.user_id.is_some(),
        "invite: sender admitted"
    );
    let details = serde_json::json!({
        "channel": channel,
        "account_id": inb.account_id,
        "sender_id": inb.sender_id,
        "invite_id": invite.id.to_string(),
    });
    let agent = agent_repo::find_by_id(&state.db, agent_id)
        .await
        .ok()
        .flatten();
    match &agent {
        Some(agent) => {
            crate::audit::record(
                &state.db,
                agent.owner_id,
                agent.team_id,
                Some(agent_id),
                "channel_access.granted",
                details.clone(),
            )
            .await;
        }
        None => {
            crate::audit::record_user_action(
                &state.db,
                state.bootstrap_user_id,
                "channel_access.granted",
                details.clone(),
            )
            .await;
        }
    }
    let Some(user) = invite.user_id else {
        return SenderAccess::Admitted;
    };
    crate::audit::record_user_action(&state.db, user, "channel_link.created", details).await;
    let name = havn_db::repo::users::find_by_id(&state.db, user)
        .await
        .ok()
        .flatten()
        .map_or_else(|| "your Havn account".to_string(), |u| u.display_name);
    SenderAccess::Linked(name)
}

/// `/disconnect` from the sender of `inb`: end their link, say so. A
/// sender with no link is told within the limit strangers are answered in.
async fn disconnect(state: &AppState, channel: &str, inb: &ChannelInbound) {
    let text = match channel_access::unlink_sender(&state.db, channel, &inb.sender_id).await {
        Ok(Some(link)) => {
            crate::audit::record_user_action(
                &state.db,
                link.user_id,
                "channel_link.removed",
                serde_json::json!({
                    "channel": channel,
                    "sender_id": inb.sender_id,
                    "by": "the sender",
                }),
            )
            .await;
            DISCONNECTED_MESSAGE.replace("{name}", &link.user_name)
        }
        Ok(None) => {
            let key = format!("{channel}|{}|{}", inb.account_id, inb.sender_id);
            if !state.dm_reject_limiter.should_send(&key) {
                return;
            }
            NOT_CONNECTED_MESSAGE.to_string()
        }
        Err(e) => {
            warn!(channel, error = %e, "disconnect: could not remove the link");
            return;
        }
    };
    tell_sender(
        state,
        channel,
        &inb.account_id,
        &inb.sender_id,
        &inb.channel_message_id,
        &text,
    )
    .await;
}

// ---- Pairing from the web (issue #182) ----

#[derive(Debug, serde::Serialize)]
pub struct PairingCode {
    pub channel: String,
    pub account_id: String,
    /// Shown once: what the person sends to the bot.
    pub code: String,
    /// A link that sends `/start <code>`, when the bot's username is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// `POST /agents/{id}/pairing`: a pairing code for each channel account
/// the agent is bound to, which links the account that redeems it to the
/// caller. Owner only; each lasts 10 minutes and works once.
pub async fn pairing_create(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::Json<Vec<PairingCode>>, crate::api::ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id.to_string();
    let mut codes = Vec::new();
    // An account bound to several agents goes to the first; only the
    // accounts whose messages reach this agent pair to it.
    for binding in state.bindings.iter().filter(|b| {
        b.agent_id == agent_id
            && channel_router::agent_for_account(&state.bindings, &b.channel, &b.account_id)
                == Some(agent_id.as_str())
    }) {
        let Some(acct) = state
            .channels
            .get(&binding.channel)
            .and_then(|entry| entry.accounts.iter().find(|a| a.id == binding.account_id))
        else {
            continue;
        };
        let made = channel_access::create_link_code(&state.db, &binding.channel, &acct.id, user.id)
            .await
            .map_err(|e| crate::api::ApiError::Internal(e.to_string()))?;
        let link = acct
            .bot_username
            .as_deref()
            .map(|u| u.trim_start_matches('@'))
            .filter(|u| binding.channel == "telegram" && !u.is_empty())
            .map(|u| format!("https://t.me/{u}?start={}", made.code));
        codes.push(PairingCode {
            channel: binding.channel.clone(),
            account_id: acct.id.clone(),
            code: made.code,
            link,
            expires_at: made.invite.expires_at,
        });
    }
    if codes.is_empty() {
        return Err(crate::api::ApiError::BadRequest(
            "this agent is bound to no channel account ([[bindings]] in config.toml)".into(),
        ));
    }
    crate::audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "channel_link.code_created",
        serde_json::json!({ "accounts": codes.iter().map(|c| format!("{}/{}", c.channel, c.account_id)).collect::<Vec<_>>() }),
    )
    .await;
    Ok(axum::Json(codes))
}

/// Who can reach one channel account an agent is bound to (issue #231).
#[derive(Debug, serde::Serialize)]
pub struct AccountAccess {
    pub channel: String,
    pub account_id: String,
    /// `allowlist` or `open`: on `open` everyone is let in.
    pub dm_policy: &'static str,
    /// Listed in config.toml, removed only there.
    pub allow_from: Vec<String>,
    /// Let in by an invite code, a connect code or the owner.
    pub allowed: Vec<AllowedView>,
    /// Invite codes made for the account, newest first.
    pub invites: Vec<InviteView>,
    /// Senders turned away, who wrote last first.
    pub refused: Vec<RefusedView>,
}

#[derive(Debug, serde::Serialize)]
pub struct AllowedView {
    pub sender_id: String,
    pub since: chrono::DateTime<chrono::Utc>,
    /// Let in by a code rather than by the owner.
    pub by_code: bool,
    /// Connected to a Havn account.
    pub connected: bool,
    /// Connected to the owner's: removing it does not shut them out.
    pub yours: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct InviteView {
    pub id: String,
    pub status: &'static str,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_by: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct RefusedView {
    pub sender_id: String,
    pub first_at: chrono::DateTime<chrono::Utc>,
    pub last_at: chrono::DateTime<chrono::Utc>,
    pub count: i64,
    pub said: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ChannelAccessView {
    pub accounts: Vec<AccountAccess>,
    /// Whether the owner is told in their chat when someone not paired
    /// writes (`config.notify_unpaired`, on unless false).
    pub notify_unpaired: bool,
}

/// The channel accounts whose messages reach `agent`: those it is bound
/// to first.
fn reaching<'a>(
    state: &'a AppState,
    agent: &havn_core::AgentId,
) -> Vec<(&'a str, &'a crate::config::ChannelAccount)> {
    let agent_id = agent.to_string();
    state
        .bindings
        .iter()
        .filter(|b| {
            b.agent_id == agent_id
                && channel_router::agent_for_account(&state.bindings, &b.channel, &b.account_id)
                    == Some(agent_id.as_str())
        })
        .filter_map(|b| {
            let account = state
                .channels
                .get(&b.channel)?
                .accounts
                .iter()
                .find(|a| a.id == b.account_id)?;
            Some((b.channel.as_str(), account))
        })
        .collect()
}

/// The account `account_id` of `channel` when its messages reach the
/// agent: else `NotFound`, so no one manages another agent's bot.
fn reaching_account<'a>(
    state: &'a AppState,
    agent: &havn_core::AgentId,
    channel: &str,
    account_id: &str,
) -> Result<&'a crate::config::ChannelAccount, crate::api::ApiError> {
    reaching(state, agent)
        .into_iter()
        .find(|(c, a)| *c == channel && a.id == account_id)
        .map(|(_, a)| a)
        .ok_or(crate::api::ApiError::NotFound)
}

async fn channel_access_view(
    state: &AppState,
    agent: &agent_repo::Agent,
) -> Result<ChannelAccessView, crate::api::ApiError> {
    let internal = |e: havn_db::DbError| crate::api::ApiError::Internal(e.to_string());
    let now = chrono::Utc::now();
    let mut accounts = Vec::new();
    for (channel, account) in reaching(state, &agent.id) {
        let invites = channel_access::list_invites(&state.db, Some(channel), Some(&account.id))
            .await
            .map_err(internal)?
            .into_iter()
            // Connect codes are their users' own.
            .filter(|i| i.user_id.is_none())
            .map(|i| InviteView {
                id: i.id.to_string(),
                status: i.status(now).as_str(),
                created_at: i.created_at,
                expires_at: i.expires_at,
                used_by: i.used_by_sender,
            })
            .collect();
        let refused = channel_access::list_refusals(&state.db, channel, &account.id)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|r| RefusedView {
                sender_id: r.sender_id,
                first_at: r.first_at,
                last_at: r.last_at,
                count: r.count,
                said: r.said,
            })
            .collect();
        let mut allowed = Vec::new();
        for a in channel_access::list_allowed_on(&state.db, channel, &account.id)
            .await
            .map_err(internal)?
        {
            let link = channel_access::linked_user(&state.db, channel, &a.sender_id)
                .await
                .map_err(internal)?;
            allowed.push(AllowedView {
                yours: link.as_ref().is_some_and(|l| l.user_id == agent.owner_id),
                connected: link.is_some(),
                sender_id: a.sender_id,
                since: a.registered_at,
                by_code: a.invite_id.is_some(),
            });
        }
        accounts.push(AccountAccess {
            channel: channel.to_string(),
            account_id: account.id.clone(),
            dm_policy: match account.dm_policy {
                DmPolicy::Allowlist => "allowlist",
                DmPolicy::Open => "open",
            },
            allow_from: account.allow_from.clone(),
            allowed,
            invites,
            refused,
        });
    }
    Ok(ChannelAccessView {
        accounts,
        notify_unpaired: agent
            .config
            .get("notify_unpaired")
            .and_then(serde_json::Value::as_bool)
            != Some(false),
    })
}

/// `GET /agents/{id}/channel-access`: who can reach the agent's bots
/// (issue #231). Owner only.
pub async fn channel_access_get(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::Json<ChannelAccessView>, crate::api::ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    Ok(axum::Json(channel_access_view(&state, &agent).await?))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteCreate {
    pub channel: String,
    pub account_id: String,
    /// Hours it lasts: 24 by default, at most 30 days.
    #[serde(default)]
    pub hours: Option<i64>,
}

/// The longest an invite made on the dashboard lasts, in hours.
const MAX_INVITE_HOURS: i64 = 30 * 24;

/// `POST /agents/{id}/channel-access/invites`: an invite code for one of
/// the agent's bots, shown once.
pub async fn channel_invite_create(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(req): axum::Json<InviteCreate>,
) -> Result<axum::Json<PairingCode>, crate::api::ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let account = reaching_account(&state, &agent.id, &req.channel, &req.account_id)?;
    let hours = req.hours.unwrap_or(24);
    if !(1..=MAX_INVITE_HOURS).contains(&hours) {
        return Err(crate::api::ApiError::BadRequest(format!(
            "hours must be 1 to {MAX_INVITE_HOURS}"
        )));
    }
    let made = channel_access::create_invite(
        &state.db,
        &req.channel,
        &account.id,
        chrono::Duration::hours(hours),
    )
    .await
    .map_err(|e| crate::api::ApiError::Internal(e.to_string()))?;
    let link = account
        .bot_username
        .as_deref()
        .map(|u| u.trim_start_matches('@'))
        .filter(|u| req.channel == "telegram" && !u.is_empty())
        .map(|u| format!("https://t.me/{u}?start={}", made.code));
    crate::audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "channel_invite.created",
        serde_json::json!({
            "channel": req.channel,
            "account_id": account.id,
            "invite": made.invite.id.to_string(),
            "expires_at": made.invite.expires_at,
        }),
    )
    .await;
    Ok(axum::Json(PairingCode {
        channel: req.channel,
        account_id: account.id.clone(),
        code: made.code,
        link,
        expires_at: made.invite.expires_at,
    }))
}

/// `DELETE /agents/{id}/channel-access/invites/{invite}`: revoke an
/// unused invite of one of the agent's bots.
pub async fn channel_invite_revoke(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Path((id, invite)): axum::extract::Path<(String, String)>,
) -> Result<axum::Json<ChannelAccessView>, crate::api::ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let invite_id = uuid::Uuid::parse_str(&invite).map_err(|_| crate::api::ApiError::NotFound)?;
    let found = channel_access::find_invite(&state.db, invite_id)
        .await
        .map_err(|e| crate::api::ApiError::Internal(e.to_string()))?
        .filter(|i| i.user_id.is_none())
        .ok_or(crate::api::ApiError::NotFound)?;
    reaching_account(&state, &agent.id, &found.channel, &found.account_id)?;
    match channel_access::revoke_invite(&state.db, invite_id).await {
        Ok(()) => {}
        Err(havn_db::DbError::Conflict(_)) => {
            return Err(crate::api::ApiError::BadRequest(
                "the invite was used: remove the sender it let in instead".into(),
            ));
        }
        Err(e) => return Err(crate::api::ApiError::Internal(e.to_string())),
    }
    crate::audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "channel_invite.revoked",
        serde_json::json!({ "invite": invite_id.to_string() }),
    )
    .await;
    Ok(axum::Json(channel_access_view(&state, &agent).await?))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SenderChange {
    pub channel: String,
    pub account_id: String,
    pub sender_id: String,
}

/// `POST /agents/{id}/channel-access/senders`: let a sender talk to one of
/// the agent's bots (issue #231).
pub async fn channel_sender_allow(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(req): axum::Json<SenderChange>,
) -> Result<axum::Json<ChannelAccessView>, crate::api::ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    reaching_account(&state, &agent.id, &req.channel, &req.account_id)?;
    let sender = req.sender_id.trim();
    if sender.is_empty()
        || sender.len() > 256
        || sender.chars().any(char::is_whitespace)
        || channel_id_codec::validate_channel_id_part("sender_id", sender).is_err()
    {
        return Err(crate::api::ApiError::BadRequest("not a sender id".into()));
    }
    admit_sender(
        &state,
        &agent,
        &req.channel,
        &req.account_id,
        sender,
        "dashboard",
    )
    .await
    .map_err(|e| crate::api::ApiError::Internal(e.to_string()))?;
    Ok(axum::Json(channel_access_view(&state, &agent).await?))
}

/// `DELETE /agents/{id}/channel-access/senders`: take back a sender's
/// access to one of the agent's bots; their next message is turned away.
pub async fn channel_sender_remove(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(req): axum::Json<SenderChange>,
) -> Result<axum::Json<ChannelAccessView>, crate::api::ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    reaching_account(&state, &agent.id, &req.channel, &req.account_id)?;
    match channel_access::revoke_sender(&state.db, &req.channel, &req.account_id, &req.sender_id)
        .await
    {
        Ok(()) => {}
        Err(havn_db::DbError::NotFound) => return Err(crate::api::ApiError::NotFound),
        Err(e) => return Err(crate::api::ApiError::Internal(e.to_string())),
    }
    crate::audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "channel_access.revoked",
        serde_json::json!({
            "channel": req.channel,
            "account_id": req.account_id,
            "sender_id": req.sender_id,
        }),
    )
    .await;
    Ok(axum::Json(channel_access_view(&state, &agent).await?))
}

/// `DELETE /agents/{id}/channel-access/refused`: forget a sender the bot
/// turned away.
pub async fn channel_refusal_forget(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(req): axum::Json<SenderChange>,
) -> Result<axum::Json<ChannelAccessView>, crate::api::ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    reaching_account(&state, &agent.id, &req.channel, &req.account_id)?;
    channel_access::forget_refusal(&state.db, &req.channel, &req.account_id, &req.sender_id)
        .await
        .map_err(|e| crate::api::ApiError::Internal(e.to_string()))?;
    Ok(axum::Json(channel_access_view(&state, &agent).await?))
}

#[derive(Debug, serde::Serialize)]
pub struct LinkView {
    pub channel: String,
    pub sender_id: String,
    pub linked_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /me/channel-links`: the caller's linked channel accounts.
pub async fn links_list(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
) -> Result<axum::Json<Vec<LinkView>>, crate::api::ApiError> {
    let links = channel_access::list_links(&state.db, Some(user.id))
        .await
        .map_err(|e| crate::api::ApiError::Internal(e.to_string()))?;
    Ok(axum::Json(
        links
            .into_iter()
            .map(|l| LinkView {
                channel: l.channel,
                sender_id: l.sender_id,
                linked_at: l.linked_at,
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct LinkQuery {
    pub channel: String,
    pub sender: String,
}

/// `DELETE /me/channel-links?channel=&sender=`: remove one of the caller's
/// links. The sender stays admitted where they were.
pub async fn links_remove(
    State(state): State<AppState>,
    user: crate::auth::AuthedUser,
    axum::extract::Query(q): axum::extract::Query<LinkQuery>,
) -> Result<axum::http::StatusCode, crate::api::ApiError> {
    let removed = channel_access::unlink(&state.db, user.id, &q.channel, &q.sender)
        .await
        .map_err(|e| crate::api::ApiError::Internal(e.to_string()))?;
    if !removed {
        return Err(crate::api::ApiError::NotFound);
    }
    crate::audit::record_user_action(
        &state.db,
        user.id,
        "channel_link.removed",
        serde_json::json!({ "channel": q.channel, "sender_id": q.sender }),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
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
    fn a_flood_of_new_senders_is_bounded_per_account() {
        let limiter = DmRejectLimiter::new();
        let start = Instant::now();
        for i in 0..DM_REPLY_MAX_KEYS {
            assert!(limiter.should_send_at(&format!("telegram|bot|flood:{i}"), start));
        }
        assert!(!limiter.should_send_at("telegram|bot|tg:new", start));
        assert!(
            limiter.should_send_at("telegram|other-bot|tg:new", start),
            "a flood at one bot does not silence another"
        );
        assert!(limiter.should_send_at("telegram|bot|tg:new", start + DM_REPLY_WINDOW));
    }

    #[test]
    fn bot_commands_are_read_with_or_without_the_bot_name() {
        assert_eq!(bot_command("/start"), Some(("start".to_string(), "")));
        assert_eq!(
            bot_command(" /start@acme_bot  7K3M-QX9P "),
            Some(("start".to_string(), "7K3M-QX9P"))
        );
        assert_eq!(
            bot_command("/HELP@acme_bot"),
            Some(("help".to_string(), ""))
        );
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
            /// The bound agent's owner.
            owner: havn_core::UserId,
        }

        async fn fixture(dm_policy: DmPolicy, allow_from: &[&str]) -> Fixture {
            let (mut state, owner) = crate::auth::tests::state_with(false).await;
            let agent_id = agent_repo::create(
                &state.db,
                agent_repo::NewAgent {
                    owner_id: owner,
                    team_id: None,
                    name: "bound",
                    config: serde_json::json!({}),
                },
            )
            .await
            .expect("agent")
            .id;
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
                            bot_username: None,
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
                owner,
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
                transcript: None,
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
                transcript: None,
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
                vec![(
                    "tg:1".to_string(),
                    GATEWAY_RESTARTING_MESSAGE[0].to_string()
                )]
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
        async fn start_codes_and_bare_codes_share_one_budget() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            for _ in 0..INVITE_ATTEMPT_LIMIT {
                f.send("tg:1", "/start AAAA-AAAA-AAAA-AAAA").await;
            }
            f.replies();
            // Its answers for this minute are used up; the attempts are
            // what is counted here.
            f.state.dm_reject_limiter = DmRejectLimiter::new();
            let code = f.invite("bot").await;
            f.send("tg:1", &code).await;
            assert!(!f.allowed("tg:1").await);
            assert_eq!(
                f.replies(),
                vec![("tg:1".to_string(), PAIR_TOO_MANY_TRIES.to_string())],
                "a valid code past the tries is told to wait, not that it is invalid"
            );
        }

        #[tokio::test]
        async fn a_link_code_may_be_lower_case_with_dashes() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = f.invite("bot").await;
            f.send("tg:1", &format!("/Start@bot {}", code.to_lowercase()))
                .await;
            assert!(f.allowed("tg:1").await);
            f.replies();
            f.send("tg:1", "/help").await;
            let replies = f.replies();
            assert!(
                replies.iter().any(|(_, t)| t.contains("/new"))
                    && !replies.iter().any(|(_, t)| t == PAIR_HELP),
                "a paired /help lists the commands, not how to pair: {replies:?}"
            );
        }

        #[tokio::test]
        async fn open_accounts_send_start_to_the_agent() {
            let mut f = fixture(DmPolicy::Open, &[]).await;
            f.send("tg:1", "/start").await;
            assert_eq!(f.forwarded(), vec!["tg:1".to_string()]);
            assert!(f.replies().is_empty());
        }

        async fn link_code(f: &Fixture, account: &str) -> String {
            channel_access::create_link_code(&f.state.db, "telegram", account, f.owner)
                .await
                .expect("code")
                .code
        }

        #[tokio::test]
        async fn the_owner_is_told_of_a_stranger_once_and_lets_them_in_with_the_button() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:owner", &format!("/start {code}")).await;
            f.replies();

            f.send("tg:stranger", "hi, it is Bob from the shop").await;
            let mut notices = Vec::new();
            let mut to_stranger = 0;
            while let Ok(frame) = f.adapter.try_recv() {
                if let ChannelFrame::Outbound(o) = frame {
                    if o.to == "tg:owner" {
                        notices.push(o);
                    } else if o.to == "tg:stranger" {
                        to_stranger += 1;
                    }
                }
            }
            assert_eq!(to_stranger, 1, "the stranger is told they are not paired");
            assert_eq!(notices.len(), 1, "{notices:?}");
            let ChannelMessageContent::Text { text } = &notices[0].content else {
                panic!("{:?}", notices[0].content);
            };
            assert!(
                text.contains("tg:stranger") && text.contains("Bob from the shop"),
                "{text}"
            );
            assert_eq!(notices[0].buttons[0][0].data, "/allow tg:stranger");
            assert!(f.forwarded().is_empty());

            f.send("tg:stranger", "hello?").await;
            assert!(
                !f.replies().iter().any(|(to, _)| to == "tg:owner"),
                "told once a day"
            );
            let refused = channel_access::list_refusals(&f.state.db, "telegram", "bot")
                .await
                .expect("refused");
            assert_eq!(refused.len(), 1);
            assert_eq!(refused[0].count, 2);

            // Someone else's /allow is not the owner's.
            f.send("tg:stranger", "/allow tg:stranger").await;
            assert!(!f.allowed("tg:stranger").await);

            f.send("tg:owner", "/allow tg:stranger").await;
            assert!(f.allowed("tg:stranger").await);
            let replies = f.replies();
            assert!(
                replies
                    .iter()
                    .any(|(to, t)| to == "tg:owner" && t.contains("can talk to this bot now")),
                "{replies:?}"
            );
            assert!(
                replies.iter().any(|(to, _)| to == "tg:stranger"),
                "they are told: {replies:?}"
            );
            assert!(
                channel_access::list_refusals(&f.state.db, "telegram", "bot")
                    .await
                    .expect("refused")
                    .is_empty()
            );
            f.forwarded();
            f.send("tg:stranger", "thanks").await;
            assert_eq!(f.forwarded(), vec!["tg:stranger".to_string()]);
        }

        #[tokio::test]
        async fn the_notice_is_plain_text_and_keeps_no_code() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:owner", &format!("/start {code}")).await;
            f.replies();
            f.send(
                "tg:stranger",
                "[Sign in again](https://evil.example)\n# urgent my code ABCD-EFGH-JKMN-PQRS",
            )
            .await;
            let notice = f
                .replies()
                .into_iter()
                .find(|(to, _)| to == "tg:owner")
                .map(|(_, text)| text)
                .expect("notice");
            assert!(
                notice.contains(r"\[Sign in again\]\(https\:\/\/evil\.example\) \# urgent"),
                "{notice}"
            );
            assert!(
                !notice.contains("PQRS") && notice.contains(r"\(a code\)"),
                "{notice}"
            );
            let refused = channel_access::list_refusals(&f.state.db, "telegram", "bot")
                .await
                .expect("refused");
            assert!(
                !refused[0].said.contains("PQRS") && refused[0].said.contains("(a code)"),
                "{}",
                refused[0].said
            );
        }

        #[tokio::test]
        async fn someone_let_in_is_never_listed_as_turned_away_nor_lets_others_in() {
            let mut f = fixture(DmPolicy::Allowlist, &["tg:friend"]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:owner", &format!("/start {code}")).await;
            f.replies();
            // A code that reads as one but is no one's.
            f.send("tg:friend", "/start ABCD-EFGH-JKMN-PQRS").await;
            f.send("tg:friend", "/allow tg:stranger").await;
            assert!(
                !f.allowed("tg:stranger").await,
                "only the owner lets people in"
            );
            let replies = f.replies();
            assert!(
                !replies.iter().any(|(to, _)| to == "tg:owner"),
                "{replies:?}"
            );
            assert!(
                replies
                    .iter()
                    .any(|(to, t)| to == "tg:friend" && t.contains("Only the agent's owner")),
                "{replies:?}"
            );
            assert!(
                channel_access::list_refusals(&f.state.db, "telegram", "bot")
                    .await
                    .expect("refused")
                    .is_empty()
            );
        }

        #[tokio::test]
        async fn the_access_page_manages_only_this_agent_s_bots() {
            use crate::api::ApiError;
            use axum::extract::{Json, Path};
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let agent = agent_repo::list_for_owner(&f.state.db, f.owner)
                .await
                .expect("agents")
                .remove(0);
            let (owner, state) = (f.owner, f.state.clone());
            let user = || crate::auth::AuthedUser {
                id: owner,
                display_name: "owner".into(),
            };
            let id = || Path(agent.id.to_string());
            let who = |account: &str, sender: &str| SenderChange {
                channel: "telegram".into(),
                account_id: account.into(),
                sender_id: sender.into(),
            };
            let st = || State(state.clone());

            let view = channel_access_get(st(), user(), id())
                .await
                .expect("view")
                .0;
            assert_eq!(view.accounts.len(), 1, "other-bot is bound to no agent");
            assert!(view.notify_unpaired);

            for (account, sender, bad) in
                [("other-bot", "tg:1", "not found"), ("bot", "tg 1", "bad")]
            {
                let err = channel_sender_allow(st(), user(), id(), Json(who(account, sender)))
                    .await
                    .expect_err(bad);
                assert!(
                    matches!(
                        (&err, bad),
                        (ApiError::NotFound, "not found") | (ApiError::BadRequest(_), "bad")
                    ),
                    "{err:?}"
                );
            }
            let view = channel_sender_allow(st(), user(), id(), Json(who("bot", "tg:9")))
                .await
                .expect("allow")
                .0;
            assert!(
                view.accounts[0]
                    .allowed
                    .iter()
                    .any(|a| a.sender_id == "tg:9" && !a.by_code)
            );
            assert!(matches!(
                channel_sender_remove(st(), user(), id(), Json(who("bot", "tg:nobody"))).await,
                Err(ApiError::NotFound)
            ));
            assert!(
                channel_sender_remove(st(), user(), id(), Json(who("bot", "tg:9")))
                    .await
                    .is_ok()
            );

            // A connect code is its user's: not listed, not revoked here.
            let connect = channel_access::create_link_code(&f.state.db, "telegram", "bot", f.owner)
                .await
                .expect("code");
            assert!(matches!(
                channel_invite_revoke(
                    st(),
                    user(),
                    Path((agent.id.to_string(), connect.invite.id.to_string()))
                )
                .await,
                Err(ApiError::NotFound)
            ));
            let made = channel_invite_create(
                st(),
                user(),
                id(),
                Json(InviteCreate {
                    channel: "telegram".into(),
                    account_id: "bot".into(),
                    hours: Some(1),
                }),
            )
            .await
            .expect("invite")
            .0;
            f.send("tg:invited", &format!("/start {}", made.code)).await;
            let used = channel_access_get(st(), user(), id())
                .await
                .expect("view")
                .0
                .accounts[0]
                .invites
                .iter()
                .find(|i| i.status == "used")
                .map(|i| i.id.clone())
                .expect("used");
            assert!(matches!(
                channel_invite_revoke(st(), user(), Path((agent.id.to_string(), used))).await,
                Err(ApiError::BadRequest(_))
            ));
            assert!(
                channel_invite_create(
                    st(),
                    user(),
                    id(),
                    Json(InviteCreate {
                        channel: "telegram".into(),
                        account_id: "other-bot".into(),
                        hours: None,
                    }),
                )
                .await
                .is_err()
            );

            f.send("tg:stranger", "hi").await;
            let view = channel_refusal_forget(st(), user(), id(), Json(who("bot", "tg:stranger")))
                .await
                .expect("forget")
                .0;
            assert!(view.accounts[0].refused.is_empty());
        }

        #[tokio::test]
        async fn no_notice_when_the_owner_turned_them_off() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:owner", &format!("/start {code}")).await;
            f.replies();
            let agent = agent_repo::list_for_owner(&f.state.db, f.owner)
                .await
                .expect("agents")
                .remove(0);
            agent_repo::update_config(&f.state.db, agent.id, None, |_| {
                Ok::<_, ()>(serde_json::json!({"notify_unpaired": false}))
            })
            .await
            .expect("config")
            .expect("changed");
            f.send("tg:stranger", "hi").await;
            assert!(!f.replies().iter().any(|(to, _)| to == "tg:owner"));
            assert_eq!(
                channel_access::list_refusals(&f.state.db, "telegram", "bot")
                    .await
                    .expect("refused")
                    .len(),
                1,
                "still kept for the dashboard"
            );
        }

        #[tokio::test]
        async fn a_web_code_links_the_sender_and_says_so() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:1", &format!("/start {code}")).await;
            assert!(f.allowed("tg:1").await);
            let link = channel_access::linked_user(&f.state.db, "telegram", "tg:1")
                .await
                .expect("db")
                .expect("linked");
            assert_eq!(link.user_id, f.owner);
            let replies = f.replies();
            assert_eq!(replies.len(), 1);
            assert!(
                replies[0].1.starts_with("Connected to Havn account ")
                    && replies[0].1.contains(&link.user_name),
                "{replies:?}"
            );
            assert!(f.forwarded().is_empty());
        }

        #[tokio::test]
        async fn an_admitted_sender_links_with_a_pairing_link_only() {
            let mut f = fixture(DmPolicy::Allowlist, &["tg:toml"]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:toml", &format!("/start@bot {code}")).await;
            assert!(f.forwarded().is_empty(), "the link is not the agent's");
            assert!(
                channel_access::linked_user(&f.state.db, "telegram", "tg:toml")
                    .await
                    .expect("db")
                    .is_some()
            );
            f.replies();
            f.send("tg:toml", "/start").await;
            f.send("tg:toml", "/start tell me a joke").await;
            assert_eq!(f.forwarded().len(), 2, "anything else is the agent's");
        }

        #[tokio::test]
        async fn the_owner_linked_through_another_bot_may_write_and_is_an_admin() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            // Linked through the other bot: not admitted to this one.
            let code = link_code(&f, "other-bot").await;
            channel_access::redeem(
                &f.state.db,
                "telegram",
                "other-bot",
                "tg:5",
                &code,
                chrono::Utc::now(),
                false,
            )
            .await
            .expect("redeem")
            .expect("redeemed");
            assert!(!f.allowed("tg:5").await);
            f.send("tg:5", "hello").await;
            assert_eq!(
                f.forwarded(),
                vec!["tg:5".to_string()],
                "the owner's own bot answers them"
            );
            f.send("tg:5", "/usage").await;
            let replies = f.replies();
            assert!(
                !replies.iter().any(|(_, t)| t.starts_with("(Only ")),
                "the owner is an admin: {replies:?}"
            );
            // Someone else's link does not count.
            let stranger = havn_db::repo::users::create(
                &f.state.db,
                havn_db::repo::users::NewUser {
                    display_name: "Stranger",
                },
            )
            .await
            .expect("user")
            .id;
            let theirs =
                channel_access::create_link_code(&f.state.db, "telegram", "other-bot", stranger)
                    .await
                    .expect("code")
                    .code;
            channel_access::redeem(
                &f.state.db,
                "telegram",
                "other-bot",
                "tg:6",
                &theirs,
                chrono::Utc::now(),
                false,
            )
            .await
            .expect("redeem");
            f.send("tg:6", "hello").await;
            assert!(f.forwarded().is_empty());
        }

        #[tokio::test]
        async fn a_web_code_for_another_bot_does_not_link_here() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = link_code(&f, "other-bot").await;
            f.send("tg:1", &format!("/start {code}")).await;
            assert!(!f.allowed("tg:1").await);
            assert_eq!(
                channel_access::linked_user(&f.state.db, "telegram", "tg:1")
                    .await
                    .expect("db"),
                None
            );
            assert_eq!(
                f.replies(),
                vec![("tg:1".to_string(), PAIR_INVALID_CODE.to_string())]
            );
        }

        fn as_user(id: havn_core::UserId) -> crate::auth::AuthedUser {
            crate::auth::AuthedUser {
                id,
                display_name: "someone".into(),
            }
        }

        #[tokio::test]
        async fn codes_are_made_only_by_the_owner_for_accounts_that_reach_the_agent() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let agent = f.state.bindings[0].agent_id.clone();
            let made = pairing_create(
                State(f.state.clone()),
                as_user(f.owner),
                axum::extract::Path(agent.clone()),
            )
            .await
            .expect("codes")
            .0;
            assert_eq!(made.len(), 1);
            assert_eq!(made[0].account_id, "bot");
            let stranger = havn_db::repo::users::create(
                &f.state.db,
                havn_db::repo::users::NewUser {
                    display_name: "Stranger",
                },
            )
            .await
            .expect("user")
            .id;
            let refused = pairing_create(
                State(f.state.clone()),
                as_user(stranger),
                axum::extract::Path(agent.clone()),
            )
            .await;
            assert!(matches!(refused, Err(crate::api::ApiError::NotFound)));
            // The stranger's own agent, bound second to the same account:
            // its messages go to the first agent, so no code for it.
            let theirs = agent_repo::create(
                &f.state.db,
                agent_repo::NewAgent {
                    owner_id: stranger,
                    team_id: None,
                    name: "second",
                    config: serde_json::json!({}),
                },
            )
            .await
            .expect("agent")
            .id;
            let mut bindings = (*f.state.bindings).clone();
            bindings.push(ChannelBindingConfig {
                agent_id: theirs.to_string(),
                channel: "telegram".into(),
                account_id: "bot".into(),
            });
            f.state.bindings = Arc::new(bindings);
            let unbound = pairing_create(
                State(f.state.clone()),
                as_user(stranger),
                axum::extract::Path(theirs.to_string()),
            )
            .await;
            assert!(matches!(unbound, Err(crate::api::ApiError::BadRequest(_))));
            // Removing someone else's link is not found.
            let code = link_code(&f, "bot").await;
            f.send("tg:1", &format!("/start {code}")).await;
            let removed = links_remove(
                State(f.state.clone()),
                as_user(stranger),
                axum::extract::Query(LinkQuery {
                    channel: "telegram".into(),
                    sender: "tg:1".into(),
                }),
            )
            .await;
            assert!(matches!(removed, Err(crate::api::ApiError::NotFound)));
        }

        #[tokio::test]
        async fn disconnecting_takes_back_the_access_the_link_gave() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:1", &format!("/start {code}")).await;
            f.send("tg:1", "hello").await;
            assert_eq!(f.forwarded(), vec!["tg:1".to_string()]);
            links_remove(
                State(f.state.clone()),
                as_user(f.owner),
                axum::extract::Query(LinkQuery {
                    channel: "telegram".into(),
                    sender: "tg:1".into(),
                }),
            )
            .await
            .expect("removed");
            f.send("tg:1", "hello again").await;
            assert!(f.forwarded().is_empty());
        }

        #[tokio::test]
        async fn a_sender_linked_to_someone_else_is_not_taken_over() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let code = link_code(&f, "bot").await;
            f.send("tg:1", &format!("/start {code}")).await;
            f.replies();
            let mallory = havn_db::repo::users::create(
                &f.state.db,
                havn_db::repo::users::NewUser {
                    display_name: "Mallory",
                },
            )
            .await
            .expect("user")
            .id;
            let theirs = channel_access::create_link_code(&f.state.db, "telegram", "bot", mallory)
                .await
                .expect("code")
                .code;
            f.send("tg:1", &format!("/start {theirs}")).await;
            assert_eq!(
                f.replies(),
                vec![("tg:1".to_string(), PAIR_LINKED_ELSEWHERE.to_string())]
            );
            let link = channel_access::linked_user(&f.state.db, "telegram", "tg:1")
                .await
                .expect("db")
                .expect("linked");
            assert_eq!(link.user_id, f.owner);
        }

        #[tokio::test]
        async fn the_person_can_disconnect_a_link_made_to_someone_else() {
            let mut f = fixture(DmPolicy::Allowlist, &[]).await;
            let mallory = havn_db::repo::users::create(
                &f.state.db,
                havn_db::repo::users::NewUser {
                    display_name: "Mallory",
                },
            )
            .await
            .expect("user")
            .id;
            let theirs = channel_access::create_link_code(&f.state.db, "telegram", "bot", mallory)
                .await
                .expect("code")
                .code;
            f.send("tg:1", &format!("/start {theirs}")).await;
            f.replies();
            f.send("tg:1", "/disconnect@bot").await;
            assert_eq!(
                f.replies(),
                vec![(
                    "tg:1".to_string(),
                    "Disconnected from Havn account Mallory.".to_string()
                )]
            );
            assert!(f.forwarded().is_empty());
            let code = link_code(&f, "bot").await;
            f.send("tg:1", &format!("/start {code}")).await;
            let link = channel_access::linked_user(&f.state.db, "telegram", "tg:1")
                .await
                .expect("db")
                .expect("linked");
            assert_eq!(link.user_id, f.owner, "their own account now");
            f.replies();
            f.send("tg:2", "/disconnect").await;
            assert_eq!(
                f.replies(),
                vec![("tg:2".to_string(), NOT_CONNECTED_MESSAGE.to_string())]
            );
        }

        #[tokio::test]
        async fn an_admitted_sender_does_not_use_up_an_invite() {
            let mut f = fixture(DmPolicy::Allowlist, &["tg:toml"]).await;
            let plain = f.invite("bot").await;
            f.send("tg:toml", &format!("/start {plain}")).await;
            assert!(f.forwarded().is_empty());
            f.replies();
            f.send("tg:new", &plain).await;
            assert!(
                f.allowed("tg:new").await,
                "the invite was still there for its person"
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
