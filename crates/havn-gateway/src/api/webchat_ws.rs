//! `/ws/chat/{agent_id}` — `WebSocket` `WebChat` endpoint.
//!
//! Auth (Phase 1, single-user mode). The connect URL must include a
//! `?token=<user_id>` query param matching the gateway's current user;
//! Phase 2 replaces this with a signed short-lived token from real auth.
//! The `Origin` header must be present and match the configured allowlist —
//! direct mitigation of `OpenClaw`'s "`ClawJacked`" CVE class so that
//! malicious websites cannot drive a local agent over a permissive
//! `WebSocket`. The agent itself must already be running (registered and
//! connected to the agent socket); the handler returns 503 otherwise.
//!
//! Per-session lifecycle: register with [`WebChatRouter`](crate::webchat::WebChatRouter)
//! → spawn writer task draining mpsc → read WS frames → wrap as `InboundMessage`
//! with `channel_id = session_id` → forward to agent. Agent's `OutboundMessage`
//! reaches us via the agent socket handler, which calls
//! [`WebChatRouter::deliver`](crate::webchat::WebChatRouter::deliver) to push
//! back onto this session's mpsc.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::Utc;
use futures_util::{SinkExt as _, StreamExt as _};
use havn_core::{AgentId, InboundMessage, MessageContent};
use havn_proto::GatewayToAgent;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;
use crate::webchat::{ClientMessage, ServerMessage, SessionId};

const SESSION_OUTBOUND_CAPACITY: usize = 64;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: String,
}

pub async fn handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(agent_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // Origin must be present and on the allowlist (spec §11 ClawJacked mitigation).
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            warn!("rejecting webchat WS: missing Origin header");
            ApiError::BadRequest("missing Origin header".into())
        })?;
    let origins = state.allowed_origins.load();
    if !origins.iter().any(|allowed| allowed == origin) {
        warn!(origin, "rejecting webchat WS: origin not in allowlist");
        return Err(ApiError::BadRequest(format!("origin {origin} not allowed")));
    }
    drop(origins);

    // Token check: the dashboard fetches its `ws_token` from /me (which
    // equals the user's id) and passes it on the upgrade query string.
    // Combined with the X-User-ID extractor above, this double-binds
    // the WS to the resolved subject — even if the upstream proxy
    // somehow leaked the cookie / SSO session, an attacker would also
    // need to know the user's id to upgrade. Symmetric to spec §11.
    if query.token != user.id.to_string() {
        warn!("rejecting webchat WS: token does not match resolved user");
        return Err(ApiError::BadRequest("invalid token".into()));
    }

    let agent = crate::api::agents::resolve_agent(&agent_id, &user, &state).await?;
    let agent_id = agent.id;

    // Lazy-spawn: starting an agent is a runtime detail, not a UX
    // step the user should have to think about. Idempotent + race-
    // safe via per-agent start lock; multiple browser tabs hitting
    // an offline agent serialize through this and only one spawn
    // happens. See agents::ensure_running for the full flow.
    crate::api::agents::ensure_running(&state, user.id, agent_id).await?;

    let sender_id = user.id.to_string();
    Ok(ws.on_upgrade(move |socket| handle_session(socket, state, agent_id, sender_id)))
}

async fn handle_session(socket: WebSocket, state: AppState, agent_id: AgentId, sender_id: String) {
    // Stable session_id per (user, agent) pair, derived deterministically
    // via UUIDv5 over a fixed namespace. Reasoning: each WS reconnect (page
    // reload, navigate-away-and-back, network blip) reuses the same id, so
    // the agent's `conversations` table in agent.db accumulates one
    // continuous thread per (user, agent). Without this, every reconnect
    // forked a fresh channel_id and the previous chat history became
    // unreachable from the dashboard.
    //
    // Trade-off: opening two browser tabs for the same agent has both
    // tabs sharing one session — the router's `register` overwrites tx,
    // so the second tab "wins" and the first goes silent. Acceptable for
    // v1; if multi-tab matters later we'll multiplex tx into a Vec.
    let session_id: SessionId = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("havn-webchat:{sender_id}:{agent_id}").as_bytes(),
    );
    info!(%agent_id, %session_id, "webchat session opened");

    let (mut ws_sink, mut ws_stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(SESSION_OUTBOUND_CAPACITY);
    state
        .webchat_router
        .register(session_id, agent_id, tx)
        .await;

    // Outbound task: drain mpsc, send to WS.
    let outbound = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    warn!(error = %e, "serialize ServerMessage failed");
                    break;
                }
            };
            if ws_sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
        // Best-effort close.
        let _ = ws_sink.send(Message::Close(None)).await;
    });

    // Inbound loop: read WS frames, dispatch as InboundMessage frames to the agent.
    while let Some(frame) = ws_stream.next().await {
        let msg = match frame {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "ws read error");
                break;
            }
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            // Phase 1 webchat is text-only; ignore other frame kinds politely.
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => continue,
        };

        let parsed: ClientMessage = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, %session_id, "discarding malformed client message");
                continue;
            }
        };
        let user_text = match parsed {
            ClientMessage::Send { content } => content,
        };

        let inbound = InboundMessage {
            channel_id: session_id.to_string(),
            sender_id: sender_id.clone(),
            channel: "webchat".into(),
            content: MessageContent::Text { text: user_text },
            timestamp: Utc::now(),
            raw: serde_json::Value::Null,
        };

        if let Err(e) = state
            .registry
            .send(agent_id, GatewayToAgent::InboundMessage(inbound))
            .await
        {
            warn!(%agent_id, error = ?e, "failed to forward InboundMessage");
            // Inform the user; the socket stays open in case the agent comes back.
            let _ = state
                .webchat_router
                .deliver(havn_core::OutboundMessage {
                    channel_id: session_id.to_string(),
                    content: MessageContent::Text {
                        text: format!("Couldn't reach the agent: {e}"),
                    },
                    reply_to: None,
                })
                .await;
        }
    }

    state.webchat_router.unregister(session_id).await;
    outbound.abort();
    let _ = outbound.await;
    info!(%agent_id, %session_id, "webchat session closed");
}
