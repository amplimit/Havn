//! `/api/v1/channel` — bidirectional WebSocket for external channel
//! adapter daemons (spec §3.1).
//!
//! One open WebSocket per (adapter daemon, channel account). Adapter
//! authenticates with `X-Adapter-Token`; gateway resolves
//! (channel, account) from query params, looks up the credential row
//! by handle, decrypts, and compares against the supplied token.
//!
//! Frames in both directions are typed JSON via [`havn_proto::channel::
//! ChannelFrame`]. v0.2 implementation accepts inbound and ping/resume
//! frames, replies to ping with pong, and routes inbound messages to
//! the bound agent. Outbound replay buffer + agent → WS push lands in
//! the next vertical (the `bindings` lookup TODO at the bottom of
//! `handle_inbound`).
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
use havn_db::repo::credentials::{self as creds, CredentialScope};
use havn_proto::GatewayToAgent;
use havn_proto::channel::{ChannelFrame, ChannelInbound, ChannelMessageContent};
use serde::Deserialize;
use std::str::FromStr as _;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::AppState;
use crate::api::ApiError;
use crate::channel_id_codec;
use crate::channel_router::{self, AccountKey};
use crate::secret_ref;

/// Header carrying the operator-generated adapter authentication
/// token (spec §3.3). The matching credential row is keyed by
/// (provider = "channel:<channel>", name = <account_id>); the gateway
/// finds it by handle and constant-time-compares the decrypted bytes
/// against this header.
const ADAPTER_TOKEN_HEADER: &str = "x-adapter-token";

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
        "channel WS: adapter authenticated"
    );

    let account_id = q.account.clone();
    let channel_id = q.channel.clone();
    Ok(ws.on_upgrade(move |socket| handle_session(socket, state, channel_id, account_id)))
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
/// generous — a slow adapter would pile up replies; the bounded
/// channel applies natural backpressure to the gateway-side
/// `channel_router::deliver` call (which awaits the send).
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
    state
        .channel_router
        .register(key.clone(), frame_tx.clone())
        .await;

    // Separate mpsc for raw WS control frames (Pong replies to
    // protocol-level Pings). Keeps `ChannelRouter` typed on
    // `ChannelFrame` (clean abstraction) while still letting the
    // gateway honour RFC 6455 control-frame liveness via the same
    // single ws_sink owner.
    let (raw_tx, mut raw_rx) = mpsc::channel::<Message>(8);

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Outbound task: drain both mpscs, write to WS. tokio::select!
    // gives equal priority. Closes WS when either mpsc is dropped or
    // any write fails.
    let outbound = tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe_frame = frame_rx.recv() => {
                    let Some(frame) = maybe_frame else { break };
                    let s = match serde_json::to_string(&frame) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(error = %e, "channel WS: failed to serialize outbound frame");
                            break;
                        }
                    };
                    if ws_sink.send(Message::Text(s.into())).await.is_err() {
                        break;
                    }
                }
                maybe_raw = raw_rx.recv() => {
                    let Some(raw) = maybe_raw else { break };
                    if ws_sink.send(raw).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_sink.send(Message::Close(None)).await;
    });

    let mut mismatch_count: u32 = 0;
    while let Some(frame) = ws_stream.next().await {
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
                let frames = state.channel_router.replay_since(&key, r.since_seq).await;
                let replayed = frames.len();
                for frame in frames {
                    if frame_tx.send(frame).await.is_err() {
                        warn!(
                            channel = %channel_id,
                            account = %account_id,
                            since_seq = r.since_seq,
                            "channel WS: outbound mpsc closed mid-replay"
                        );
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
            // ChannelFrame is non_exhaustive but serde already rejects
            // unknown discriminator strings before we reach this match
            // (see `unknown_kind_is_rejected` in havn-proto's tests).
            // The wildcard exists only to satisfy the compiler against
            // future variants added by this crate's own future selves
            // — at which point the right thing is to handle them, not
            // silently drop. Compile error here = "you added a variant,
            // now wire it up".
            #[allow(
                unreachable_patterns,
                reason = "compiler requires arm for non_exhaustive enum"
            )]
            _ => unreachable!("serde rejects unknown ChannelFrame kinds before this match"),
        }
    }

    state.channel_router.unregister(&key).await;
    outbound.abort();
    let _ = outbound.await;
    info!(channel = %channel_id, account = %account_id, "channel WS closed");
}

/// Route an inbound frame to the agent bound to `(channel,
/// account_id)` via gateway config `[[bindings]]` (spec §3.5). When
/// no binding exists, log + drop — the user gets no reply, which is
/// the operator-visible signal that they need to add a binding (the
/// startup `dangling_bindings` warning catches the inverse case).
async fn handle_inbound(state: &AppState, channel: &str, inb: ChannelInbound) {
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

    let encoded_channel_id = channel_id_codec::encode(channel, &inb.account_id, &inb.sender_id);
    let content = match inb.content {
        ChannelMessageContent::Text { text } => MessageContent::Text { text },
        ChannelMessageContent::Image { url, caption } => MessageContent::Image { url, caption },
        ChannelMessageContent::File { url, filename } => MessageContent::File { url, filename },
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
    let msg = InboundMessage {
        channel_id: encoded_channel_id,
        sender_id: inb.sender_id,
        channel: channel.to_string(),
        content,
        timestamp: inb.timestamp,
        raw: inb.raw.unwrap_or(serde_json::Value::Null),
    };

    // Parity with WebChat: a message to a stopped agent should wake it, not
    // vanish. WebChat lazy-spawns on its WS upgrade (api::webchat_ws); channel
    // inbound had no equivalent, so a message to an offline bound agent used to
    // be dropped with only a warn. The binding is the authorization here;
    // attribute the lazy start to the agent's owner for the audit log.
    //
    // This `ensure_running` is awaited inline in the per-account WS read loop,
    // so a cold spawn blocks this one connection (including its ping/pong) for
    // up to ensure_running's handshake timeout. That head-of-line blocking is
    // intentional: it bounds the stall to a single (channel, account) — never
    // other accounts — and the inline await is what preserves inbound ordering
    // across a cold-start burst (message N+1 isn't read until N is forwarded).
    // Do NOT move this to a detached task without restoring an ordering
    // guarantee. The hot path (already connected) skips it via is_connected.
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

    if let Err(e) = state
        .registry
        .send(agent_id, GatewayToAgent::InboundMessage(msg))
        .await
    {
        warn!(
            channel,
            account = %inb.account_id,
            %agent_id,
            error = ?e,
            "channel inbound: failed to forward to agent (offline?)"
        );
    }
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
}
