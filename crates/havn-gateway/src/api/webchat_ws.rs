//! `/ws/chat/{agent_id}` — `WebSocket` `WebChat` endpoint.
//!
//! Auth (Phase 1, single-user mode). The connect URL must include a
//! `?token=<user_id>` query param matching the gateway's current user;
//! Phase 2 replaces this with a signed short-lived token from real auth.
//! The `Origin` header must be present and match the configured allowlist,
//! or, on the dashboard's listener, be that page's own origin —
//! direct mitigation of `OpenClaw`'s "`ClawJacked`" CVE class so that
//! malicious websites cannot drive a local agent over a permissive
//! `WebSocket`. The agent itself must already be running (registered and
//! connected to the agent socket); the handler returns 503 otherwise.
//!
//! Per-session lifecycle: register with [`WebChatRouter`]
//! → spawn writer task draining mpsc → read WS frames → wrap as `InboundMessage`
//! with `channel_id = session_id` → forward to agent. Agent's `OutboundMessage`
//! reaches us via the agent socket handler, which calls
//! [`WebChatRouter::deliver`](crate::webchat::WebChatRouter::deliver) to push
//! back onto the mpsc of every open tab of this session.

use axum::Extension;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::Utc;
use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use havn_core::{AgentId, InboundMessage, MessageContent};
use havn_proto::GatewayToAgent;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::AppState;
use crate::agent_registry::AgentRegistry;
use crate::api::{ApiError, WS_SEND_TIMEOUT};
use crate::auth::AuthedUser;
use crate::webchat::{ClientMessage, ServerMessage, SessionId, TabId, WebChatRouter};

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
    dashboard: Option<Extension<crate::dashboard::SameOrigin>>,
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
    let own = dashboard.is_some() && crate::dashboard::same_origin(origin, &headers);
    if !own && !origins.iter().any(|allowed| allowed == origin) {
        warn!(origin, "rejecting webchat WS: origin not in allowlist");
        return Err(ApiError::BadRequest(format!("origin {origin} not allowed")));
    }
    drop(origins);

    // Token check: the dashboard fetches its `ws_token` from /me (which
    // equals the user's id) and passes it on the upgrade query string.
    // Combined with the identity-header extractor above, this double-binds
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

/// Files one message carries at most.
const MAX_ATTACHMENTS: usize = 10;

/// Whether `a` names files in the inbox, with a name and type of a
/// sensible length: what the upload answered with.
fn attachment_ok(a: &havn_core::Attachment) -> bool {
    havn_core::Attachment::in_inbox(&a.path)
        && a.text_path
            .as_deref()
            .is_none_or(havn_core::Attachment::in_inbox)
        && !a.name.is_empty()
        && a.name.chars().count() <= 255
        && !a.name.chars().any(char::is_control)
        && a.media_type.len() <= 100
        && !a.media_type.chars().any(char::is_control)
}

/// A chat id a tab named, when it is short enough to be one.
fn chat_id(conversation: Option<String>) -> Option<String> {
    conversation.filter(|c| !c.is_empty() && c.len() <= crate::webchat::MAX_CONVERSATION_ID)
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
    // Two browser tabs for the same agent share one session; each is its
    // own tab in the router and gets every reply.
    let session_id: SessionId = crate::webchat::session_for(&sender_id, agent_id);
    info!(%agent_id, %session_id, "webchat session opened");

    // Who the chat commands take the user for.
    let asker = web_asker(&state, agent_id, session_id, &sender_id).await;
    let (ws_sink, ws_stream) = socket.split();
    serve_session(
        ws_sink,
        ws_stream,
        &state.registry,
        &state.webchat_router,
        &state.db,
        agent_id,
        session_id,
        &sender_id,
        Some((&state, asker)),
    )
    .await;
    info!(%agent_id, %session_id, "webchat session closed");
}

/// The web chat user `sender_id` as chat commands see them: the agent's
/// owner may use the admin ones.
async fn web_asker(
    state: &AppState,
    agent_id: AgentId,
    session_id: SessionId,
    sender_id: &str,
) -> crate::commands::Asker {
    let agent = havn_db::repo::agents::find_by_id(&state.db, agent_id)
        .await
        .ok()
        .flatten();
    let owner = agent
        .as_ref()
        .is_some_and(|a| a.owner_id.to_string() == sender_id);
    let language = agent
        .as_ref()
        .map(|a| havn_core::Language::of_config(&a.config))
        .unwrap_or_default();
    let name = match sender_id.parse() {
        Ok(id) => havn_db::repo::users::find_by_id(&state.db, id)
            .await
            .ok()
            .flatten()
            .map(|u| u.display_name),
        Err(_) => None,
    };
    crate::commands::Asker {
        agent_id,
        scope: session_id.to_string(),
        conversation: None,
        who: format!(
            "{} in the web chat{}",
            name.clone().unwrap_or_else(|| sender_id.to_string()),
            if owner { ", the agent's owner" } else { "" }
        ),
        who_shown: language.pick(
            format!(
                "{} in the web chat{}",
                name.clone().unwrap_or_else(|| sender_id.to_string()),
                if owner { ", the agent's owner" } else { "" }
            ),
            format!(
                "网页聊天里的 {}{}",
                name.unwrap_or_else(|| sender_id.to_string()),
                if owner { "，agent 的所有者" } else { "" }
            ),
        ),
        admin: owner,
        owner,
        admin_hint: language.pick("the agent's owner", "agent 的所有者"),
        language,
    }
}

/// Answer the chat command `parsed`, sent in chat `conversation` of a web
/// chat session; what to send the agent as the sender's message instead
/// (a choice, issue #204), and the question it answers.
async fn web_command(
    state: &AppState,
    asker: &crate::commands::Asker,
    conversation: Option<String>,
    parsed: crate::commands::Parsed<'_>,
) -> Option<(String, String)> {
    let mut asker = asker.clone();
    asker.conversation = conversation;
    let session: SessionId = match asker.scope.parse() {
        Ok(s) => s,
        Err(_) => return None,
    };
    let reply = match parsed {
        refused @ (crate::commands::Parsed::Unknown(_) | crate::commands::Parsed::Misused(_)) => {
            refused.refusal(asker.language).unwrap_or_default()
        }
        crate::commands::Parsed::Known(command, args) => match command.name {
            // The page has its own way of doing these.
            "new" => asker
                .language
                .pick(
                    "(Start a new chat with the New chat button.)",
                    "（用「New chat」按钮开始新对话。）",
                )
                .into(),
            "stop" => {
                let stop = GatewayToAgent::StopTurn(havn_proto::StopTurn {
                    conversation: asker.conversation.clone(),
                    channel_id: asker.scope.clone(),
                });
                if let Err(e) = state.registry.send(asker.agent_id, stop).await {
                    warn!(agent_id = %asker.agent_id, error = ?e, "failed to forward StopTurn");
                }
                return None;
            }
            "retry" => asker
                .language
                .pick(
                    "(Answer again with the button under the reply.)",
                    "（用回复下面的按钮重新回答。）",
                )
                .into(),
            _ => match crate::commands::run(state, &asker, command, args).await {
                crate::commands::Action::Reply(text) => text,
                crate::commands::Action::Say { text, question } => {
                    return Some((text, question));
                }
                crate::commands::Action::Agent(command) => {
                    let frame = GatewayToAgent::ChatCommand(command);
                    let saved = crate::commands::saved_already(&frame, asker.language);
                    if !state
                        .webchat_router
                        .expect_command_reply(asker.agent_id, session)
                    {
                        asker
                            .language
                            .pick(
                                "(Too many commands are waiting for an answer; try again in a \
                                 moment.)",
                                "（等待回答的命令太多了，请稍后再试。）",
                            )
                            .into()
                    } else {
                        match state.registry.send(asker.agent_id, frame).await {
                            Ok(()) => return None,
                            Err(e) => {
                                state
                                    .webchat_router
                                    .take_command_reply(asker.agent_id, session);
                                warn!(agent_id = %asker.agent_id, error = ?e, "failed to forward a chat command");
                                saved
                                    .unwrap_or(asker.language.pick(
                                        "(The agent is not reachable right now; try again in a \
                                         moment.)",
                                        "（现在连不上 agent，请稍后再试。）",
                                    ))
                                    .into()
                            }
                        }
                    }
                }
            },
        },
    };
    if let Err(e) = state
        .webchat_router
        .command_notice(asker.agent_id, session, asker.conversation.clone(), reply)
        .await
    {
        debug!(error = ?e, "no tab took a command's answer");
    }
    None
}

/// Serve one tab until the browser closes it, the socket fails, or the
/// router drops the tab for not reading.
#[allow(
    clippy::too_many_arguments,
    reason = "the session's handles; a struct for one call site would only rename them"
)]
async fn serve_session<Si, St, E>(
    mut ws_sink: Si,
    mut ws_stream: St,
    registry: &AgentRegistry,
    router: &WebChatRouter,
    db: &sqlx::SqlitePool,
    agent_id: AgentId,
    session_id: SessionId,
    sender_id: &str,
    // The gateway, and who the user is, for chat commands; without them
    // (tests) a command goes to the agent as text.
    commands: Option<(&AppState, crate::commands::Asker)>,
) where
    Si: Sink<Message> + Unpin + Send + 'static,
    St: Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
{
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(SESSION_OUTBOUND_CAPACITY);
    let registration = router.register(session_id, agent_id, tx).await;

    // Outbound task: drain mpsc, send to WS.
    let mut outbound = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    warn!(error = %e, "serialize ServerMessage failed");
                    break;
                }
            };
            match tokio::time::timeout(WS_SEND_TIMEOUT, ws_sink.send(Message::Text(json.into())))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    warn!(%session_id, "webchat client is not reading; closing the tab");
                    break;
                }
            }
        }
        // Best-effort close.
        let _ = tokio::time::timeout(WS_SEND_TIMEOUT, ws_sink.send(Message::Close(None))).await;
    });
    let mut cleanup = TabCleanup {
        router: router.clone(),
        tab: registration.tab,
        outbound: outbound.abort_handle(),
        armed: true,
    };

    // Inbound loop: read WS frames, dispatch as InboundMessage frames to the
    // agent. Ends too when the outbound task does or the router closes the
    // tab; returning drops the socket.
    let mut outbound_done = false;
    loop {
        let frame = tokio::select! {
            frame = ws_stream.next() => frame,
            _ = &mut outbound => {
                outbound_done = true;
                None
            }
            () = registration.closed.notified() => None,
        };
        let Some(frame) = frame else { break };
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
        let mut attachments = Vec::new();
        // The question a choice answers, forgotten once it reached the agent.
        let mut answered = None;
        let (user_text, conversation, branch) = match parsed {
            ClientMessage::Send {
                content,
                conversation,
                edit,
                attachments: sent,
            } if edit.is_none()
                && sent.is_empty()
                && commands.is_some()
                && crate::commands::parse(&content).is_some() =>
            {
                let chat = chat_id(conversation);
                let said = match (commands.as_ref(), crate::commands::parse(&content)) {
                    (Some((state, asker)), Some(parsed)) => {
                        web_command(state, asker, chat.clone(), parsed).await
                    }
                    _ => None,
                };
                // A choice goes on as the person's message, in the chat it
                // was made in.
                match said {
                    Some((text, question)) => {
                        answered = Some(question);
                        (text, chat, None)
                    }
                    None => continue,
                }
            }
            ClientMessage::Send {
                content,
                conversation,
                edit,
                attachments: sent,
            } => match (edit.is_some(), chat_id(edit)) {
                _ if sent.len() > MAX_ATTACHMENTS || sent.iter().any(|a| !attachment_ok(a)) => {
                    let _ = router
                        .error(
                            agent_id,
                            session_id,
                            format!(
                                "A message carries up to {MAX_ATTACHMENTS} files uploaded to the \
                                 agent's inbox."
                            ),
                        )
                        .await;
                    continue;
                }
                (true, None) => {
                    let _ = router
                        .error(
                            agent_id,
                            session_id,
                            "That message cannot be edited.".into(),
                        )
                        .await;
                    continue;
                }
                (_, edit) => {
                    // A transcript is the gateway's, read from the inbox.
                    attachments = Vec::with_capacity(sent.len());
                    for mut a in sent {
                        match commands.as_ref() {
                            Some((state, _)) => {
                                a = crate::api::files::with_transcript(state, agent_id, a).await;
                            }
                            None => a.transcript = None,
                        }
                        attachments.push(a);
                    }
                    (
                        content,
                        chat_id(conversation),
                        edit.map(|turn| havn_core::Branch::Edit { turn }),
                    )
                }
            },
            ClientMessage::Regenerate { conversation, turn } => {
                match (turn.is_some(), chat_id(turn)) {
                    (true, None) => {
                        let _ = router
                            .error(
                                agent_id,
                                session_id,
                                "That reply cannot be regenerated.".into(),
                            )
                            .await;
                        continue;
                    }
                    (_, turn) => (
                        String::new(),
                        chat_id(conversation),
                        Some(havn_core::Branch::Regenerate { turn }),
                    ),
                }
            }
            ClientMessage::Decide {
                id,
                approve,
                remember,
                note,
            } => {
                crate::agent_socket::forward_decision(
                    registry, db, router, agent_id, sender_id, &id, approve, remember, note,
                )
                .await;
                continue;
            }
            ClientMessage::Stop { conversation } => {
                let stop = GatewayToAgent::StopTurn(havn_proto::StopTurn {
                    conversation: chat_id(conversation),
                    channel_id: session_id.to_string(),
                });
                if let Err(e) = registry.send(agent_id, stop).await {
                    warn!(%agent_id, error = ?e, "failed to forward StopTurn");
                }
                continue;
            }
        };

        let inbound = InboundMessage {
            conversation,
            branch,
            channel_id: session_id.to_string(),
            sender_id: sender_id.to_string(),
            channel: "webchat".into(),
            content: MessageContent::Text { text: user_text },
            timestamp: Utc::now(),
            attachments,
            raw: serde_json::Value::Null,
            // The dashboard's chats are its owner's.
            from_owner: true,
            group: false,
        };

        let sent = registry
            .send(agent_id, GatewayToAgent::InboundMessage(inbound))
            .await;
        if sent.is_ok()
            && let Some(question) = answered
        {
            router.forget_choice(&question);
        }
        if let Err(e) = sent {
            warn!(%agent_id, error = ?e, "failed to forward InboundMessage");
            // Inform the user; the socket stays open in case the agent comes back.
            let _ = router
                .error(
                    agent_id,
                    session_id,
                    format!("Couldn't reach the agent: {e}"),
                )
                .await;
        }
    }

    cleanup.armed = false;
    router.unregister(registration.tab).await;
    if !outbound_done {
        outbound.abort();
        let _ = outbound.await;
    }
}

/// Unregisters the tab and stops its writer when `serve_session` is
/// dropped before it finishes (for example a cancelled upgrade future).
struct TabCleanup {
    router: WebChatRouter,
    tab: TabId,
    outbound: tokio::task::AbortHandle,
    armed: bool,
}

impl Drop for TabCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.outbound.abort();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let router = self.router.clone();
            let tab = self.tab;
            runtime.spawn(async move { router.unregister(tab).await });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    fn reading_sink() -> (
        impl Sink<Message, Error = ()> + Unpin + Send + 'static,
        mpsc::UnboundedReceiver<Message>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = Box::pin(futures_util::sink::unfold(
            tx,
            |tx, msg: Message| async move {
                tx.send(msg).map_err(|_| ())?;
                Ok::<_, ()>(tx)
            },
        ));
        (sink, rx)
    }

    fn stalled_sink() -> impl Sink<Message, Error = ()> + Unpin + Send + 'static {
        Box::pin(futures_util::sink::unfold((), |(), _msg: Message| {
            std::future::pending::<Result<(), ()>>()
        }))
    }

    fn browser() -> (
        mpsc::UnboundedSender<Message>,
        impl Stream<Item = Result<Message, std::io::Error>> + Unpin,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let stream = Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|m| (Ok(m), rx))
        }));
        (tx, stream)
    }

    fn spawn_tab<Si>(
        router: &WebChatRouter,
        sink: Si,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> (mpsc::UnboundedSender<Message>, tokio::task::JoinHandle<()>)
    where
        Si: Sink<Message> + Unpin + Send + 'static,
    {
        let (input, stream) = browser();
        let router = router.clone();
        let task = tokio::spawn(async move {
            let registry = AgentRegistry::new();
            let db = havn_db::connect_in_memory().await.expect("db");
            serve_session(
                sink, stream, &registry, &router, &db, agent_id, session_id, "u", None,
            )
            .await;
        });
        (input, task)
    }

    #[tokio::test]
    async fn every_tab_gets_replies_and_a_stalled_tab_is_closed_alone() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (healthy_sink, mut healthy_inbox) = reading_sink();
        let (healthy_input, healthy) = spawn_tab(&router, healthy_sink, agent_id, session_id);
        let (_stalled_input, stalled) = spawn_tab(&router, stalled_sink(), agent_id, session_id);
        while router.tab_count(session_id).await < 2 {
            tokio::task::yield_now().await;
        }

        // One message is stuck in the stalled tab's sink, its queue holds the
        // next SESSION_OUTBOUND_CAPACITY, and the one after is refused.
        let sent = SESSION_OUTBOUND_CAPACITY + 2;
        for i in 0..sent {
            router
                .deliver(
                    agent_id,
                    havn_core::OutboundMessage {
                        conversation: None,
                        channel_id: session_id.to_string(),
                        content: MessageContent::Text {
                            text: format!("m{i}"),
                        },
                        reply_to: None,
                        buttons: Vec::new(),
                        poll: None,
                        partial: false,
                    },
                )
                .await
                .expect("the healthy tab takes every message");
            tokio::task::yield_now().await;
        }

        tokio::time::timeout(Duration::from_secs(5), stalled)
            .await
            .expect("the stalled tab is closed")
            .expect("join");
        assert_eq!(
            router.tab_count(session_id).await,
            1,
            "the healthy tab stays open and is not replaced"
        );
        for i in 0..sent {
            match healthy_inbox.recv().await {
                Some(Message::Text(t)) => {
                    let expected = format!("{:?}", format!("m{i}"));
                    assert!(t.as_str().contains(&expected), "{t}");
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(!healthy.is_finished());

        drop(healthy_input);
        tokio::time::timeout(Duration::from_secs(5), healthy)
            .await
            .expect("the healthy tab ends with its browser")
            .expect("join");
        assert_eq!(router.tab_count(session_id).await, 0);
    }

    #[tokio::test]
    async fn a_cancelled_session_still_unregisters_its_tab() {
        let router = WebChatRouter::new();
        let agent_id = AgentId::new();
        let session_id = Uuid::now_v7();
        let (sink, _inbox) = reading_sink();
        let (_input, task) = spawn_tab(&router, sink, agent_id, session_id);
        while router.tab_count(session_id).await < 1 {
            tokio::task::yield_now().await;
        }

        task.abort();
        let _ = task.await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert_eq!(router.tab_count(session_id).await, 0);
    }
}
