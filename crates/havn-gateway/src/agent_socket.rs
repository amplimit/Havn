//! Unix-socket listener for connecting agent runtime processes.
//!
//! Each connected agent gets one accepted socket. The handshake is:
//! 1. Agent sends `Hello { agent_id, version }`.
//! 2. Gateway validates protocol version and that the agent was registered
//!    via the lifecycle endpoint (`POST /agents/:id/start`).
//! 3. Gateway replies `Welcome { session_token }`.
//! 4. Per-connection writer task drains an mpsc and writes frames.
//! 5. Per-connection reader loop dispatches inbound frames:
//!    - `LlmRequest` → spawn proxy task → reply with `LlmResponse`
//!    - `OutboundMessage` → route via channel adapter (Phase 1: log only)
//!    - `LivenessPing` → record (Phase 2: drives unhealthy-agent detection)

use std::path::Path;
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::Context as _;
use havn_core::{AgentId, UserId};
use havn_proto::{
    AgentQuery, AgentQueryOutcome, AgentQueryResult, AgentToGateway, FrameError, GatewayToAgent,
    IncomingQuery, LlmOutcome, LlmRequest, LlmResponse, LlmUsage, PROTOCOL_VERSION, read_frame,
    write_frame,
};
use sqlx::SqlitePool;
use tokio::io::{AsyncBufRead, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::agent_registry::AgentRegistry;
use crate::cron::CronScheduler;
use crate::llm_proxy::{self, AnthropicMessage, AnthropicRequest};
use crate::policy_resolver;
use crate::webchat::WebChatRouter;
use havn_core::Policy;
use havn_db::repo::agents as agents_repo;

const OUTBOUND_QUEUE_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct AgentSocketCtx {
    pub registry: AgentRegistry,
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub webchat_router: WebChatRouter,
    /// Per-(channel, account) outbound mpsc map for connected channel
    /// adapter daemons (spec §3). Drained by [`crate::channel_router::
    /// ChannelRouter::deliver`] when an agent's `OutboundMessage` carries
    /// a channel-adapter `channel_id` (decoded via `channel_id_codec`).
    pub channel_router: crate::channel_router::ChannelRouter,
    pub cron_scheduler: CronScheduler,
    /// Pending admin RPC senders (`MemoryForget`, `SkillPin`). The
    /// reader matches `*Response` variants and resolves the matching
    /// pending future.
    pub admin_rpc: crate::admin_rpc::AdminRpc,
    /// Cross-agent query correlator (spec §4.4 v0.7). The reader matches
    /// `AgentQueryResponse` frames against this broker so the in-flight
    /// `handle_agent_query` future can wake.
    pub query_broker: crate::query_broker::QueryBroker,
    /// Live `[embedding]` config — opaque JSON shipped to the runtime
    /// in the Welcome frame (spec §9.4 v0.7). The gateway doesn't
    /// interpret it; the runtime materialises the `EmbeddingProvider`
    /// impl. Held as `Arc<ArcSwap<Value>>` shared with `AppState` so
    /// `PATCH /embedding` atomically affects every subsequent Welcome
    /// without a side-channel.
    pub embedding_config: Arc<arc_swap::ArcSwap<serde_json::Value>>,
    /// Operator-declared extra bind-mounts shipped to each runtime in
    /// Welcome so it can add Landlock entries (spec §4.1 v0.7). Cloned
    /// from `GatewayConfig::extra_mounts` at startup; same list the
    /// spawner already used to set `HAVN_EXTRA_MOUNTS`.
    pub extra_mounts: Vec<havn_spawner::ExtraMount>,
    /// Operator-declared in-namespace tmpfs mounts (spec §16.2). The
    /// runtime needs the paths in the Welcome frame so it can add them
    /// to its Landlock rw-allowlist; the actual `mount tmpfs` happens
    /// in havn-init before the runtime starts.
    pub tmpfs_mounts: Vec<havn_spawner::TmpfsMount>,
    /// Operator-declared seccomp allowances (spec §16.2). The runtime
    /// validates names against `libc::SYS_*` and removes them from the
    /// blocklist. Empty list = the v0.6 baseline blocklist applies.
    pub seccomp_allow_extra: Vec<String>,
    /// Operator's credential decryption key (spec §13 Phase 3).
    /// `None` only on a fresh install with no credentials yet — in
    /// that mode `LlmRequest`s short-circuit with NoCredential
    /// rather than touching the proxy.
    pub keyring: Option<crate::keyring::KeyRing>,
}

/// Bind the Unix-socket listener and accept agent connections forever.
pub async fn run_listener(socket_path: &Path, ctx: AgentSocketCtx) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating socket parent {}", parent.display()))?;
    }
    // Remove any stale socket file from a previous run.
    let _ = tokio::fs::remove_file(socket_path).await;

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding agent socket {}", socket_path.display()))?;
    info!(path = %socket_path.display(), "agent socket listening");

    loop {
        let (stream, _addr) = listener.accept().await.context("accept")?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, ctx).await {
                warn!(error = ?e, "agent connection ended with error");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, ctx: AgentSocketCtx) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    // 1. Handshake: read Hello
    let hello = read_frame::<_, AgentToGateway>(&mut reader)
        .await
        .context("reading Hello")?;
    let (agent_id_str, version) = match hello {
        Some(AgentToGateway::Hello { agent_id, version }) => (agent_id, version),
        Some(other) => anyhow::bail!("expected Hello, got {other:?}"),
        None => anyhow::bail!("connection closed before Hello"),
    };

    if version != PROTOCOL_VERSION {
        anyhow::bail!("protocol version mismatch (agent {version}, gateway {PROTOCOL_VERSION})");
    }

    let agent_id = AgentId::from_str(&agent_id_str)
        .with_context(|| format!("parsing agent_id {agent_id_str:?}"))?;

    // 2. Resolve policy + send Welcome.
    //
    //    Policy is data, not code (spec §6.1). The runtime builds its tool
    //    registry from this snapshot — without it, the runtime would fall
    //    back to `Policy::default()` and ignore per-agent restrictions.
    //
    //    Lookup is best-effort: if the agent row was deleted between
    //    `start` and the runtime connecting (rare), or the DB is briefly
    //    unavailable, ship `Policy::default()` rather than refuse the
    //    connection. Refusing here would just orphan the runtime; the
    //    spawner would retry into the same dead end.
    let (policy, model): (Policy, Option<String>) =
        match agents_repo::find_by_id(&ctx.db, agent_id).await {
            Ok(Some(agent)) => {
                let model = agent
                    .config
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                // for_session walks per-agent override → user/team
                // merged → default. Same chain used at spawn time so
                // cgroup limits and tool registry stay consistent
                // (spec §6.3).
                (policy_resolver::for_session(&ctx.db, &agent).await, model)
            }
            Ok(None) => {
                warn!(%agent_id, "agent not found at handshake; sending default policy");
                (Policy::default(), None)
            }
            Err(e) => {
                warn!(%agent_id, error = %e, "policy lookup failed; sending default policy");
                (Policy::default(), None)
            }
        };

    let session_token = Uuid::now_v7().to_string();
    write_frame(
        &mut write,
        &GatewayToAgent::Welcome {
            session_token: session_token.clone(),
            policy,
            model,
            embedding: ctx.embedding_config.load().as_ref().clone(),
            // Wire form is `{path: String, mode: "ro"|"rw"}`; we
            // translate from the spawner type once at handshake. Empty
            // when the operator didn't configure any.
            extra_mounts: ctx
                .extra_mounts
                .iter()
                .map(|m| havn_proto::ExtraMountWire {
                    path: m.path.display().to_string(),
                    mode: m.mode.as_wire_str().to_string(),
                })
                .collect(),
            tmpfs_mounts: ctx
                .tmpfs_mounts
                .iter()
                .map(|m| havn_proto::TmpfsMountWire {
                    path: m.path.display().to_string(),
                    size_mb: m.size_mb,
                })
                .collect(),
            seccomp_allow_extra: ctx.seccomp_allow_extra.clone(),
        },
    )
    .await
    .context("sending Welcome")?;

    info!(%agent_id, "agent connected");

    // 3. Wire outbound channel
    let (tx, mut rx) = mpsc::channel::<GatewayToAgent>(OUTBOUND_QUEUE_CAPACITY);
    ctx.registry
        .attach(agent_id, tx)
        .await
        .with_context(|| format!("attach: agent {agent_id} not registered"))?;

    // 4. Writer task: drain mpsc → wire
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if let Err(e) = write_frame(&mut write, &frame).await {
                error!(error = %e, "agent writer failed");
                break;
            }
        }
    });

    // 5. Reader loop
    let result = run_reader(&mut reader, agent_id, &ctx).await;

    // 6. Cleanup
    ctx.registry.detach(agent_id).await;
    writer_task.abort();
    let _ = writer_task.await;

    if let Err(e) = result {
        warn!(%agent_id, error = ?e, "agent reader loop terminated");
    } else {
        info!(%agent_id, "agent disconnected cleanly");
    }
    Ok(())
}

async fn run_reader<R>(
    reader: &mut R,
    agent_id: AgentId,
    ctx: &AgentSocketCtx,
) -> Result<(), FrameError>
where
    R: AsyncBufRead + Unpin,
{
    while let Some(frame) = read_frame::<_, AgentToGateway>(reader).await? {
        match frame {
            AgentToGateway::Hello { .. } => {
                warn!(%agent_id, "duplicate Hello after handshake — ignoring");
            }
            AgentToGateway::OutboundMessage(msg) => {
                // Three dispatch cases by channel_id shape:
                //  1. "broadcast" sentinel — fan out to all open WebChat
                //     sessions for this agent (heartbeat/cron output).
                //  2. `ch|<channel>|<account>|<sender>` — channel adapter
                //     codec (spec §3, channel_id_codec). Decode +
                //     deliver via channel_router to the right WS.
                //  3. Anything else — assumed to be a WebChat session
                //     UUID; route via webchat_router.
                if msg.channel_id == "broadcast" {
                    let n = ctx.webchat_router.broadcast(agent_id, msg).await;
                    debug!(%agent_id, sessions = n, "broadcast outbound delivered");
                } else if let Some((channel, account_id, sender_id)) =
                    crate::channel_id_codec::decode(&msg.channel_id)
                {
                    // Channel-adapter outbound. Translate havn-core's
                    // MessageContent into havn-proto's
                    // ChannelMessageContent and push via the router.
                    use havn_core::MessageContent;
                    use havn_proto::channel::ChannelMessageContent;
                    let translated = match msg.content {
                        MessageContent::Text { text } => Some(ChannelMessageContent::Text { text }),
                        MessageContent::Image { url, caption } => {
                            Some(ChannelMessageContent::Image { url, caption })
                        }
                        MessageContent::File { url, filename } => {
                            Some(ChannelMessageContent::File { url, filename })
                        }
                        // Agents rarely emit Audio / Location / Contact
                        // outbound — these are user-input shapes. When
                        // they DO emit one (e.g., an agent skill that
                        // shares a location), we flatten to Text so the
                        // adapter doesn't need a per-platform "send
                        // location" pathway. Future Layer when needed.
                        MessageContent::Audio { transcript, .. } => {
                            Some(ChannelMessageContent::Text { text: transcript })
                        }
                        MessageContent::Location {
                            latitude,
                            longitude,
                            name,
                            ..
                        } => {
                            let label = name.unwrap_or_default();
                            let coords = format!("{latitude:.6},{longitude:.6}");
                            let text = if label.is_empty() {
                                format!("[location: {coords}]")
                            } else {
                                format!("[location: {coords} ({label})]")
                            };
                            Some(ChannelMessageContent::Text { text })
                        }
                        MessageContent::Contact {
                            display_name,
                            phone_number,
                            ..
                        } => {
                            let text = match phone_number {
                                Some(ph) => format!("[contact: {display_name} {ph}]"),
                                None => format!("[contact: {display_name}]"),
                            };
                            Some(ChannelMessageContent::Text { text })
                        }
                        // MessageContent is non_exhaustive in havn-core;
                        // future variants without a channel counterpart
                        // are dropped with a warn (we can't surface them
                        // through this path).
                        _ => None,
                    };
                    match translated {
                        Some(content) => {
                            if let Err(e) = ctx
                                .channel_router
                                .deliver(channel, account_id, sender_id, msg.reply_to, content)
                                .await
                            {
                                warn!(
                                    %agent_id,
                                    channel,
                                    account = account_id,
                                    error = %e,
                                    "could not deliver outbound to channel adapter (offline?)"
                                );
                            }
                        }
                        None => {
                            warn!(%agent_id, "dropping channel-adapter outbound: unsupported MessageContent variant")
                        }
                    }
                } else if let Err(e) = ctx.webchat_router.deliver(msg).await {
                    warn!(%agent_id, error = %e, "could not deliver outbound to webchat");
                }
            }
            AgentToGateway::LlmRequest(req) => {
                let ctx_cloned = ctx.clone();
                tokio::spawn(async move {
                    handle_llm_request(agent_id, req, ctx_cloned).await;
                });
            }
            AgentToGateway::LivenessPing => {
                debug!(%agent_id, "liveness ping");
            }
            AgentToGateway::CronResult(result) => {
                let job_id = result.job_id.clone();
                if let Err(e) = ctx.cron_scheduler.record_result(agent_id, result).await {
                    warn!(%agent_id, %job_id, error = %e, "cron result recording failed");
                }
            }
            AgentToGateway::MemoryForgetResponse(resp) => {
                ctx.admin_rpc.resolve(&resp.request_id, resp.outcome).await;
            }
            AgentToGateway::SkillPinResponse(resp) => {
                ctx.admin_rpc.resolve(&resp.request_id, resp.outcome).await;
            }
            AgentToGateway::AgentQuery(query) => {
                // Caller-side initiation. Spawn so a slow target doesn't
                // block the caller's reader loop — the per-target
                // concurrency cap and timeout enforcement happens
                // inside the handler.
                let ctx_cloned = ctx.clone();
                tokio::spawn(async move {
                    handle_agent_query(agent_id, query, ctx_cloned).await;
                });
            }
            AgentToGateway::AgentQueryResponse(resp) => {
                // Callee-side reply — match against pending broker entry
                // so the caller-side handler can wake.
                ctx.query_broker.resolve(resp).await;
            }
            // The protocol enum is `#[non_exhaustive]`; future variants land
            // here so older gateways gracefully ignore frames they don't know.
            other => {
                warn!(%agent_id, ?other, "unknown frame variant — ignoring");
            }
        }
    }
    Ok(())
}

async fn handle_llm_request(agent_id: AgentId, req: LlmRequest, ctx: AgentSocketCtx) {
    let request_id = req.request_id.clone();

    // Caller-pays billing override (spec §4.4 v0.7 cross-agent query).
    // When the runtime is processing an `IncomingQuery` it stamps every
    // `LlmRequest.billing_user_id` with the *caller's* user id, so token
    // spend hits the caller's credentials, not the responding agent's.
    // We validate the override is a parseable UserId before trusting it
    // — a malformed string falls back to agent-owner billing rather than
    // failing the call.
    let billing_override: Option<UserId> = req
        .billing_user_id
        .as_deref()
        .and_then(|s| UserId::from_str(s).ok());
    if req.billing_user_id.is_some() && billing_override.is_none() {
        warn!(
            %agent_id,
            raw = ?req.billing_user_id,
            "LlmRequest.billing_user_id failed to parse — falling back to agent owner",
        );
    }

    // Resolve the agent's owner — the LLM call bills against THEIR
    // credentials, not against whichever HTTP user happens to be online.
    // Spec §1.7: agent ownership drives credential resolution; the
    // X-User-ID middleware decides who's allowed to TALK to the agent
    // via the dashboard, but mid-conversation LLM proxy calls always
    // attribute to the agent's owner.
    let owner_id = match havn_db::repo::agents::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => agent.owner_id,
        Ok(None) => {
            warn!(%agent_id, "LLM request for unknown agent — cannot resolve owner");
            let response = LlmResponse {
                request_id,
                outcome: LlmOutcome::Error {
                    message: format!("agent {agent_id} not found in registry"),
                },
                provider_response: serde_json::Value::Null,
                usage: None,
            };
            let _ = ctx
                .registry
                .send(agent_id, GatewayToAgent::LlmResponse(response))
                .await;
            return;
        }
        Err(e) => {
            warn!(%agent_id, error = %e, "agent owner lookup failed");
            let response = LlmResponse {
                request_id,
                outcome: LlmOutcome::Error {
                    message: format!("owner lookup failed: {e}"),
                },
                provider_response: serde_json::Value::Null,
                usage: None,
            };
            let _ = ctx
                .registry
                .send(agent_id, GatewayToAgent::LlmResponse(response))
                .await;
            return;
        }
    };

    let response = match build_anthropic_request(&req) {
        Ok(ar) => {
            let Some(ring) = ctx.keyring.as_ref() else {
                let _ = ctx
                    .registry
                    .send(
                        agent_id,
                        GatewayToAgent::LlmResponse(LlmResponse {
                            request_id,
                            outcome: LlmOutcome::Error {
                                message: "credential encryption is not configured: \
                                          set HAVN_AGE_KEY and restart the gateway"
                                    .into(),
                            },
                            provider_response: serde_json::Value::Null,
                            usage: None,
                        }),
                    )
                    .await;
                return;
            };
            // Provider resolution: explicit `options.provider` hint wins,
            // otherwise infer from the model name (claude-* → anthropic,
            // gpt-*/o1*/o3* → openai, gemini-* → gemini, vendor/model →
            // openrouter). Unknown bare names fall back to anthropic so
            // legacy configs keep working.
            let provider = req
                .options
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .map_or_else(
                    || llm_proxy::provider::provider_for_model(&ar.model).to_string(),
                    str::to_string,
                );
            // Billing user. Spec §4.4 v0.7 lets a runtime stamp a
            // `billing_user_id` override on `LlmRequest` so an
            // IncomingQuery's LLM spend hits the caller's credentials,
            // not the responding agent's. **In v1 (same-owner only) we
            // accept the override only when it equals the agent's own
            // owner.** That's a no-op functionally — same owner means
            // override == owner_id always — but it shuts the door on a
            // malicious or buggy runtime that fakes a victim user_id to
            // drain someone else's tokens. When cross-owner support
            // lands the gate becomes "is `billing_user_id` listed in
            // an IncomingQuery this gateway dispatched to this agent?"
            // — designed-for, not yet wired.
            let billing_user = match billing_override {
                Some(u) if u == owner_id => u,
                Some(attempted_uid) => {
                    // Don't log the attempted UserId at warn — a
                    // malicious runtime could otherwise inject any
                    // valid-UUID string into the gateway's structured
                    // log stream and create false-attribution noise
                    // in log scrapers. The rejection itself is the
                    // signal; the value isn't needed at warn level.
                    // Operators chasing one of these can flip RUST_LOG
                    // to debug for `havn_gateway::agent_socket` to see
                    // the attempted id.
                    warn!(
                        %agent_id,
                        "LlmRequest.billing_user_id != agent owner — rejecting override (v1 same-owner gate; spec §4.4 v0.7)",
                    );
                    debug!(%agent_id, attempted = %attempted_uid, "rejected billing override target");
                    owner_id
                }
                None => owner_id,
            };
            match llm_proxy::complete_with_provider(
                &ctx.db,
                &ctx.http,
                ring,
                billing_user,
                &provider,
                &ar,
            )
            .await
            {
                Ok(resp) => {
                    let usage = LlmUsage {
                        input_tokens: resp.usage.input_tokens,
                        output_tokens: resp.usage.output_tokens,
                    };
                    LlmResponse {
                        request_id,
                        outcome: LlmOutcome::Ok,
                        provider_response: serde_json::to_value(&resp)
                            .unwrap_or(serde_json::Value::Null),
                        usage: Some(usage),
                    }
                }
                Err(e) => LlmResponse {
                    request_id,
                    outcome: LlmOutcome::Error {
                        message: e.to_string(),
                    },
                    provider_response: serde_json::Value::Null,
                    usage: None,
                },
            }
        }
        Err(e) => LlmResponse {
            request_id,
            outcome: LlmOutcome::Error {
                message: format!("invalid request: {e}"),
            },
            provider_response: serde_json::Value::Null,
            usage: None,
        },
    };

    if let Err(e) = ctx
        .registry
        .send(agent_id, GatewayToAgent::LlmResponse(response))
        .await
    {
        warn!(%agent_id, error = %e, "could not deliver LlmResponse");
    }
}

fn build_anthropic_request(req: &LlmRequest) -> anyhow::Result<AnthropicRequest> {
    let messages: Vec<AnthropicMessage> = serde_json::from_value(req.messages.clone())
        .context("messages field is not a valid Anthropic messages array")?;
    let max_tokens = req
        .options
        .get("max_tokens")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(1024);
    let system = req
        .options
        .get("system")
        .and_then(|v| v.as_str())
        .map(String::from);
    let temperature = req
        .options
        .get("temperature")
        .and_then(serde_json::Value::as_f64);
    let tools = req.options.get("tools").cloned();
    Ok(AnthropicRequest {
        model: req.model.clone(),
        max_tokens,
        messages,
        system,
        temperature,
        tools,
    })
}

/// Hard ceiling on `AgentQuery.timeout_seconds`. Caller-supplied values
/// are clamped — even with a malicious caller asking for 24h, the
/// broker tears the in-flight query down well before that. Spec §4.4
/// v0.7 specifies 300s; keep both numbers in sync if changed.
const AGENT_QUERY_MAX_TIMEOUT_SECS: u32 = 300;

/// Caller-side handler: validate same-owner, route an `IncomingQuery` to
/// the target, await the reply (or timeout), record the audit row, and
/// send `AgentQueryResult` back to the caller.
///
/// Boundary rules per spec §4.4 v0.7:
/// - Same owner only. Cross-owner returns `Error{message: "..."}` with
///   no audit row beyond the standard agent-not-found / not-connected
///   paths.
/// - Target must be connected. Dispatch refuses to auto-start; the
///   caller's LLM sees an `Error` tool_result and decides to retry.
/// - Timeout clamped to [`AGENT_QUERY_MAX_TIMEOUT_SECS`]; on expiry the
///   pending broker slot is cancelled and the caller gets `Timeout`.
async fn handle_agent_query(caller_id: AgentId, query: AgentQuery, ctx: AgentSocketCtx) {
    let request_id = query.request_id.clone();
    let started_at = chrono::Utc::now();

    // Look up caller's owner first — every rejection path past this
    // point can write a structured audit row (spec §4.4 rule 1: "audit
    // row written either way"). The two paths that fail before we
    // know the user (caller agent missing from DB, DB error during
    // lookup) only get tracing warnings — they describe an impossible
    // state (a connected agent absent from the agents table) so we
    // don't have a user to attribute against.
    let caller_owner = match havn_db::repo::agents::find_by_id(&ctx.db, caller_id).await {
        Ok(Some(a)) => a.owner_id,
        Ok(None) => {
            warn!(%caller_id, "agent_query from unknown caller agent");
            send_query_error(
                &ctx,
                caller_id,
                request_id.clone(),
                "caller agent not found".into(),
            )
            .await;
            return;
        }
        Err(e) => {
            warn!(%caller_id, error = %e, "caller owner lookup failed");
            send_query_error(
                &ctx,
                caller_id,
                request_id.clone(),
                format!("caller lookup failed: {e}"),
            )
            .await;
            return;
        }
    };

    let target_id = match AgentId::from_str(&query.target_agent_id) {
        Ok(id) => id,
        Err(e) => {
            send_query_error(
                &ctx,
                caller_id,
                request_id.clone(),
                format!("invalid target_agent_id: {e}"),
            )
            .await;
            // No cross_agent_queries row (target_id won't satisfy the
            // FK) but we DO write a high-level audit_log entry —
            // operators monitoring 'agent.queried' actions see all
            // attempted queries, valid or not.
            audit_pre_dispatch(
                &ctx,
                caller_id,
                caller_owner,
                &query.target_agent_id,
                "invalid_target_id",
            )
            .await;
            return;
        }
    };

    if target_id == caller_id {
        // Querying yourself is a recursion / fork-bomb shaped pattern —
        // refuse before the broker even touches the registry. v1 has no
        // legitimate use case for it.
        send_query_error(
            &ctx,
            caller_id,
            request_id.clone(),
            "cannot query self".into(),
        )
        .await;
        // target_id == caller_id, so the FK is satisfied; record a full
        // cross_agent_queries row for symmetry with cross-owner refusals.
        record_audit(
            &ctx,
            caller_id,
            target_id,
            caller_owner,
            &query,
            havn_db::repo::cross_agent_queries::Outcome::Error,
            Some("self-query refused"),
            started_at,
        )
        .await;
        return;
    }
    let target_agent = match havn_db::repo::agents::find_by_id(&ctx.db, target_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            send_query_error(
                &ctx,
                caller_id,
                request_id.clone(),
                "target agent not found".into(),
            )
            .await;
            return;
        }
        Err(e) => {
            send_query_error(
                &ctx,
                caller_id,
                request_id.clone(),
                format!("target lookup failed: {e}"),
            )
            .await;
            return;
        }
    };
    if target_agent.owner_id != caller_owner {
        send_query_error(
            &ctx,
            caller_id,
            request_id.clone(),
            "target agent is owned by a different user".into(),
        )
        .await;
        record_audit(
            &ctx,
            caller_id,
            target_id,
            caller_owner,
            &query,
            havn_db::repo::cross_agent_queries::Outcome::Error,
            Some("cross-owner refused"),
            started_at,
        )
        .await;
        return;
    }

    if !ctx.registry.is_connected(target_id).await {
        send_query_error(
            &ctx,
            caller_id,
            request_id.clone(),
            "target agent is not running".into(),
        )
        .await;
        record_audit(
            &ctx,
            caller_id,
            target_id,
            caller_owner,
            &query,
            havn_db::repo::cross_agent_queries::Outcome::Error,
            Some("target not running"),
            started_at,
        )
        .await;
        return;
    }

    // Dispatch IncomingQuery to the target.
    let timeout_secs = query.timeout_seconds.min(AGENT_QUERY_MAX_TIMEOUT_SECS);
    let incoming = IncomingQuery {
        request_id: request_id.clone(),
        caller_agent_id: caller_id.to_string(),
        billing_user_id: caller_owner.to_string(),
        prompt: query.prompt.clone(),
        include_transcript: query.include_transcript,
    };
    let rx = ctx.query_broker.register(&request_id).await;
    if let Err(e) = ctx
        .registry
        .send(target_id, GatewayToAgent::IncomingQuery(incoming))
        .await
    {
        ctx.query_broker.cancel(&request_id).await;
        send_query_error(
            &ctx,
            caller_id,
            request_id.clone(),
            format!("dispatch to target failed: {e}"),
        )
        .await;
        record_audit(
            &ctx,
            caller_id,
            target_id,
            caller_owner,
            &query,
            havn_db::repo::cross_agent_queries::Outcome::Error,
            Some("dispatch failed"),
            started_at,
        )
        .await;
        return;
    }

    // Await the response under the (clamped) timeout.
    let response =
        match tokio::time::timeout(std::time::Duration::from_secs(u64::from(timeout_secs)), rx)
            .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                // Sender dropped without delivering — broker pending entry
                // is already gone via Resolve, so we're clean.
                send_query_error(
                    &ctx,
                    caller_id,
                    request_id.clone(),
                    "target replied with no payload (sender closed)".into(),
                )
                .await;
                record_audit(
                    &ctx,
                    caller_id,
                    target_id,
                    caller_owner,
                    &query,
                    havn_db::repo::cross_agent_queries::Outcome::Error,
                    Some("sender closed"),
                    started_at,
                )
                .await;
                return;
            }
            Err(_) => {
                ctx.query_broker.cancel(&request_id).await;
                send_query_timeout(&ctx, caller_id, request_id.clone()).await;
                record_audit(
                    &ctx,
                    caller_id,
                    target_id,
                    caller_owner,
                    &query,
                    havn_db::repo::cross_agent_queries::Outcome::Timeout,
                    Some(&format!("{timeout_secs}s elapsed")),
                    started_at,
                )
                .await;
                return;
            }
        };

    // Forward to caller. Audit row reflects the target's own outcome.
    let audit_outcome = match &response.outcome {
        AgentQueryOutcome::Ok => havn_db::repo::cross_agent_queries::Outcome::Ok,
        AgentQueryOutcome::Error { .. } => havn_db::repo::cross_agent_queries::Outcome::Error,
        AgentQueryOutcome::Timeout => havn_db::repo::cross_agent_queries::Outcome::Timeout,
        _ => havn_db::repo::cross_agent_queries::Outcome::Error,
    };
    let audit_message = match &response.outcome {
        AgentQueryOutcome::Error { message } => Some(message.as_str()),
        _ => None,
    };
    record_audit(
        &ctx,
        caller_id,
        target_id,
        caller_owner,
        &query,
        audit_outcome,
        audit_message,
        started_at,
    )
    .await;

    let result = AgentQueryResult {
        request_id: response.request_id,
        outcome: response.outcome,
        text: response.text,
        transcript: response.transcript,
    };
    if let Err(e) = ctx
        .registry
        .send(caller_id, GatewayToAgent::AgentQueryResult(result))
        .await
    {
        warn!(%caller_id, error = %e, "could not deliver AgentQueryResult to caller");
    }
}

async fn send_query_error(
    ctx: &AgentSocketCtx,
    caller_id: AgentId,
    request_id: String,
    msg: String,
) {
    let _ = ctx
        .registry
        .send(
            caller_id,
            GatewayToAgent::AgentQueryResult(AgentQueryResult {
                request_id,
                outcome: AgentQueryOutcome::Error { message: msg },
                text: String::new(),
                transcript: serde_json::Value::Null,
            }),
        )
        .await;
}

async fn send_query_timeout(ctx: &AgentSocketCtx, caller_id: AgentId, request_id: String) {
    let _ = ctx
        .registry
        .send(
            caller_id,
            GatewayToAgent::AgentQueryResult(AgentQueryResult {
                request_id,
                outcome: AgentQueryOutcome::Timeout,
                text: String::new(),
                transcript: serde_json::Value::Null,
            }),
        )
        .await;
}

#[allow(clippy::too_many_arguments, reason = "audit row is wide on purpose")]
async fn record_audit(
    ctx: &AgentSocketCtx,
    caller_id: AgentId,
    target_id: AgentId,
    caller_user_id: UserId,
    query: &AgentQuery,
    outcome: havn_db::repo::cross_agent_queries::Outcome,
    error_message: Option<&str>,
    started_at: chrono::DateTime<chrono::Utc>,
) {
    let started = started_at.to_rfc3339();
    let finished = chrono::Utc::now().to_rfc3339();
    if let Err(e) = havn_db::repo::cross_agent_queries::record(
        &ctx.db,
        havn_db::repo::cross_agent_queries::NewQuery {
            caller_agent_id: caller_id,
            target_agent_id: target_id,
            caller_user_id,
            prompt: &query.prompt,
            outcome,
            error_message,
            include_transcript: query.include_transcript,
            started_at_rfc3339: &started,
            finished_at_rfc3339: &finished,
        },
    )
    .await
    {
        warn!(error = %e, "cross_agent_queries audit write failed (non-fatal)");
    }

    // Also write a high-level audit_log row so /teams/:id/audit picks
    // it up alongside the existing actions.
    crate::audit::record_agent_action(
        &ctx.db,
        caller_user_id,
        caller_id,
        "agent.queried",
        serde_json::json!({
            "target_agent_id": target_id.to_string(),
            "outcome": match outcome {
                havn_db::repo::cross_agent_queries::Outcome::Ok => "ok",
                havn_db::repo::cross_agent_queries::Outcome::Error => "error",
                havn_db::repo::cross_agent_queries::Outcome::Timeout => "timeout",
            },
        }),
    )
    .await;
}

/// Audit a query that was rejected before dispatch — used when the
/// target_agent_id failed to parse so we can't write a
/// `cross_agent_queries` row (FK to agents would be invalid). The
/// high-level audit log still gets a row with the stringy target.
async fn audit_pre_dispatch(
    ctx: &AgentSocketCtx,
    caller_id: AgentId,
    caller_user_id: UserId,
    target_agent_id_raw: &str,
    reason: &str,
) {
    crate::audit::record_agent_action(
        &ctx.db,
        caller_user_id,
        caller_id,
        "agent.queried",
        serde_json::json!({
            "target_agent_id": target_agent_id_raw,
            "outcome": "error",
            "reason": reason,
        }),
    )
    .await;
}
