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
//! point in the stream. Overflow (more than [`REPLAY_MAX_FRAMES`] in
//! the window) drops the oldest entry with a warn log; agents do not
//! block on adapter availability (spec §3.2).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use havn_proto::channel::{ChannelFrame, ChannelMessageContent, ChannelOutbound};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

use crate::config::ChannelBindingConfig;

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
}

#[derive(Debug, Clone)]
struct ReplayEntry {
    seq: u64,
    frame: ChannelFrame,
    inserted_at: Instant,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeliverError {
    /// No adapter daemon is currently connected for this (channel,
    /// account) pair. The agent's reply has nowhere to go; the caller
    /// (agent_socket outbound dispatch) should log + drop. This is the
    /// normal case for an adapter that's offline; agents do NOT block
    /// on adapter availability (spec §3.2).
    #[error("no adapter connected for {0:?}")]
    NoSession(AccountKey),
    /// The mpsc sender was dropped between lookup and send — the WS
    /// closed concurrently. Treated like NoSession by the caller.
    #[error("adapter writer closed")]
    WriterClosed,
}

impl ChannelRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connected adapter's outbound mpsc. If an
    /// `AccountState` already exists for `key` (reconnect), the new
    /// `tx` replaces the old one and the seq counter + replay buffer
    /// are preserved — `Resume{since_seq}` from the adapter can then
    /// pull missed frames via [`replay_since`].
    pub async fn register(&self, key: AccountKey, tx: mpsc::Sender<ChannelFrame>) {
        let mut map = self.inner.lock().await;
        match map.get_mut(&key) {
            Some(state) => {
                state.tx = Some(tx);
            }
            None => {
                map.insert(
                    key,
                    AccountState {
                        tx: Some(tx),
                        next_seq: 1,
                        replay: VecDeque::new(),
                    },
                );
            }
        }
    }

    /// Mark the adapter as disconnected but keep the `AccountState` —
    /// the replay buffer and seq counter survive until the next
    /// reconnect (or until the gateway restarts). Idempotent.
    pub async fn unregister(&self, key: &AccountKey) {
        if let Some(state) = self.inner.lock().await.get_mut(key) {
            state.tx = None;
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
        let key = AccountKey::new(channel, account_id);
        let (tx, frame, seq) = {
            let mut map = self.inner.lock().await;
            let state = map
                .get_mut(&key)
                .ok_or_else(|| DeliverError::NoSession(key.clone()))?;
            let tx = state
                .tx
                .clone()
                .ok_or_else(|| DeliverError::NoSession(key.clone()))?;
            let seq = state.next_seq;
            state.next_seq = state.next_seq.saturating_add(1);
            let frame = ChannelFrame::Outbound(ChannelOutbound {
                seq,
                account_id: account_id.to_string(),
                to: sender_id.to_string(),
                in_reply_to,
                content,
            });
            push_replay(&key, &mut state.replay, seq, frame.clone());
            (tx, frame, seq)
        };
        tx.send(frame)
            .await
            .map_err(|_| DeliverError::WriterClosed)?;
        Ok(seq)
    }

    /// Collect buffered outbound frames with `seq > since_seq` for
    /// replay through the currently-registered adapter (spec §3.2).
    /// Returns frames in original seq order. Frames older than
    /// [`REPLAY_WINDOW`] have already been evicted on every prior
    /// mutation, so the returned slice is the live replay set.
    ///
    /// If no `AccountState` exists for `key`, returns an empty Vec
    /// (cold-start reconnect — adapter is asking to resume a stream
    /// that the gateway never saw, e.g. because the gateway itself
    /// restarted). The caller logs a warn for that case.
    pub async fn replay_since(&self, key: &AccountKey, since_seq: u64) -> Vec<ChannelFrame> {
        let mut map = self.inner.lock().await;
        let Some(state) = map.get_mut(key) else {
            return Vec::new();
        };
        evict_aged(&mut state.replay);
        state
            .replay
            .iter()
            .filter(|e| e.seq > since_seq)
            .map(|e| e.frame.clone())
            .collect()
    }
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
        assert_eq!(seq, 1);

        let got = rx.recv().await.expect("frame received");
        match got {
            ChannelFrame::Outbound(o) => {
                assert_eq!(o.seq, 1);
                assert_eq!(o.account_id, "alice-tg-bot");
                assert_eq!(o.to, "tg:123");
                assert_eq!(o.in_reply_to.as_deref(), Some("tg:msg-7"));
            }
            other => panic!("expected Outbound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_returns_no_session_when_unregistered() {
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
        assert_eq!(seqs, vec![1, 2, 3]);
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
        router.register(key.clone(), tx1).await;
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

        router.unregister(&key).await;
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
        assert_eq!(seq, 2, "seq continues from prior count after reconnect");
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
        router.register(key.clone(), tx).await;
        router.unregister(&key).await;
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
        router.register(key.clone(), tx).await;
        for i in 0..3 {
            router
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
        }
        // Drain the in-flight frames the original tx received.
        for _ in 0..3 {
            let _ = rx.recv().await.expect("frame");
        }

        router.unregister(&key).await;
        let replayed = router.replay_since(&key, 1).await;
        let seqs: Vec<u64> = replayed
            .iter()
            .filter_map(|f| match f {
                ChannelFrame::Outbound(o) => Some(o.seq),
                _ => None,
            })
            .collect();
        assert_eq!(
            seqs,
            vec![2, 3],
            "replay returns frames strictly after since_seq"
        );
    }

    #[tokio::test]
    async fn replay_since_unknown_key_returns_empty() {
        // Cold-start reconnect after gateway restart: account_state
        // doesn't exist yet, replay yields nothing instead of panicking.
        let router = ChannelRouter::new();
        let got = router
            .replay_since(&AccountKey::new("telegram", "ghost"), 42)
            .await;
        assert!(got.is_empty());
    }
}
