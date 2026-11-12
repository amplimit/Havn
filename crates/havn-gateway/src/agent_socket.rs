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
use std::time::Duration;

use anyhow::Context as _;
use havn_core::{AgentId, UserId};
use havn_proto::codec::MAX_FRAME_BYTES;
use havn_proto::{
    AgentQuery, AgentQueryOutcome, AgentQueryResponse, AgentQueryResult, AgentToGateway,
    FrameError, GatewayToAgent, IncomingQuery, LlmOutcome, LlmRequest, LlmResponse, LlmUsage,
    PROTOCOL_VERSION, read_frame, write_frame,
};
use sqlx::SqlitePool;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::agent_registry::{
    AgentRegistry, MAX_INFLIGHT_AGENT_QUERIES, MAX_INFLIGHT_LLM_REQUESTS, SendError, Slot,
};
use crate::cron::CronScheduler;
use crate::llm_proxy::{self, AnthropicMessage, AnthropicRequest};
use crate::policy_resolver;
use crate::webchat::WebChatRouter;
use havn_core::Policy;
use havn_db::repo::agents as agents_repo;

const OUTBOUND_QUEUE_CAPACITY: usize = 64;

/// Longest one frame write may take. A runtime that stops reading its socket
/// is disconnected after this, which also releases every task waiting for
/// room in its outbound queue.
const AGENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Replacements tried for one frame over the size limit (a query result
/// first loses its transcript, then becomes an error); then it is dropped.
const MAX_SUBSTITUTIONS: usize = 2;

/// Longest request id accepted from an agent. Ids are echoed in replies, so
/// an oversized one would make even an error reply unsendable.
const MAX_REQUEST_ID_BYTES: usize = 128;

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
    /// `[[bindings]]` and `[[channels]]` startup snapshots (taken from the
    /// same config as `AppState`'s). Outbound channel messages are only delivered through
    /// accounts bound to the sending agent, to recipients its account's
    /// `dm_policy` allows.
    pub bindings: Arc<Vec<crate::config::ChannelBindingConfig>>,
    pub channels: Arc<std::collections::BTreeMap<String, crate::config::ChannelEntry>>,
    pub cron_scheduler: CronScheduler,
    /// Pending admin RPC senders (`MemoryForget`, `SkillPin`, `Data`). The
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
    pub multi_user: bool,
    /// `[llm] max_tokens_ceiling`: upper bound on `max_tokens` for every
    /// proxied LLM call, whatever the agent's config asks for.
    pub max_tokens_ceiling: u32,
    /// True when agents run under `NamespaceSpawner`, each in its own
    /// `agent-<id>` cgroup. The gateway then accepts a connection for an
    /// agent only from a process in that cgroup. With the unisolated
    /// subprocess spawner there is nothing to check against.
    pub verify_peer_cgroup: bool,
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

/// Read the cgroup of the process on the other end of `stream`, first
/// thing in the connection's task and before the peer has sent anything,
/// so the peer cannot choose when the check runs. The pid from SO_PEERCRED
/// is a number that could be reused once the peer exits. Where the kernel
/// supports SO_PEERPIDFD (Linux 6.5+) the process is pinned with a pidfd
/// and must still be alive after its cgroup is read; if the kernel cannot
/// produce a pidfd for the peer (for example because it already exited),
/// the connection is refused. Only kernels without SO_PEERPIDFD fall back
/// to the bare pid, leaving the short window between connect and this read.
#[cfg(target_os = "linux")]
async fn peer_cgroup(stream: &UnixStream) -> Option<String> {
    let pid = stream.peer_cred().ok()?.pid()?;
    let pin = peer_pidfd(stream);
    if matches!(pin, PeerPidfd::Failed) {
        return None;
    }
    let contents = tokio::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .await
        .ok()?;
    match &pin {
        PeerPidfd::Pinned(fd) if !pidfd_alive(fd) => None,
        _ => Some(contents),
    }
}

#[cfg(not(target_os = "linux"))]
async fn peer_cgroup(_stream: &UnixStream) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
enum PeerPidfd {
    Pinned(std::os::fd::OwnedFd),
    /// The kernel (or this architecture) has no SO_PEERPIDFD.
    Unsupported,
    /// SO_PEERPIDFD exists but returned no pidfd for this peer.
    Failed,
}

#[cfg(target_os = "linux")]
fn peer_pidfd(stream: &UnixStream) -> PeerPidfd {
    // libc does not export SO_PEERPIDFD; 77 is the asm-generic value used
    // by x86_64 and aarch64, the architectures havn is built for.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
        const SO_PEERPIDFD: libc::c_int = 77;
        let mut fd: libc::c_int = -1;
        let Ok(mut len) = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()) else {
            return PeerPidfd::Failed;
        };
        // SAFETY: `fd` and `len` are valid for writes of the sizes passed,
        // and the socket fd is borrowed from a live stream for the call.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_PEERPIDFD,
                (&raw mut fd).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            let unsupported =
                std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOPROTOOPT);
            return if unsupported {
                PeerPidfd::Unsupported
            } else {
                PeerPidfd::Failed
            };
        }
        if fd < 0 {
            return PeerPidfd::Failed;
        }
        // SAFETY: on success getsockopt returned a new pidfd that nothing
        // else owns, so taking ownership here closes it exactly once.
        PeerPidfd::Pinned(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = stream;
        PeerPidfd::Unsupported
    }
}

#[cfg(target_os = "linux")]
fn pidfd_alive(pidfd: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd as _;
    // A pidfd becomes readable when its process exits. poll needs no
    // permission and is not filtered by seccomp profiles that block
    // pidfd_send_signal, which would otherwise make every agent look dead.
    let mut entry = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `entry` is one valid pollfd and the timeout is zero.
    let rc = unsafe { libc::poll(&raw mut entry, 1, 0) };
    rc == 0
}

fn peer_cgroup_is_agents(contents: Option<&str>, agent_id: AgentId) -> bool {
    #[cfg(target_os = "linux")]
    {
        contents.is_some_and(|c| cgroup_belongs_to_agent(c, agent_id))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (contents, agent_id);
        false
    }
}

/// Parse `/proc/<pid>/cgroup` (cgroup v2: a single `0::<path>` line, shown
/// relative to the gateway's cgroup root, the same root the spawner
/// creates `havn.slice` under) and require exactly the agent's cgroup.
/// Comparing the whole path matters: a cgroup the agent creates below its
/// own, even one named after another agent, does not match.
#[cfg(target_os = "linux")]
fn cgroup_belongs_to_agent(contents: &str, agent_id: AgentId) -> bool {
    let expected = format!("/{}/agent-{agent_id}", havn_spawner::cgroup::SLICE);
    contents
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .any(|path| path.trim_end() == expected)
}

/// At most 64 chars of a peer-supplied string, for logging.
fn clip(s: &str) -> &str {
    s.char_indices().nth(64).map_or(s, |(end, _)| &s[..end])
}

async fn handle_connection(stream: UnixStream, ctx: AgentSocketCtx) -> anyhow::Result<()> {
    // Every sandbox sees the same socket, so the agent id in Hello is only
    // a claim. The peer's process comes from the kernel and cannot be
    // forged; look up its cgroup now, before reading anything from it.
    let peer_cgroup = if ctx.verify_peer_cgroup {
        peer_cgroup(&stream).await
    } else {
        None
    };
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
        // Both values come from the peer before anything is verified.
        let (agent_id_str, version) = (clip(&agent_id_str), clip(&version));
        warn!(
            agent_id = ?agent_id_str,
            agent_version = ?version,
            gateway_version = PROTOCOL_VERSION,
            "refusing agent: runtime protocol version differs from the gateway's; \
             install the havn-runtime that ships with this gateway"
        );
        anyhow::bail!("protocol version mismatch (agent {version:?}, gateway {PROTOCOL_VERSION})");
    }

    let agent_id = AgentId::from_str(&agent_id_str)
        .with_context(|| format!("parsing agent_id {agent_id_str:?}"))?;

    if ctx.verify_peer_cgroup && !peer_cgroup_is_agents(peer_cgroup.as_deref(), agent_id) {
        warn!(
            %agent_id,
            peer_cgroup = peer_cgroup.as_deref().map(str::trim_end),
            "refusing agent connection: the connecting process is not in that agent's cgroup"
        );
        anyhow::bail!("agent {agent_id}: connecting process is not in the agent's cgroup");
    }

    // Attach before sending Welcome, so a second connection for an agent
    // that is already connected is refused before it learns anything.
    let (tx, rx) = mpsc::channel::<GatewayToAgent>(OUTBOUND_QUEUE_CAPACITY);
    ctx.registry
        .attach(agent_id, tx.clone())
        .await
        .with_context(|| format!("refusing connection for agent {agent_id}"))?;

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
    let (policy, model, max_tokens, agent_name): (
        Policy,
        Option<String>,
        Option<u32>,
        Option<String>,
    ) = match agents_repo::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => {
            let model = agent
                .config
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let max_tokens = agent
                .config
                .get("max_tokens")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0);
            let name = Some(agent.name.clone());
            // for_session walks per-agent override → user/team
            // merged → default. Same chain used at spawn time so
            // cgroup limits and tool registry stay consistent
            // (spec §6.3).
            (
                policy_resolver::for_session(&ctx.db, &agent, ctx.multi_user).await,
                model,
                max_tokens,
                name,
            )
        }
        Ok(None) => {
            warn!(%agent_id, "agent not found at handshake; sending default policy");
            (Policy::default(), None, None, None)
        }
        Err(e) => {
            warn!(%agent_id, error = %e, "policy lookup failed; sending default policy");
            (Policy::default(), None, None, None)
        }
    };

    let session_token = Uuid::now_v7().to_string();
    let welcome = write_frame(
        &mut write,
        &GatewayToAgent::Welcome {
            session_token: session_token.clone(),
            policy,
            agent_name,
            model,
            max_tokens,
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
    .await;
    if let Err(e) = welcome {
        ctx.registry.detach(agent_id, &tx).await;
        return Err(e).context("sending Welcome");
    }

    info!(%agent_id, "agent connected");

    // 4. Writer task: drain mpsc → wire
    let disconnect = ctx
        .registry
        .disconnect_signal(agent_id, &tx)
        .await
        .unwrap_or_default();
    let mut writer_task = tokio::spawn(run_writer(write, rx, agent_id, ctx.clone(), disconnect));

    // 5. Reader loop; the connection also ends when the writer gives up.
    let reader_result = tokio::select! {
        result = run_reader(&mut reader, agent_id, &ctx) => Some(result),
        _ = &mut writer_task => None,
    };

    // 6. Cleanup
    ctx.registry.detach(agent_id, &tx).await;
    let Some(result) = reader_result else {
        warn!(%agent_id, "agent writer stopped; connection closed");
        return Ok(());
    };
    writer_task.abort();
    let _ = writer_task.await;

    if let Err(e) = result {
        warn!(%agent_id, error = ?e, "agent reader loop terminated");
    } else {
        info!(%agent_id, "agent disconnected cleanly");
    }
    Ok(())
}

/// Write queued frames until the queue closes, a write fails or stalls, or
/// the registry asks for this agent to be disconnected.
async fn run_writer<W>(
    write: W,
    rx: mpsc::Receiver<GatewayToAgent>,
    agent_id: AgentId,
    ctx: AgentSocketCtx,
    disconnect: Arc<Notify>,
) where
    W: AsyncWrite + Unpin,
{
    let disconnected = disconnect.notified();
    tokio::select! {
        () = write_frames(write, rx, agent_id, &ctx) => {}
        () = disconnected => {}
    }
}

async fn write_frames<W>(
    mut write: W,
    mut rx: mpsc::Receiver<GatewayToAgent>,
    agent_id: AgentId,
    ctx: &AgentSocketCtx,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        let mut next = Some(frame);
        let mut substitutions = 0;
        while let Some(frame) = next.take() {
            match tokio::time::timeout(AGENT_WRITE_TIMEOUT, write_frame(&mut write, &frame)).await {
                Ok(Ok(())) => {}
                // Rejected before anything was written: the stream is intact.
                Ok(Err(FrameError::FrameTooLarge(size))) if substitutions < MAX_SUBSTITUTIONS => {
                    substitutions += 1;
                    next = oversized_reply(agent_id, frame, size, substitutions, ctx).await;
                }
                Ok(Err(FrameError::FrameTooLarge(size))) => {
                    error!(%agent_id, size, "replacement frame is still over the size limit; dropping it");
                }
                Ok(Err(e)) => {
                    error!(%agent_id, error = %e, "agent writer failed");
                    return;
                }
                Err(_) => {
                    warn!(
                        %agent_id,
                        timeout_s = AGENT_WRITE_TIMEOUT.as_secs(),
                        "agent is not reading its socket; disconnecting"
                    );
                    return;
                }
            }
        }
    }
}

/// `frame` is over [`MAX_FRAME_BYTES`] and cannot be sent. Fail whoever is
/// waiting on it; returns the smaller frame to send in its place, if any.
/// `attempt` counts the replacements tried for this frame, from 1.
async fn oversized_reply(
    agent_id: AgentId,
    frame: GatewayToAgent,
    size: usize,
    attempt: usize,
    ctx: &AgentSocketCtx,
) -> Option<GatewayToAgent> {
    let limit = format!("{size} bytes exceeds the {MAX_FRAME_BYTES}-byte frame limit");
    match frame {
        GatewayToAgent::LlmResponse(resp) => {
            warn!(%agent_id, request_id = %resp.request_id, size, "LlmResponse too large; replying with an error");
            Some(GatewayToAgent::LlmResponse(LlmResponse {
                request_id: resp.request_id,
                outcome: LlmOutcome::Error {
                    message: format!("provider response of {limit}"),
                },
                provider_response: serde_json::Value::Null,
                usage: resp.usage,
            }))
        }
        GatewayToAgent::AgentQueryResult(result)
            if attempt == 1 && !result.transcript.is_null() =>
        {
            warn!(%agent_id, request_id = %result.request_id, size, "AgentQueryResult too large; dropping its transcript");
            Some(GatewayToAgent::AgentQueryResult(AgentQueryResult {
                transcript: serde_json::Value::String(format!(
                    "transcript omitted: result of {limit}"
                )),
                ..result
            }))
        }
        GatewayToAgent::AgentQueryResult(result) => {
            warn!(%agent_id, request_id = %result.request_id, size, "AgentQueryResult too large; replying with an error");
            Some(GatewayToAgent::AgentQueryResult(AgentQueryResult {
                request_id: result.request_id,
                outcome: AgentQueryOutcome::Error {
                    message: format!("answer of {limit}"),
                },
                text: String::new(),
                transcript: serde_json::Value::Null,
            }))
        }
        GatewayToAgent::IncomingQuery(query) => {
            warn!(%agent_id, size, "IncomingQuery too large; failing the query");
            ctx.query_broker
                .resolve(
                    agent_id,
                    AgentQueryResponse {
                        request_id: query.request_id,
                        outcome: AgentQueryOutcome::Error {
                            message: format!("query of {limit}"),
                        },
                        text: String::new(),
                        transcript: serde_json::Value::Null,
                    },
                )
                .await;
            None
        }
        GatewayToAgent::InboundMessage(msg) => {
            warn!(%agent_id, size, "InboundMessage too large; telling the sender");
            let notice = "This message is too large for the agent to receive.".to_string();
            if let Some((channel, account_id, sender_id)) =
                crate::channel_id_codec::decode(&msg.channel_id)
            {
                let _ = ctx
                    .channel_router
                    .deliver(
                        channel,
                        account_id,
                        sender_id,
                        None,
                        havn_proto::channel::ChannelMessageContent::Text { text: notice },
                    )
                    .await;
            } else {
                let _ = ctx
                    .webchat_router
                    .deliver(
                        agent_id,
                        havn_core::OutboundMessage {
                            channel_id: msg.channel_id,
                            content: havn_core::MessageContent::Text { text: notice },
                            reply_to: None,
                        },
                    )
                    .await;
            }
            None
        }
        GatewayToAgent::MemoryForgetRequest(havn_proto::MemoryForgetRequest {
            request_id, ..
        })
        | GatewayToAgent::SkillPinRequest(havn_proto::SkillPinRequest { request_id, .. })
        | GatewayToAgent::DataRequest(havn_proto::DataRequest { request_id, .. }) => {
            warn!(%agent_id, size, "admin request too large; failing it");
            ctx.admin_rpc
                .resolve(
                    agent_id,
                    &request_id,
                    havn_proto::AdminOutcome::Error {
                        message: format!("request of {limit}"),
                    },
                )
                .await;
            None
        }
        _ => {
            error!(%agent_id, size, "dropping an outbound frame over the size limit");
            None
        }
    }
}

/// Reply to a request refused before it was handled. A queue too full to
/// take even that means the agent is not reading, so it is disconnected.
async fn refuse(ctx: &AgentSocketCtx, agent_id: AgentId, refusal: GatewayToAgent) {
    match ctx.registry.try_send(agent_id, refusal).await {
        Ok(()) => {}
        Err(SendError::Stalled(_)) => {
            warn!(
                %agent_id,
                "outbound queue is full, so a refusal cannot be sent; disconnecting the agent"
            );
            ctx.registry.disconnect(agent_id, None).await;
        }
        Err(e) => debug!(%agent_id, error = %e, "could not deliver a refusal"),
    }
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
                    if let Err(refusal) = crate::channel_router::outbound_permitted(
                        &ctx.bindings,
                        &ctx.channels,
                        agent_id,
                        channel,
                        account_id,
                        sender_id,
                    ) {
                        warn!(
                            %agent_id,
                            channel,
                            account = account_id,
                            reason = refusal.as_str(),
                            "dropping channel-adapter outbound"
                        );
                        continue;
                    }
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
                } else if let Err(e) = ctx.webchat_router.deliver(agent_id, msg).await {
                    warn!(%agent_id, error = %e, "could not deliver outbound to webchat");
                }
            }
            AgentToGateway::LlmRequest(req) => {
                if req.request_id.len() > MAX_REQUEST_ID_BYTES {
                    warn!(%agent_id, len = req.request_id.len(), "dropping LlmRequest with an oversized request_id");
                    continue;
                }
                let Some(permit) = ctx.registry.try_reserve(agent_id, Slot::Llm).await else {
                    warn!(%agent_id, limit = MAX_INFLIGHT_LLM_REQUESTS, "too many LLM requests in flight; refusing one");
                    let refusal = GatewayToAgent::LlmResponse(LlmResponse {
                        request_id: req.request_id,
                        outcome: LlmOutcome::Error {
                            message: format!(
                                "this agent already has {MAX_INFLIGHT_LLM_REQUESTS} LLM requests in flight; retry when one finishes"
                            ),
                        },
                        provider_response: serde_json::Value::Null,
                        usage: None,
                    });
                    refuse(ctx, agent_id, refusal).await;
                    continue;
                };
                let ctx_cloned = ctx.clone();
                tokio::spawn(async move {
                    handle_llm_request(agent_id, req, ctx_cloned).await;
                    drop(permit);
                });
            }
            AgentToGateway::LivenessPing => {
                debug!(%agent_id, "liveness ping");
            }
            AgentToGateway::CronResult(result) => {
                let job_id = result.job_id.clone();
                match ctx.cron_scheduler.record_result(agent_id, result).await {
                    Ok(()) => {}
                    Err(e @ crate::cron::CronError::NotPending(_)) => {
                        warn!(%agent_id, %job_id, error = %e, "rejecting unsolicited CronResult");
                    }
                    Err(e @ crate::cron::CronError::ExpiredRun(_)) => {
                        info!(%agent_id, %job_id, error = %e, "dropping CronResult: result for an expired run");
                    }
                    Err(e) => {
                        warn!(%agent_id, %job_id, error = %e, "cron result recording failed");
                    }
                }
            }
            AgentToGateway::MemoryForgetResponse(resp) => {
                ctx.admin_rpc
                    .resolve(agent_id, &resp.request_id, resp.outcome)
                    .await;
            }
            AgentToGateway::SkillPinResponse(resp) => {
                ctx.admin_rpc
                    .resolve(agent_id, &resp.request_id, resp.outcome)
                    .await;
            }
            AgentToGateway::DataResponse(resp) => {
                ctx.admin_rpc
                    .resolve(agent_id, &resp.request_id, resp.outcome)
                    .await;
            }
            AgentToGateway::AgentQuery(query) => {
                // Caller-side initiation. Spawn so a slow target doesn't
                // block the caller's reader loop; at most
                // MAX_INFLIGHT_AGENT_QUERIES per caller, each bounded by
                // the timeout enforced inside the handler.
                if query.request_id.len() > MAX_REQUEST_ID_BYTES {
                    warn!(%agent_id, len = query.request_id.len(), "dropping AgentQuery with an oversized request_id");
                    continue;
                }
                let Some(permit) = ctx.registry.try_reserve(agent_id, Slot::Query).await else {
                    warn!(%agent_id, limit = MAX_INFLIGHT_AGENT_QUERIES, "too many agent queries in flight; refusing one");
                    let refusal = GatewayToAgent::AgentQueryResult(AgentQueryResult {
                        request_id: query.request_id,
                        outcome: AgentQueryOutcome::Error {
                            message: format!(
                                "this agent already has {MAX_INFLIGHT_AGENT_QUERIES} queries in flight; retry when one finishes"
                            ),
                        },
                        text: String::new(),
                        transcript: serde_json::Value::Null,
                    });
                    refuse(ctx, agent_id, refusal).await;
                    continue;
                };
                let ctx_cloned = ctx.clone();
                tokio::spawn(async move {
                    handle_agent_query(agent_id, query, ctx_cloned).await;
                    drop(permit);
                });
            }
            AgentToGateway::AgentQueryResponse(resp) => {
                // Callee-side reply — match against pending broker entry
                // so the caller-side handler can wake.
                ctx.query_broker.resolve(agent_id, resp).await;
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
    let agent = match havn_db::repo::agents::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => agent,
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

    let owner_id = agent.owner_id;
    let policy = policy_resolver::for_session(&ctx.db, &agent, ctx.multi_user).await;
    let egress_allowed = policy.network_policy.egress_allowed;

    let response = match build_anthropic_request(&req) {
        Ok(ar) => {
            if !llm_proxy::model_allowed(&policy.allowed_models, &ar.model) {
                warn!(%agent_id, model = ?ar.model, "refusing LLM request for a model the policy does not allow");
                let _ = ctx
                    .registry
                    .send(
                        agent_id,
                        GatewayToAgent::LlmResponse(LlmResponse {
                            request_id,
                            outcome: LlmOutcome::Error {
                                message: format!(
                                    "model {:?} is not allowed by this agent's policy",
                                    ar.model
                                ),
                            },
                            provider_response: serde_json::Value::Null,
                            usage: None,
                        }),
                    )
                    .await;
                return;
            }
            // An agent without egress must not get the provider to fetch
            // URLs for it (a way to send data out through the query string).
            if !egress_allowed && let Some(reason) = llm_proxy::provider_side_fetch(&ar) {
                warn!(%agent_id, reason, "refusing LLM request from an agent without egress");
                let _ = ctx
                    .registry
                    .send(
                        agent_id,
                        GatewayToAgent::LlmResponse(LlmResponse {
                            request_id,
                            outcome: LlmOutcome::Error {
                                message: format!(
                                    "{reason}; this agent's policy does not allow network egress"
                                ),
                            },
                            provider_response: serde_json::Value::Null,
                            usage: None,
                        }),
                    )
                    .await;
                return;
            }
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
            let billing = llm_proxy::Billing {
                user_id: billing_user,
                agent_id: Some(agent_id),
                agent_team: agent.team_id,
                budget: policy.budget.clone(),
            };
            match llm_proxy::complete_with_provider(
                &ctx.db,
                &ctx.http,
                ring,
                &billing,
                &provider,
                &ar,
                ctx.max_tokens_ceiling,
            )
            .await
            {
                Ok(resp) => {
                    let usage = LlmUsage {
                        input_tokens: resp.usage.total_input_tokens(),
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
        .unwrap_or(8192);
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
    let caller_agent = match havn_db::repo::agents::find_by_id(&ctx.db, caller_id).await {
        Ok(Some(a)) => a,
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

    let caller_owner = caller_agent.owner_id;

    // The runtime only offers the agent_query tool when the policy allows
    // it, but the frame comes from inside the sandbox, so the gateway has
    // to enforce the same rule itself.
    let caller_policy = policy_resolver::for_session(&ctx.db, &caller_agent, ctx.multi_user).await;
    if !caller_policy.permissions.can_query_other_agents {
        send_query_error(
            &ctx,
            caller_id,
            request_id.clone(),
            "querying other agents is not permitted by this agent's policy".into(),
        )
        .await;
        audit_pre_dispatch(
            &ctx,
            caller_id,
            caller_owner,
            &query.target_agent_id,
            "permission_denied",
        )
        .await;
        return;
    }

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
    // The target sees a gateway-chosen id, not the caller's, and only the
    // target may answer it.
    let (dispatch_id, rx) = ctx.query_broker.register(target_id).await;
    let incoming = IncomingQuery {
        request_id: dispatch_id.clone(),
        caller_agent_id: caller_id.to_string(),
        billing_user_id: caller_owner.to_string(),
        prompt: query.prompt.clone(),
        include_transcript: query.include_transcript,
    };
    if let Err(e) = ctx
        .registry
        .send(target_id, GatewayToAgent::IncomingQuery(incoming))
        .await
    {
        ctx.query_broker.cancel(&dispatch_id).await;
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
                // The broker drops a sender only by resolving or cancelling
                // it, and dispatch ids are unique, so this is not expected;
                // report it as an error rather than waiting.
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
                ctx.query_broker.cancel(&dispatch_id).await;
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
        request_id,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use havn_db::connect_in_memory;
    use havn_db::repo::agents::{NewAgent, create as create_agent};
    use havn_db::repo::users::{NewUser, create as create_user};
    use havn_proto::AgentQueryOutcome;
    use havn_spawner::AgentHandle;
    use std::time::Duration;

    async fn ctx_with(multi_user: bool, verify_peer_cgroup: bool) -> AgentSocketCtx {
        let db = connect_in_memory().await.expect("db");
        let registry = AgentRegistry::new();
        let webchat_router = WebChatRouter::new();
        AgentSocketCtx {
            cron_scheduler: CronScheduler::new(
                db.clone(),
                registry.clone(),
                webchat_router.clone(),
                std::env::temp_dir(),
            ),
            registry,
            db,
            http: reqwest::Client::new(),
            webchat_router,
            channel_router: crate::channel_router::ChannelRouter::new(),
            bindings: Arc::new(Vec::new()),
            channels: Arc::new(std::collections::BTreeMap::new()),
            admin_rpc: crate::admin_rpc::AdminRpc::new(),
            query_broker: crate::query_broker::QueryBroker::new(),
            embedding_config: Arc::new(arc_swap::ArcSwap::from_pointee(
                serde_json::json!({"provider": "disabled"}),
            )),
            multi_user,
            max_tokens_ceiling: crate::config::DEFAULT_MAX_TOKENS_CEILING,
            verify_peer_cgroup,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
            keyring: None,
        }
    }

    async fn connected_agent(
        ctx: &AgentSocketCtx,
        owner: UserId,
        name: &str,
        config: serde_json::Value,
    ) -> (AgentId, mpsc::Receiver<GatewayToAgent>) {
        let agent = create_agent(
            &ctx.db,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name,
                config,
            },
        )
        .await
        .expect("create agent");
        ctx.registry
            .register(AgentHandle {
                agent_id: agent.id,
                pid: 1,
            })
            .await;
        let (tx, rx) = mpsc::channel(8);
        ctx.registry.attach(agent.id, tx).await.expect("attach");
        (agent.id, rx)
    }

    fn query_to(target: AgentId) -> AgentQuery {
        AgentQuery {
            request_id: "q1".into(),
            target_agent_id: target.to_string(),
            prompt: "show me your conversation".into(),
            timeout_seconds: 1,
            include_transcript: true,
        }
    }

    fn policy_allowing_queries() -> serde_json::Value {
        let mut p = Policy::default();
        p.permissions.can_query_other_agents = true;
        serde_json::json!({ "policy": p })
    }

    #[tokio::test]
    async fn agent_query_is_refused_when_policy_forbids_it() {
        let ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (caller, mut caller_rx) =
            connected_agent(&ctx, owner, "caller", serde_json::json!({})).await;
        let (target, mut target_rx) =
            connected_agent(&ctx, owner, "target", serde_json::json!({})).await;

        handle_agent_query(caller, query_to(target), ctx.clone()).await;

        match caller_rx.try_recv().expect("caller gets a result") {
            GatewayToAgent::AgentQueryResult(r) => {
                assert!(
                    matches!(r.outcome, AgentQueryOutcome::Error { .. }),
                    "{:?}",
                    r.outcome
                );
            }
            other => panic!("unexpected frame {other:?}"),
        }
        assert!(
            target_rx.try_recv().is_err(),
            "the target must not receive the query"
        );
    }

    #[tokio::test]
    async fn agent_query_is_dispatched_when_policy_allows_it() {
        let ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (caller, _caller_rx) =
            connected_agent(&ctx, owner, "caller", policy_allowing_queries()).await;
        let (target, mut target_rx) =
            connected_agent(&ctx, owner, "target", serde_json::json!({})).await;

        tokio::spawn(handle_agent_query(caller, query_to(target), ctx.clone()));

        let frame = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
            .await
            .expect("dispatch within 5s")
            .expect("frame");
        assert!(
            matches!(frame, GatewayToAgent::IncomingQuery(_)),
            "{frame:?}"
        );
    }

    fn llm_request(request_id: &str) -> LlmRequest {
        LlmRequest {
            request_id: request_id.into(),
            model: "claude-test".into(),
            messages: serde_json::json!([{ "role": "user", "content": "hi" }]),
            options: serde_json::json!({}),
            billing_user_id: None,
        }
    }

    fn refused(outcome: &LlmOutcome) -> bool {
        matches!(outcome, LlmOutcome::Error { message } if message.contains("in flight"))
    }

    #[tokio::test]
    async fn llm_requests_over_the_inflight_cap_are_refused_at_once() {
        let ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, mut rx) = connected_agent(&ctx, owner, "busy", serde_json::json!({})).await;
        let mut bytes = Vec::new();
        for i in 0..=MAX_INFLIGHT_LLM_REQUESTS {
            write_frame(
                &mut bytes,
                &AgentToGateway::LlmRequest(llm_request(&format!("r{i}"))),
            )
            .await
            .expect("encode");
        }

        // Hold the pool's only connection: every accepted request then
        // blocks on its database lookup and stays in flight.
        let held = ctx.db.acquire().await.expect("connection");
        run_reader(&mut BufReader::new(bytes.as_slice()), agent, &ctx)
            .await
            .expect("reader");

        let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the refusal is immediate")
            .expect("frame");
        match frame {
            GatewayToAgent::LlmResponse(r) => {
                assert_eq!(r.request_id, format!("r{MAX_INFLIGHT_LLM_REQUESTS}"));
                assert!(refused(&r.outcome), "{:?}", r.outcome);
            }
            other => panic!("unexpected frame {other:?}"),
        }

        drop(held);
        for _ in 0..MAX_INFLIGHT_LLM_REQUESTS {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("accepted requests finish")
                .expect("frame")
            {
                GatewayToAgent::LlmResponse(r) => assert!(!refused(&r.outcome), "{:?}", r.outcome),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    fn spawn_writer<W>(
        ctx: &AgentSocketCtx,
        write: W,
        agent_id: AgentId,
    ) -> (
        mpsc::Sender<GatewayToAgent>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    )
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(8);
        let disconnect = Arc::new(Notify::new());
        let writer = tokio::spawn(run_writer(
            write,
            rx,
            agent_id,
            ctx.clone(),
            Arc::clone(&disconnect),
        ));
        (tx, disconnect, writer)
    }

    #[tokio::test]
    async fn oversized_frames_fail_whoever_waits_on_them() {
        let ctx = ctx_with(false, false).await;
        let agent_id = AgentId::new();
        let (dispatch_id, query_rx) = ctx.query_broker.register(agent_id).await;
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (tx, _, writer) = spawn_writer(&ctx, server, agent_id);
        let big = "x".repeat(MAX_FRAME_BYTES);

        let frames = [
            GatewayToAgent::IncomingQuery(IncomingQuery {
                request_id: dispatch_id,
                caller_agent_id: AgentId::new().to_string(),
                billing_user_id: "u".into(),
                prompt: big.clone(),
                include_transcript: false,
            }),
            GatewayToAgent::LlmResponse(LlmResponse {
                request_id: "llm".into(),
                outcome: LlmOutcome::Ok,
                provider_response: serde_json::json!({ "content": big }),
                usage: None,
            }),
            GatewayToAgent::AgentQueryResult(AgentQueryResult {
                request_id: "q".into(),
                outcome: AgentQueryOutcome::Ok,
                text: "answer".into(),
                transcript: serde_json::json!([big]),
            }),
        ];
        for frame in frames {
            tx.send(frame).await.expect("queue");
        }
        drop(tx);

        let mut reader = BufReader::new(client);
        match read_frame::<_, GatewayToAgent>(&mut reader).await {
            Ok(Some(GatewayToAgent::LlmResponse(r))) => {
                assert_eq!(r.request_id, "llm");
                assert!(
                    matches!(r.outcome, LlmOutcome::Error { .. }),
                    "{:?}",
                    r.outcome
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        match read_frame::<_, GatewayToAgent>(&mut reader).await {
            Ok(Some(GatewayToAgent::AgentQueryResult(r))) => {
                assert_eq!(r.request_id, "q");
                assert!(
                    matches!(r.outcome, AgentQueryOutcome::Ok),
                    "{:?}",
                    r.outcome
                );
                assert_eq!(r.text, "answer");
                assert!(r.transcript.is_string(), "transcript is replaced by a note");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            read_frame::<_, GatewayToAgent>(&mut reader).await,
            Ok(None)
        ));
        writer.await.expect("writer");

        let answer = tokio::time::timeout(Duration::from_secs(5), query_rx)
            .await
            .expect("the caller is not left waiting")
            .expect("answer");
        assert!(
            matches!(answer.outcome, AgentQueryOutcome::Error { .. }),
            "{:?}",
            answer.outcome
        );
    }

    #[tokio::test]
    async fn replacements_that_are_still_too_large_are_dropped() {
        let ctx = ctx_with(false, false).await;
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (tx, _, writer) = spawn_writer(&ctx, server, AgentId::new());
        let big = "x".repeat(MAX_FRAME_BYTES);

        // The error reply echoes the id, so it is as large as the original.
        tx.send(GatewayToAgent::LlmResponse(LlmResponse {
            request_id: big.clone(),
            outcome: LlmOutcome::Ok,
            provider_response: serde_json::Value::Null,
            usage: None,
        }))
        .await
        .expect("queue");
        // Dropping the transcript is not enough; the error reply is.
        tx.send(GatewayToAgent::AgentQueryResult(AgentQueryResult {
            request_id: "q".into(),
            outcome: AgentQueryOutcome::Ok,
            text: big.clone(),
            transcript: serde_json::json!([big]),
        }))
        .await
        .expect("queue");
        drop(tx);

        let mut reader = BufReader::new(client);
        let frame = tokio::time::timeout(
            Duration::from_secs(10),
            read_frame::<_, GatewayToAgent>(&mut reader),
        )
        .await
        .expect("the writer does not spin");
        match frame {
            Ok(Some(GatewayToAgent::AgentQueryResult(r))) => {
                assert_eq!(r.request_id, "q");
                assert!(
                    matches!(r.outcome, AgentQueryOutcome::Error { .. }),
                    "{:?}",
                    r.outcome
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            read_frame::<_, GatewayToAgent>(&mut reader).await,
            Ok(None)
        ));
        writer.await.expect("writer");
    }

    #[tokio::test]
    async fn writer_stops_when_the_registry_disconnects_the_agent() {
        let ctx = ctx_with(false, false).await;
        let (_client, server) = tokio::io::duplex(64);
        let (tx, disconnect, writer) = spawn_writer(&ctx, server, AgentId::new());
        tokio::task::yield_now().await;

        disconnect.notify_waiters();

        tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("the writer stops")
            .expect("join");
        assert!(tx.is_closed());
    }

    #[tokio::test]
    async fn writer_disconnects_an_agent_that_stops_reading() {
        let ctx = ctx_with(false, false).await;
        tokio::time::pause();
        let (_client, server) = tokio::io::duplex(64);
        let (tx, _, writer) = spawn_writer(&ctx, server, AgentId::new());
        tx.send(GatewayToAgent::LlmResponse(LlmResponse {
            request_id: "r".into(),
            outcome: LlmOutcome::Ok,
            provider_response: serde_json::json!({ "content": "x".repeat(1024) }),
            usage: None,
        }))
        .await
        .expect("queue");

        tokio::time::timeout(AGENT_WRITE_TIMEOUT * 2, writer)
            .await
            .expect("the writer gives up on the agent")
            .expect("join");
        assert!(tx.is_closed(), "tasks waiting on the queue are released");
    }

    async fn registered_agent(ctx: &AgentSocketCtx) -> AgentId {
        let agent_id = AgentId::new();
        ctx.registry
            .register(AgentHandle { agent_id, pid: 1 })
            .await;
        agent_id
    }

    /// Connect over a socket pair, send Hello for `agent_id`, and return
    /// the gateway's verdict plus whatever it wrote back.
    async fn hello(
        ctx: &AgentSocketCtx,
        agent_id: AgentId,
    ) -> (anyhow::Result<()>, Option<GatewayToAgent>) {
        hello_with_version(ctx, agent_id, PROTOCOL_VERSION).await
    }

    async fn hello_with_version(
        ctx: &AgentSocketCtx,
        agent_id: AgentId,
        version: &str,
    ) -> (anyhow::Result<()>, Option<GatewayToAgent>) {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let gateway = tokio::spawn(handle_connection(server, ctx.clone()));
        let (client_read, mut client_write) = client.into_split();
        write_frame(
            &mut client_write,
            &AgentToGateway::Hello {
                agent_id: agent_id.to_string(),
                version: version.into(),
            },
        )
        .await
        .expect("send hello");
        let mut reader = BufReader::new(client_read);
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<_, GatewayToAgent>(&mut reader),
        )
        .await
        .expect("gateway answers or closes")
        .ok()
        .flatten();
        drop(client_write);
        drop(reader);
        let verdict = gateway.await.expect("join");
        (verdict, reply)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_path_must_name_the_agent() {
        let id = AgentId::new();
        let other = AgentId::new();
        let own = format!("0::/havn.slice/agent-{id}\n");
        assert!(cgroup_belongs_to_agent(&own, id));
        assert!(!cgroup_belongs_to_agent(
            &format!("0::/elsewhere/havn.slice/agent-{id}\n"),
            id
        ));
        assert!(
            !cgroup_belongs_to_agent(&format!("0::/havn.slice/agent-{other}/agent-{id}\n"), id),
            "a cgroup created inside another agent's cgroup must not match"
        );
        assert!(!cgroup_belongs_to_agent(&own, other));
        assert!(!cgroup_belongs_to_agent(
            &format!("0::/havn.slice/agent-{id}-x\n"),
            id
        ));
        assert!(!cgroup_belongs_to_agent(
            &format!("0::/havn.slice/agent-{id}/nested\n"),
            id
        ));
        assert!(!cgroup_belongs_to_agent(
            &format!("1:name=systemd:/havn.slice/agent-{id}\n"),
            id
        ));
        assert!(!cgroup_belongs_to_agent("0::/user.slice\n", id));
        assert!(!cgroup_belongs_to_agent("", id));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn peer_cgroup_reads_the_connecting_process() {
        let (ours, theirs) = UnixStream::pair().expect("socket pair");
        let seen = peer_cgroup(&ours).await.expect("peer cgroup");
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("own cgroup");
        assert_eq!(seen, own);
        // SO_PEERPIDFD needs Linux 6.5+; older kernels fall back to the pid.
        if let PeerPidfd::Pinned(pidfd) = peer_pidfd(&ours) {
            assert!(pidfd_alive(&pidfd));
        }
        drop(theirs);
    }

    /// A peer that connected and has since exited and been reaped must not
    /// be checked through its (reusable) pid number.
    #[cfg(target_os = "linux")]
    #[test]
    fn peer_that_already_exited_is_not_accepted() {
        use std::os::unix::net::UnixListener as StdListener;
        let dir = std::env::temp_dir().join(format!("havn-peer-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("s.sock");
        let listener = StdListener::bind(&path).expect("bind");

        // A short-lived child process connects and exits; the parent reaps
        // it before looking at the connection.
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg("import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])")
            .arg(&path)
            .status();
        let Ok(status) = status else {
            eprintln!("python3 not available; skipping");
            return;
        };
        assert!(status.success());
        let (accepted, _) = listener.accept().expect("accept");
        accepted.set_nonblocking(true).expect("nonblocking");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let seen = rt.block_on(async {
            let stream = UnixStream::from_std(accepted).expect("tokio stream");
            peer_cgroup(&stream).await
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.is_none(), "an exited peer must be refused: {seen:?}");
    }

    #[tokio::test]
    async fn connection_from_outside_the_agents_cgroup_is_refused() {
        let ctx = ctx_with(false, true).await;
        let victim = registered_agent(&ctx).await;

        let (verdict, reply) = hello(&ctx, victim).await;

        assert!(verdict.is_err(), "the claim must be rejected");
        assert!(reply.is_none(), "no Welcome may be sent: {reply:?}");
        assert!(!ctx.registry.is_connected(victim).await);
    }

    #[tokio::test]
    async fn second_connection_cannot_take_over_a_connected_agent() {
        let ctx = ctx_with(false, false).await;
        let agent = registered_agent(&ctx).await;
        let (live, mut live_rx) = mpsc::channel::<GatewayToAgent>(8);
        ctx.registry.attach(agent, live).await.expect("attach");

        let (verdict, reply) = hello(&ctx, agent).await;

        assert!(verdict.is_err(), "the second connection must be rejected");
        assert!(reply.is_none(), "no Welcome may be sent: {reply:?}");
        ctx.registry
            .send(agent, GatewayToAgent::HeartbeatTick)
            .await
            .expect("send");
        assert!(matches!(
            live_rx.recv().await,
            Some(GatewayToAgent::HeartbeatTick)
        ));
    }

    #[tokio::test]
    async fn runtime_with_another_protocol_version_is_refused() {
        let ctx = ctx_with(false, false).await;
        let agent = registered_agent(&ctx).await;

        let (verdict, reply) = hello_with_version(&ctx, agent, "0.1.0").await;

        let err = verdict.expect_err("an older runtime must be refused at Hello");
        assert_eq!(clip(&"x".repeat(100)).len(), 64);
        assert_eq!(clip("0.1.0"), "0.1.0");
        assert!(
            err.to_string().contains("protocol version mismatch"),
            "{err:#}"
        );
        assert!(reply.is_none(), "no Welcome may be sent: {reply:?}");
    }

    #[tokio::test]
    async fn unverified_mode_still_accepts_a_registered_agent() {
        let ctx = ctx_with(false, false).await;
        let agent = registered_agent(&ctx).await;

        let (_, reply) = hello(&ctx, agent).await;

        assert!(
            matches!(reply, Some(GatewayToAgent::Welcome { .. })),
            "{reply:?}"
        );
    }
}
