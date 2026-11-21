//! Per-account router for the `/api/v1/channel` WS endpoint (spec §3).
//!
//! Each open external-channel WebSocket holds one entry keyed by
//! `(channel, account_id)`. When the gateway needs to deliver an
//! agent's outbound to a specific user via that adapter, it pushes a
//! `ChannelFrame::Outbound` onto the per-account mpsc; the WS
//! handler's outbound task drains the mpsc and writes to the socket.
//!
//! Conceptually parallel to [`crate::webchat::WebChatRouter`] but
//! keyed differently:
//! - `WebChatRouter`: HashMap<SessionId UUID, …> — one entry per open
//!   browser tab; webchat is loopback and the dashboard establishes
//!   one session per (user, agent) pair.
//! - `ChannelRouter`: HashMap<(channel, account_id), …> — one entry
//!   per adapter daemon, channel, account triple. An adapter handles
//!   ALL users of one bot identity, so the routing key is the bot
//!   identity (account), not a per-user session.
//!
//! Replay buffer (spec §3.2): per-account in-memory ring of the most
//! recent outbound frames within a sliding 5-minute window. Adapters
//! reconnect with `Resume{since_seq}`; the gateway replays buffered
//! frames whose seq > since_seq. The seq counter is monotonic per
//! account across reconnects so `since_seq` actually identifies a
//! point in the stream, and starts from the time an account is first seen
//! (in microseconds), so it is past every seq a gateway that ran before
//! gave: an adapter that kept its last seq across a gateway restart does
//! not take new frames for ones it had. Replies are kept for an account
//! before any adapter connected too (after a gateway restart, until the
//! adapter is back). Overflow (more than [`REPLAY_MAX_FRAMES`] in the
//! window) drops the oldest entry with a warn log; agents do not block on
//! adapter availability (spec §3.2).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use havn_proto::channel::{ChannelFrame, ChannelMessageContent, ChannelOutbound};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

use crate::config::{ChannelBindingConfig, ChannelEntry, DmPolicy};

/// Sliding window for the per-account replay buffer (spec §3.2).
const REPLAY_WINDOW: Duration = Duration::from_secs(300);

/// Hard cap on buffered frames per account, regardless of window.
/// Bounds memory under a sustained high-rate outbound from one agent
/// while the adapter is unable to keep up; oldest is dropped with a
/// warn log on overflow. 1024 frames is roughly two messages per
/// second for the full 5-minute window — well above conversational
/// rates and below any plausible memory concern.
const REPLAY_MAX_FRAMES: usize = 1024;

/// Composite key identifying one (channel, account) routing slot.
/// Two distinct adapters serving the same channel under different
/// account_ids hold separate entries; reconnects of the same adapter
/// with the same account_id replace the old entry (the `register`
/// contract).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountKey {
    pub channel: String,
    pub account_id: String,
}

impl AccountKey {
    #[must_use]
    pub fn new(channel: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            account_id: account_id.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChannelRouter {
    inner: Arc<Mutex<HashMap<AccountKey, AccountState>>>,
}

#[derive(Debug)]
struct AccountState {
    /// `Some` while an adapter daemon holds the WS; `None` between
    /// disconnect and reconnect. Replay-buffer state survives the gap.
    tx: Option<mpsc::Sender<ChannelFrame>>,
    /// Gateway-side monotonic seq for outbound frames to this account
    /// (spec §3.1 — adapters track the last seq they acked and use it
    /// in `Resume` after reconnect). Never resets — that's the whole
    /// point of `Resume{since_seq}`.
    next_seq: u64,
    /// Sliding 5-minute window of recently-delivered outbound frames
    /// for replay on reconnect. Oldest-first; aged + capacity-trimmed
    /// on every mutation.
    replay: VecDeque<ReplayEntry>,
    /// The sender a `Resume` replay is going through. While it is `tx`,
    /// deliveries are only buffered and the replay sends them when it
    /// catches up, so a long replay cannot fill the queue and look like a
    /// stall. A replay by a superseded connection never matches.
    replaying: Option<mpsc::Sender<ChannelFrame>>,
    /// Per group id, whether the bot sees every message there, as its
    /// adapter last said with a message from it.
    sees_all: HashMap<String, bool>,
}

#[derive(Debug, Clone)]
struct ReplayEntry {
    seq: u64,
    frame: ChannelFrame,
    inserted_at: Instant,
}

/// Groups whose `sees_all` an account keeps at most.
const GROUPS_KNOWN: usize = 1000;

/// The first seq of an account: microseconds since the epoch, past the
/// seqs of a gateway that ran before (unless it gave more than one frame
/// a microsecond, or the clock went back).
fn first_seq() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX / 2))
        .max(1)
}

impl AccountState {
    fn new(tx: Option<mpsc::Sender<ChannelFrame>>) -> Self {
        Self {
            tx,
            next_seq: first_seq(),
            replay: VecDeque::new(),
            replaying: None,
            sees_all: HashMap::new(),
        }
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeliverError {
    /// No adapter daemon is currently connected for this (channel,
    /// account) pair. The reply is kept in the replay buffer for its
    /// `Resume`; the caller (agent_socket
    /// outbound dispatch) should log. Agents do NOT block on adapter
    /// availability (spec §3.2).
    #[error("no adapter connected for {0:?}")]
    NoSession(AccountKey),
    /// The mpsc sender was dropped between lookup and send — the WS
    /// closed concurrently. Treated like NoSession by the caller.
    #[error("adapter writer closed")]
    WriterClosed,
    /// The adapter's outbound queue is full: it is not reading. Its
    /// sender is dropped, which closes its WS; the frame stays in the
    /// replay buffer so the reconnecting adapter's `Resume` gets it.
    #[error("adapter for {0:?} is not reading its queue; disconnected it")]
    Stalled(AccountKey),
}

impl ChannelRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note whether `key`'s bot sees every message in `group`, as its
    /// adapter says with a message from it: `None`, that it does not know
    /// (any more). At most `GROUPS_KNOWN` groups are kept per account.
    pub async fn note_sees_all(&self, key: AccountKey, group: &str, sees_all: Option<bool>) {
        let mut map = self.inner.lock().await;
        let known = &mut map
            .entry(key)
            .or_insert_with(|| AccountState::new(None))
            .sees_all;
        let Some(sees_all) = sees_all else {
            known.remove(group);
            return;
        };
        if known.len() >= GROUPS_KNOWN && !known.contains_key(group) {
            if let Some(any) = known.keys().next().cloned() {
                known.remove(&any);
            }
        }
        known.insert(group.to_string(), sees_all);
    }

    /// Whether `key`'s bot sees every message in `group`, as its adapter
    /// last said; `None` before it said (or since the gateway started).
    pub async fn sees_all(&self, key: &AccountKey, group: &str) -> Option<bool> {
        self.inner
            .lock()
            .await
            .get(key)
            .and_then(|s| s.sees_all.get(group).copied())
    }

    /// What `key`'s adapter last said of each of its groups.
    pub async fn groups_seeing(&self, key: &AccountKey) -> HashMap<String, bool> {
        self.inner
            .lock()
            .await
            .get(key)
            .map(|s| s.sees_all.clone())
            .unwrap_or_default()
    }

    /// Register a connected adapter's outbound mpsc. If an
    /// `AccountState` already exists for `key` (reconnect), the new
    /// `tx` replaces the old one and the seq counter + replay buffer
    /// are preserved — `Resume{since_seq}` from the adapter can then
    /// pull missed frames via [`Self::replay_since`].
    pub async fn register(&self, key: AccountKey, tx: mpsc::Sender<ChannelFrame>) {
        self.register_as(key, tx, false).await;
    }

    /// [`Self::register`] for an adapter that resumes as it connects: until
    /// its replay ([`Self::replay_since`], [`Self::replay_more`]) catches
    /// up, new deliveries are only buffered, so none reaches it before the
    /// ones it missed.
    pub async fn register_resuming(&self, key: AccountKey, tx: mpsc::Sender<ChannelFrame>) {
        self.register_as(key, tx, true).await;
    }

    async fn register_as(&self, key: AccountKey, tx: mpsc::Sender<ChannelFrame>, resuming: bool) {
        let mut map = self.inner.lock().await;
        let state = map.entry(key).or_insert_with(|| AccountState::new(None));
        state.replaying = resuming.then(|| tx.clone());
        state.tx = Some(tx);
    }

    /// Where a `Resume` from `since_seq` starts: there, unless it is a seq
    /// this gateway never gave (at or past its next one), from a gateway
    /// that ran before with its clock ahead or another instance: then from
    /// the start of what is kept.
    pub async fn resume_point(&self, key: &AccountKey, since_seq: u64) -> u64 {
        match self.inner.lock().await.get(key) {
            Some(state) if since_seq >= state.next_seq => 0,
            _ => since_seq,
        }
    }

    /// Mark the adapter holding `tx` as disconnected but keep the
    /// `AccountState` — the replay buffer and seq counter survive until
    /// the next reconnect (or until the gateway restarts). A newer
    /// connection that already replaced `tx` is left alone. Idempotent.
    pub async fn unregister(&self, key: &AccountKey, tx: &mpsc::Sender<ChannelFrame>) {
        if let Some(state) = self.inner.lock().await.get_mut(key)
            && state.tx.as_ref().is_some_and(|t| t.same_channel(tx))
        {
            state.tx = None;
            state.replaying = None;
        }
    }

    /// Deliver an agent reply through the adapter currently holding
    /// `(channel, account)`. Allocates the next gateway-side seq,
    /// constructs a `ChannelFrame::Outbound`, sends to the adapter's
    /// mpsc, and stores a copy in the replay buffer for possible
    /// resumption. Returns the assigned seq.
    pub async fn deliver(
        &self,
        channel: &str,
        account_id: &str,
        sender_id: &str,
        in_reply_to: Option<String>,
        content: ChannelMessageContent,
    ) -> Result<u64, DeliverError> {
        self.deliver_with(
            channel,
            account_id,
            sender_id,
            in_reply_to,
            content,
            Vec::new(),
            None,
            false,
        )
        .await
    }

    /// [`Self::deliver`], with rows of buttons under the message, or a poll
    /// in its place.
    #[allow(clippy::too_many_arguments)]
    pub async fn deliver_with(
        &self,
        channel: &str,
        account_id: &str,
        sender_id: &str,
        in_reply_to: Option<String>,
        content: ChannelMessageContent,
        buttons: Vec<Vec<havn_core::Button>>,
        poll: Option<havn_core::Poll>,
        partial: bool,
    ) -> Result<u64, DeliverError> {
        let key = AccountKey::new(channel, account_id);
        let (tx, frame, seq) = {
            let mut map = self.inner.lock().await;
            // Kept before any adapter connected too: after a gateway
            // restart, until the adapter is back to resume.
            let state = map
                .entry(key.clone())
                .or_insert_with(|| AccountState::new(None));
            let seq = state.next_seq;
            state.next_seq = state.next_seq.saturating_add(1);
            let frame = ChannelFrame::Outbound(ChannelOutbound {
                seq,
                account_id: account_id.to_string(),
                to: sender_id.to_string(),
                in_reply_to,
                content,
                buttons,
                poll,
                partial,
            });
            // Buffered even while the adapter is away, so its `Resume`
            // after reconnecting gets it.
            push_replay(&key, &mut state.replay, seq, frame.clone());
            match &state.tx {
                None => return Err(DeliverError::NoSession(key.clone())),
                // The running replay sends it once it catches up.
                Some(tx) if state.replaying.as_ref().is_some_and(|r| r.same_channel(tx)) => {
                    return Ok(seq);
                }
                Some(tx) => (tx.clone(), frame, seq),
            }
        };
        // Never wait on the adapter: it serves every agent bound to its
        // account, and the caller is an agent's reader loop.
        match tx.try_send(frame) {
            Ok(()) => Ok(seq),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(DeliverError::WriterClosed),
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    channel = %key.channel,
                    account = %key.account_id,
                    seq,
                    "channel adapter is not reading; disconnecting it so it resumes"
                );
                self.unregister(&key, &tx).await;
                Err(DeliverError::Stalled(key))
            }
        }
    }

    /// Collect buffered outbound frames with `seq > since_seq` for
    /// replay through the currently-registered adapter (spec §3.2).
    /// Returns frames in original seq order. Frames older than
    /// [`REPLAY_WINDOW`] have already been evicted on every prior
    /// mutation, so the returned slice is the live replay set.
    ///
    /// If no `AccountState` exists for `key`, returns an empty Vec:
    /// nothing was ever delivered for it.
    ///
    /// Starts a replay through `tx` when it is the registered sender:
    /// until [`Self::replay_more`] reports it caught up, new deliveries
    /// are only buffered.
    pub async fn replay_since(
        &self,
        key: &AccountKey,
        since_seq: u64,
        tx: &mpsc::Sender<ChannelFrame>,
    ) -> Vec<ChannelFrame> {
        let mut map = self.inner.lock().await;
        let Some(state) = map.get_mut(key) else {
            return Vec::new();
        };
        if state.tx.as_ref().is_some_and(|t| t.same_channel(tx)) {
            state.replaying = Some(tx.clone());
        }
        frames_after(state, since_seq)
    }

    /// Frames buffered after `after_seq` while the replay through `tx`
    /// was being sent. Empty means it caught up: it ends, and deliveries
    /// go straight to the adapter again.
    pub async fn replay_more(
        &self,
        key: &AccountKey,
        after_seq: u64,
        tx: &mpsc::Sender<ChannelFrame>,
    ) -> Vec<ChannelFrame> {
        let mut map = self.inner.lock().await;
        let Some(state) = map.get_mut(key) else {
            return Vec::new();
        };
        let frames = frames_after(state, after_seq);
        if frames.is_empty() && state.replaying.as_ref().is_some_and(|r| r.same_channel(tx)) {
            state.replaying = None;
        }
        frames
    }
}

fn frames_after(state: &mut AccountState, seq: u64) -> Vec<ChannelFrame> {
    evict_aged(&mut state.replay);
    state
        .replay
        .iter()
        .filter(|e| e.seq > seq)
        .map(|e| e.frame.clone())
        .collect()
}

/// Append a frame to the replay buffer for an account. Trims aged
/// entries first, then enforces the hard frame cap by dropping oldest
/// with a warn log (operators see when sustained load is outpacing
/// adapter throughput).
fn push_replay(
    key: &AccountKey,
    replay: &mut VecDeque<ReplayEntry>,
    seq: u64,
    frame: ChannelFrame,
) {
    evict_aged(replay);
    while replay.len() >= REPLAY_MAX_FRAMES {
        if let Some(dropped) = replay.pop_front() {
            warn!(
                channel = %key.channel,
                account = %key.account_id,
                dropped_seq = dropped.seq,
                window_secs = REPLAY_WINDOW.as_secs(),
                cap = REPLAY_MAX_FRAMES,
                "channel replay buffer overflow — dropping oldest frame (spec §3.2)"
            );
        } else {
            break;
        }
    }
    replay.push_back(ReplayEntry {
        seq,
        frame,
        inserted_at: Instant::now(),
    });
}

fn evict_aged(replay: &mut VecDeque<ReplayEntry>) {
    let now = Instant::now();
    while let Some(front) = replay.front() {
        if now.duration_since(front.inserted_at) > REPLAY_WINDOW {
            replay.pop_front();
        } else {
            break;
        }
    }
}

/// Look up the agent_id bound to an incoming `(channel, account_id)`
/// pair. Returns `None` when there's no binding — the inbound is
/// dropped with a warn log; spec §3.5 implies the operator notices via
/// the `dangling_bindings` startup warnings, so the runtime path can
/// be silent-drop.
#[must_use]
pub fn agent_for_account<'a>(
    bindings: &'a [ChannelBindingConfig],
    channel: &str,
    account_id: &str,
) -> Option<&'a str> {
    bindings
        .iter()
        .find(|b| b.channel == channel && b.account_id == account_id)
        .map(|b| b.agent_id.as_str())
}

/// Why an agent may not send a channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundRefusal {
    NotBound,
    UnknownAccount,
    RecipientNotAllowed,
}

impl OutboundRefusal {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotBound => "agent is not bound to this channel account",
            Self::UnknownAccount => "channel account is not configured",
            Self::RecipientNotAllowed => {
                "recipient is not in the account's allow_from or invite allowlist"
            }
        }
    }
}

/// Decide whether `agent_id` may send through `(channel, account_id)` to
/// `recipient`. The agent must be the one `[[bindings]]` binds to the
/// account. Under `dm_policy = "allowlist"` the recipient must be in
/// `allow_from`; under `open` anyone on the platform can already talk to
/// the agent, so any recipient is allowed. `RecipientNotAllowed` only
/// covers the TOML list: the caller still checks the invite allowlist
/// in the DB (issue #25).
pub fn outbound_permitted(
    bindings: &[ChannelBindingConfig],
    channels: &std::collections::BTreeMap<String, ChannelEntry>,
    agent_id: havn_core::AgentId,
    channel: &str,
    account_id: &str,
    recipient: &str,
) -> Result<(), OutboundRefusal> {
    // Parse the configured id the same way inbound routing does, so
    // uppercase or otherwise non-canonical UUIDs in config still match.
    let bound = agent_for_account(bindings, channel, account_id)
        .and_then(|id| <havn_core::AgentId as std::str::FromStr>::from_str(id).ok());
    if bound != Some(agent_id) {
        return Err(OutboundRefusal::NotBound);
    }
    let account = channels
        .get(channel)
        .and_then(|entry| entry.accounts.iter().find(|a| a.id == account_id))
        .ok_or(OutboundRefusal::UnknownAccount)?;
    match account.dm_policy {
        DmPolicy::Open => Ok(()),
        DmPolicy::Allowlist if account.allow_from.iter().any(|id| id == recipient) => Ok(()),
        DmPolicy::Allowlist => Err(OutboundRefusal::RecipientNotAllowed),
    }
}

/// [`outbound_permitted`], or the recipient was admitted by invite code
/// (kept in the DB).
pub async fn send_permitted(
    db: &sqlx::SqlitePool,
    bindings: &[ChannelBindingConfig],
    channels: &std::collections::BTreeMap<String, ChannelEntry>,
    agent_id: havn_core::AgentId,
    channel: &str,
    account_id: &str,
    recipient: &str,
) -> Result<(), OutboundRefusal> {
    match outbound_permitted(bindings, channels, agent_id, channel, account_id, recipient) {
        Err(OutboundRefusal::RecipientNotAllowed) => {
            match havn_db::repo::channel_access::is_allowed(db, channel, account_id, recipient)
                .await
            {
                Ok(true) => Ok(()),
                Ok(false) => group_permitted(db, channels, channel, account_id, recipient).await,
                Err(e) => {
                    warn!(%agent_id, error = %e, "channel allowlist lookup failed");
                    Err(OutboundRefusal::RecipientNotAllowed)
                }
            }
        }
        other => other,
    }
}

/// Whether `recipient` is a group the account may be used in (issue
/// #263): in `allow_groups`, let in by the owner, or, while the account
/// is `open` to groups, one it was used in. An account whose groups are
/// `off` sends to none.
async fn group_permitted(
    db: &sqlx::SqlitePool,
    channels: &std::collections::BTreeMap<String, ChannelEntry>,
    channel: &str,
    account_id: &str,
    recipient: &str,
) -> Result<(), OutboundRefusal> {
    let Some(account) = channels
        .get(channel)
        .and_then(|entry| entry.accounts.iter().find(|a| a.id == account_id))
    else {
        return Err(OutboundRefusal::UnknownAccount);
    };
    let open = match account.group_policy {
        crate::config::GroupPolicy::Off => return Err(OutboundRefusal::RecipientNotAllowed),
        crate::config::GroupPolicy::Allowlist => false,
        crate::config::GroupPolicy::Open => true,
    };
    if account.allow_groups.iter().any(|g| g == recipient) {
        return Ok(());
    }
    match havn_db::repo::channel_access::group_allowed(db, channel, account_id, recipient, open)
        .await
    {
        Ok(true) => Ok(()),
        Ok(false) => Err(OutboundRefusal::RecipientNotAllowed),
        Err(e) => {
            warn!(error = %e, "group allowlist lookup failed");
            Err(OutboundRefusal::RecipientNotAllowed)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn binding(agent: &str, channel: &str, account: &str) -> ChannelBindingConfig {
        ChannelBindingConfig {
            agent_id: agent.into(),
            channel: channel.into(),
            account_id: account.into(),
        }
    }

    #[test]
    fn agent_for_account_finds_first_match() {
        let bs = vec![
            binding("a1", "telegram", "alice-tg-bot"),
            binding("a2", "slack", "team-bot"),
        ];
        assert_eq!(
            agent_for_account(&bs, "telegram", "alice-tg-bot"),
            Some("a1")
        );
        assert_eq!(agent_for_account(&bs, "slack", "team-bot"), Some("a2"));
    }

    #[test]
    fn agent_for_account_missing_pair_returns_none() {
        let bs = vec![binding("a1", "telegram", "alice-tg-bot")];
        assert!(agent_for_account(&bs, "telegram", "bob-tg-bot").is_none());
        assert!(agent_for_account(&bs, "slack", "alice-tg-bot").is_none());
    }

    #[tokio::test]
    async fn register_then_deliver_round_trips() {
        let router = ChannelRouter::new();
        let (tx, mut rx) = mpsc::channel(8);
        router
            .register(AccountKey::new("telegram", "alice-tg-bot"), tx)
            .await;

        let seq = router
            .deliver(
                "telegram",
                "alice-tg-bot",
                "tg:123",
                Some("tg:msg-7".into()),
                ChannelMessageContent::Text { text: "hi".into() },
            )
            .await
            .expect("deliver");
        assert!(seq > 1_000_000_000_000_000, "from the time: {seq}");

        let got = rx.recv().await.expect("frame received");
        match got {
            ChannelFrame::Outbound(o) => {
                assert_eq!(o.seq, seq);
                assert_eq!(o.account_id, "alice-tg-bot");
                assert_eq!(o.to, "tg:123");
                assert_eq!(o.in_reply_to.as_deref(), Some("tg:msg-7"));
            }
            other => panic!("expected Outbound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_adapter_resuming_as_it_connects_gets_nothing_ahead_of_its_replay() {
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let deliver = || {
            router.deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
        };
        let (old, _old_rx) = mpsc::channel(8);
        router.register(key.clone(), old.clone()).await;
        router.unregister(&key, &old).await;
        let missed = deliver().await;
        assert!(matches!(missed, Err(DeliverError::NoSession(_))));

        let (tx, mut rx) = mpsc::channel(8);
        router.register_resuming(key.clone(), tx.clone()).await;
        // A reply before the replay starts waits for it.
        deliver().await.expect("buffered for the replay");
        assert!(rx.try_recv().is_err(), "nothing ahead of the replay");
        let first = router.replay_since(&key, 0, &tx).await;
        assert_eq!(first.len(), 2);
        let last = seqs(&first)[1];
        assert!(router.replay_more(&key, last, &tx).await.is_empty());
        deliver().await.expect("live again");
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn a_seq_this_gateway_never_gave_resumes_from_the_start() {
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let kept = router
            .deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
            .await;
        assert!(matches!(kept, Err(DeliverError::NoSession(_))));
        assert_eq!(router.resume_point(&key, u64::MAX / 2).await, 0);
        assert_eq!(router.resume_point(&key, 3).await, 3);
        let unknown = AccountKey::new("telegram", "ghost");
        assert_eq!(router.resume_point(&unknown, 9).await, 9);
    }

    #[tokio::test]
    async fn a_reply_before_any_adapter_is_kept_for_its_resume() {
        let router = ChannelRouter::new();
        let r = router
            .deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
            .await;
        assert!(matches!(r, Err(DeliverError::NoSession(_))));
        // The adapter connects after a gateway restart and resumes from a
        // seq of the gateway that ran before, which is smaller.
        let key = AccountKey::new("telegram", "alice");
        let (tx, _rx) = mpsc::channel(8);
        router.register(key.clone(), tx.clone()).await;
        let kept = router.replay_since(&key, 5_000, &tx).await;
        assert_eq!(kept.len(), 1);
    }

    #[tokio::test]
    async fn deliver_to_a_full_adapter_queue_fails_at_once_and_disconnects_it() {
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let (tx, mut rx) = mpsc::channel(1);
        router.register(key.clone(), tx).await;
        let send = || {
            router.deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
        };
        send().await.expect("first fits");
        let r = tokio::time::timeout(Duration::from_secs(5), send())
            .await
            .expect("deliver must not wait for the adapter");
        assert!(matches!(r, Err(DeliverError::Stalled(_))), "{r:?}");
        assert_eq!(
            router.replay_since(&key, 0, &stranger()).await.len(),
            2,
            "the dropped frame stays replayable"
        );
        assert!(rx.recv().await.is_some(), "the queued frame");
        assert!(
            rx.recv().await.is_none(),
            "the router dropped its sender, so the adapter's WS closes"
        );
        assert!(matches!(send().await, Err(DeliverError::NoSession(_))));
        assert_eq!(
            router.replay_since(&key, 0, &stranger()).await.len(),
            3,
            "replies while the adapter is away are buffered too"
        );
    }

    fn stranger() -> mpsc::Sender<ChannelFrame> {
        mpsc::channel(1).0
    }

    #[tokio::test]
    async fn a_superseded_connection_cannot_pause_the_new_one() {
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let (old, _old_rx) = mpsc::channel(8);
        router.register(key.clone(), old.clone()).await;
        let (new, mut new_rx) = mpsc::channel(8);
        router.register(key.clone(), new).await;

        // The old session handles a late Resume after the new one registered.
        router.replay_since(&key, 0, &old).await;
        router
            .deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
            .await
            .expect("deliver");

        assert!(
            new_rx.try_recv().is_ok(),
            "the new adapter gets live frames"
        );
    }

    fn seqs(frames: &[ChannelFrame]) -> Vec<u64> {
        frames
            .iter()
            .filter_map(|f| match f {
                ChannelFrame::Outbound(o) => Some(o.seq),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn deliveries_during_a_replay_wait_for_it_instead_of_overflowing() {
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let deliver = || {
            router.deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
        };
        let (old, _old_rx) = mpsc::channel(8);
        router.register(key.clone(), old.clone()).await;
        router.unregister(&key, &old).await;
        for _ in 0..5 {
            assert!(matches!(deliver().await, Err(DeliverError::NoSession(_))));
        }

        // The reconnected adapter's queue is smaller than the replay.
        let (tx, mut rx) = mpsc::channel(1);
        router.register(key.clone(), tx.clone()).await;
        let first = router.replay_since(&key, 0, &tx).await;
        let base = seqs(&first)[0] - 1;
        let at = |n: u64| base + n;
        assert_eq!(seqs(&first), (1..=5).map(at).collect::<Vec<_>>());
        for _ in 0..3 {
            deliver()
                .await
                .expect("buffered for the replay, not refused");
        }
        assert!(rx.try_recv().is_err(), "nothing bypasses the replay");

        let more = router.replay_more(&key, at(5), &tx).await;
        assert_eq!(seqs(&more), (6..=8).map(at).collect::<Vec<_>>());
        assert!(router.replay_more(&key, at(8), &tx).await.is_empty());
        deliver().await.expect("live again");
        match rx.try_recv() {
            Ok(ChannelFrame::Outbound(o)) => assert_eq!(o.seq, at(9)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn stale_unregister_keeps_a_newer_adapter() {
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let (old, _old_rx) = mpsc::channel(8);
        router.register(key.clone(), old.clone()).await;
        let (new, mut new_rx) = mpsc::channel(8);
        router.register(key.clone(), new).await;

        router.unregister(&key, &old).await;

        router
            .deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
            .await
            .expect("the newer adapter is still registered");
        assert!(new_rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn seq_increments_per_delivery_within_account() {
        let router = ChannelRouter::new();
        let (tx, mut rx) = mpsc::channel(8);
        router
            .register(AccountKey::new("telegram", "alice"), tx)
            .await;
        for _ in 0..3 {
            router
                .deliver(
                    "telegram",
                    "alice",
                    "tg:1",
                    None,
                    ChannelMessageContent::Text { text: "x".into() },
                )
                .await
                .expect("deliver");
        }
        let mut seqs = Vec::new();
        for _ in 0..3 {
            let f = rx.recv().await.expect("frame");
            if let ChannelFrame::Outbound(o) = f {
                seqs.push(o.seq);
            }
        }
        assert_eq!(seqs, vec![seqs[0], seqs[0] + 1, seqs[0] + 2]);
    }

    #[tokio::test]
    async fn seq_persists_across_unregister_and_register() {
        // Spec §3.2: the seq counter is monotonic per account across
        // reconnects so `Resume{since_seq}` identifies a real point
        // in the stream. Restarting at 1 would defeat the whole point
        // of the replay buffer.
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let (tx1, _rx1) = mpsc::channel(8);
        router.register(key.clone(), tx1.clone()).await;
        let first = router
            .deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
            .await
            .expect("deliver");

        router.unregister(&key, &tx1).await;
        let (tx2, mut rx2) = mpsc::channel(8);
        router.register(key.clone(), tx2).await;
        let seq = router
            .deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "y".into() },
            )
            .await
            .expect("deliver");
        assert_eq!(
            seq,
            first + 1,
            "seq continues from prior count after reconnect"
        );
        let _ = rx2.recv().await;
    }

    #[tokio::test]
    async fn deliver_returns_no_session_after_unregister() {
        // Unregister marks tx=None but keeps state alive. Until a new
        // register, deliveries fail with NoSession — agents never
        // block on adapter availability (spec §3.2).
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let (tx, _rx) = mpsc::channel(8);
        router.register(key.clone(), tx.clone()).await;
        router.unregister(&key, &tx).await;
        let r = router
            .deliver(
                "telegram",
                "alice",
                "tg:1",
                None,
                ChannelMessageContent::Text { text: "x".into() },
            )
            .await;
        assert!(matches!(r, Err(DeliverError::NoSession(_))));
    }

    #[tokio::test]
    async fn replay_since_returns_frames_above_seq() {
        // Deliver three frames, simulate adapter ack of seq=1,
        // unregister + re-register, ask for replay since_seq=1 →
        // should get seqs 2 and 3 back in order.
        let router = ChannelRouter::new();
        let key = AccountKey::new("telegram", "alice");
        let (tx, mut rx) = mpsc::channel(8);
        router.register(key.clone(), tx.clone()).await;
        let mut delivered = Vec::new();
        for i in 0..3 {
            let seq = router
                .deliver(
                    "telegram",
                    "alice",
                    "tg:1",
                    None,
                    ChannelMessageContent::Text {
                        text: format!("m{i}"),
                    },
                )
                .await
                .expect("deliver");
            delivered.push(seq);
        }
        // Drain the in-flight frames the original tx received.
        for _ in 0..3 {
            let _ = rx.recv().await.expect("frame");
        }

        router.unregister(&key, &tx).await;
        let replayed = router.replay_since(&key, delivered[0], &tx).await;
        let seqs: Vec<u64> = replayed
            .iter()
            .filter_map(|f| match f {
                ChannelFrame::Outbound(o) => Some(o.seq),
                _ => None,
            })
            .collect();
        assert_eq!(
            seqs,
            delivered[1..],
            "replay returns frames strictly after since_seq"
        );
    }

    #[tokio::test]
    async fn replay_since_unknown_key_returns_empty() {
        // Cold-start reconnect after gateway restart: account_state
        // doesn't exist yet, replay yields nothing instead of panicking.
        let router = ChannelRouter::new();
        let got = router
            .replay_since(&AccountKey::new("telegram", "ghost"), 42, &stranger())
            .await;
        assert!(got.is_empty());
    }

    fn channels_with(
        account: &str,
        dm_policy: DmPolicy,
        allow_from: &[&str],
    ) -> std::collections::BTreeMap<String, ChannelEntry> {
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "telegram".to_string(),
            ChannelEntry {
                accounts: vec![crate::config::ChannelAccount {
                    id: account.into(),
                    adapter_token_ref: "secret:channel:telegram:bot".into(),
                    dm_policy,
                    allow_from: allow_from.iter().map(|s| (*s).to_string()).collect(),
                    admins: Vec::new(),
                    bot_username: None,
                    group_policy: crate::config::GroupPolicy::Off,
                    allow_groups: Vec::new(),
                }],
            },
        );
        m
    }

    #[test]
    fn outbound_requires_binding_to_the_account() {
        let (a1, a2) = (havn_core::AgentId::new(), havn_core::AgentId::new());
        let bs = vec![binding(&a1.to_string(), "telegram", "bot")];
        let chs = channels_with("bot", DmPolicy::Open, &[]);
        assert_eq!(
            outbound_permitted(&bs, &chs, a2, "telegram", "bot", "123"),
            Err(OutboundRefusal::NotBound)
        );
        assert_eq!(
            outbound_permitted(&bs, &chs, a1, "telegram", "other-bot", "123"),
            Err(OutboundRefusal::NotBound)
        );
        assert_eq!(
            outbound_permitted(&bs, &chs, a1, "telegram", "bot", "123"),
            Ok(())
        );
    }

    #[test]
    fn outbound_matches_non_canonical_binding_ids() {
        let a1 = havn_core::AgentId::new();
        let bs = vec![binding(&a1.to_string().to_uppercase(), "telegram", "bot")];
        let chs = channels_with("bot", DmPolicy::Open, &[]);
        assert_eq!(
            outbound_permitted(&bs, &chs, a1, "telegram", "bot", "123"),
            Ok(())
        );
    }

    #[test]
    fn outbound_under_allowlist_only_reaches_allowed_senders() {
        let a1 = havn_core::AgentId::new();
        let bs = vec![binding(&a1.to_string(), "telegram", "bot")];
        let chs = channels_with("bot", DmPolicy::Allowlist, &["111"]);
        assert_eq!(
            outbound_permitted(&bs, &chs, a1, "telegram", "bot", "111"),
            Ok(())
        );
        assert_eq!(
            outbound_permitted(&bs, &chs, a1, "telegram", "bot", "999"),
            Err(OutboundRefusal::RecipientNotAllowed)
        );
    }

    #[test]
    fn outbound_to_unconfigured_account_is_refused() {
        let a1 = havn_core::AgentId::new();
        let bs = vec![binding(&a1.to_string(), "telegram", "bot")];
        let chs = std::collections::BTreeMap::new();
        assert_eq!(
            outbound_permitted(&bs, &chs, a1, "telegram", "bot", "111"),
            Err(OutboundRefusal::UnknownAccount)
        );
    }
}
