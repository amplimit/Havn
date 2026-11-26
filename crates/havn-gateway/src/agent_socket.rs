//! Unix-socket listener for connecting agent runtime processes.
//!
//! Each connected agent gets one accepted socket. The handshake is:
//! 0. Before reading anything, the gateway looks up the peer's cgroup.
//!    With verification on, a peer outside every agent cgroup is closed.
//!    The connection then needs a handshake slot (`MAX_PENDING_HANDSHAKES`
//!    in all, `MAX_PENDING_HANDSHAKES_PER_AGENT` per agent cgroup), held
//!    until Welcome; without one it is closed straight away.
//! 1. Agent sends `Hello { agent_id, version }` within `HELLO_TIMEOUT`.
//! 2. Gateway validates protocol version and that the agent was registered
//!    via the lifecycle endpoint (`POST /agents/:id/start`).
//! 3. Gateway replies `Welcome { session_token }`.
//! 4. Per-connection writer task drains an mpsc and writes frames.
//! 5. Per-connection reader loop dispatches inbound frames:
//!    - `LlmRequest` → spawn proxy task → reply with `LlmResponse`
//!    - `OutboundMessage` → route via channel adapter (Phase 1: log only)
//!    - every frame, `LivenessPing` included, is recorded in the registry;
//!      the supervisor restarts agents that go quiet (`crate::supervisor`)

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use anyhow::Context as _;
use havn_core::{AgentId, OutboundMessage, UserId};
use havn_proto::codec::MAX_FRAME_BYTES;
use havn_proto::{
    AgentQuery, AgentQueryOutcome, AgentQueryResponse, AgentQueryResult, AgentToGateway,
    FrameError, GatewayToAgent, GatewayToolCall, GatewayToolOutcome, GatewayToolResult,
    GatewayToolSpec, IncomingQuery, LlmOutcome, LlmRequest, LlmResponse, LlmUsage,
    PROTOCOL_VERSION, read_frame, write_frame,
};
use sqlx::SqlitePool;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::agent_registry::{
    AgentRegistry, MAX_INFLIGHT_AGENT_QUERIES, MAX_INFLIGHT_LLM_REQUESTS, MAX_INFLIGHT_TOOL_CALLS,
    SendError, Slot,
};
use crate::cron::CronScheduler;
use crate::llm_proxy::{self, AnthropicMessage, AnthropicRequest};
use crate::policy_resolver;
use crate::webchat::WebChatRouter;
use havn_core::Policy;
use havn_db::repo::agents as agents_repo;

const OUTBOUND_QUEUE_CAPACITY: usize = 64;

/// How long a Hello may wait for the registration of its agent.
const REGISTRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a connected peer has to send Hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections allowed between the peer lookup and Welcome at once, the
/// registration wait included. The rest are closed as soon as looked up.
const MAX_PENDING_HANDSHAKES: usize = 64;

/// Share of `MAX_PENDING_HANDSHAKES` one agent cgroup may hold, so a
/// compromised agent cannot keep every other agent from connecting.
const MAX_PENDING_HANDSHAKES_PER_AGENT: usize = 2;

/// Connections allowed between accept and the end of their peer lookup.
const MAX_PEER_LOOKUPS: usize = 256;

/// Longest the peer cgroup lookup may take.
const PEER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Refused connections are counted and reported once per interval.
const REFUSAL_WARN_INTERVAL: Duration = Duration::from_secs(10);

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
    /// Where agents' workspaces are: images a turn refers to are read
    /// from its inbox.
    pub workspace_root: std::path::PathBuf,
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
    /// `dm_policy` allows (TOML `allow_from` or the invite allowlist in `db`).
    pub bindings: Arc<Vec<crate::config::ChannelBindingConfig>>,
    pub channels: std::sync::Arc<
        arc_swap::ArcSwap<std::collections::BTreeMap<String, crate::config::ChannelEntry>>,
    >,
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
    /// The LLM calls in flight of turns answering someone (issues #162
    /// and #305).
    pub reply_streams: ReplyStreams,
    /// The replies chat apps are shown as they are written (issue #325).
    pub chat_replies: ChatReplies,
    /// `[llm]` settings: the `max_tokens` ceiling for every proxied LLM
    /// call, whatever the agent's config asks for, and provider base URLs.
    pub llm: Arc<crate::config::LlmConfig>,
    pub models: Arc<arc_swap::ArcSwap<crate::model_catalog::ModelCatalog>>,
    /// The models without `vision` set whose provider refused images
    /// (issue #202), as their entries are: they are sent notes in place of
    /// images until the entry changes or the gateway restarts.
    pub vision_refused: VisionRefused,
    /// `[attachments]`, hot-reloaded (issue #211).
    pub attachments: Arc<arc_swap::ArcSwap<crate::config::AttachmentLimits>>,
    /// True when agents run under `NamespaceSpawner`, each in its own
    /// `agent-<id>` cgroup. The gateway then accepts a connection for an
    /// agent only from a process in that cgroup. With the unisolated
    /// subprocess spawner there is nothing to check against.
    pub verify_peer_cgroup: bool,
    /// Operator-declared extra bind-mounts shipped to each runtime in
    /// Welcome so it can add Landlock entries (spec §4.1 v0.7). Cloned
    /// from `GatewayConfig::extra_mounts` at startup. The namespace
    /// spawner binds these and the host's tool directories
    /// (`havn_spawner::agent_mounts`) and has havn-init apply the Landlock
    /// rules for both itself.
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
    /// The agents' secrets (issue #233), shared with `AppState`.
    pub secrets: crate::secrets::SecretStore,
    /// Backend selection and rate limit of the `web_search` gateway tool.
    pub search: Arc<crate::web_search::WebSearch>,
    /// Rate limit, page cache and answer model of the `web_fetch` gateway
    /// tool.
    pub fetch: Arc<crate::web_fetch::WebFetch>,
    /// The agent subnet pool and port of the egress proxy, when it runs.
    pub egress_proxy: Option<(havn_spawner::network::AgentSubnetPool, u16)>,
    /// Rate of the `set_timezone` gateway tool, per owner.
    pub timezone_rate: Arc<crate::web_search::UserRate>,
}

/// Model entries, by id and hash, whose provider refused images.
pub type VisionRefused = Arc<std::sync::Mutex<std::collections::HashSet<(String, u64)>>>;

/// The key of `entry` in [`VisionRefused`]: an edited entry is tried again.
fn vision_key(entry: &crate::model_catalog::ModelEntry) -> (String, u64) {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::hash::DefaultHasher::new();
    entry.hash(&mut hasher);
    (entry.id.clone(), hasher.finish())
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
    // Under a root gateway isolated agents run as host ids of their own
    // and need write access to connect. Their cgroup, not the socket's
    // mode, is what admits them: a peer outside every agent cgroup is
    // closed at once. havn-init binds the socket into the agent from a
    // handle, so its directory can stay closed to other users.
    // SAFETY: geteuid cannot fail.
    if ctx.verify_peer_cgroup && unsafe { libc::geteuid() } == 0 {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))
            .await
            .with_context(|| format!("opening agent socket {} to agents", socket_path.display()))?;
        if other_users_reach(socket_path) {
            warn!(
                path = %socket_path.display(),
                "other users can reach the agent socket; the gateway closes their connections, \
                 but a directory only root can enter keeps them away"
            );
        }
    }
    info!(path = %socket_path.display(), "agent socket listening");

    let admission = Admission::new(
        MAX_PEER_LOOKUPS,
        MAX_PENDING_HANDSHAKES,
        MAX_PENDING_HANDSHAKES_PER_AGENT,
    );
    serve(listener, ctx, Arc::new(admission)).await
}

/// Whether users other than the owners can walk to `path`: every directory
/// above it lets others search it.
fn other_users_reach(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.ancestors()
        .skip(1)
        .all(|dir| std::fs::metadata(dir).is_ok_and(|meta| meta.permissions().mode() & 0o001 != 0))
}

type PendingPerAgent = Arc<std::sync::Mutex<HashMap<AgentId, usize>>>;

/// Limits on connections that have not finished the handshake, and counts
/// of the connections refused since the last report.
struct Admission {
    lookups: Arc<Semaphore>,
    handshakes: Arc<Semaphore>,
    per_agent: PendingPerAgent,
    per_agent_limit: usize,
    busy: AtomicU64,
    foreign: AtomicU64,
    lookup_failed: AtomicU64,
}

/// One connection's place in the handshake limits, given back on drop.
struct HandshakeSlot {
    _global: OwnedSemaphorePermit,
    agent: Option<(AgentId, PendingPerAgent)>,
}

impl Admission {
    fn new(lookups: usize, handshakes: usize, per_agent_limit: usize) -> Self {
        Self {
            lookups: Arc::new(Semaphore::new(lookups)),
            handshakes: Arc::new(Semaphore::new(handshakes)),
            per_agent: Arc::default(),
            per_agent_limit,
            busy: AtomicU64::new(0),
            foreign: AtomicU64::new(0),
            lookup_failed: AtomicU64::new(0),
        }
    }

    /// A handshake slot for a connection from `agent`'s cgroup (`None`
    /// when peers are not verified), unless a limit is reached.
    fn try_admit(&self, agent: Option<AgentId>) -> Option<HandshakeSlot> {
        let mut pending = self
            .per_agent
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = agent
            && pending.get(&id).copied().unwrap_or(0) >= self.per_agent_limit
        {
            return None;
        }
        let global = Arc::clone(&self.handshakes).try_acquire_owned().ok()?;
        if let Some(id) = agent {
            *pending.entry(id).or_default() += 1;
        }
        Some(HandshakeSlot {
            _global: global,
            agent: agent.map(|id| (id, Arc::clone(&self.per_agent))),
        })
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let Some((id, per_agent)) = self.agent.take() else {
            return;
        };
        let mut pending = per_agent.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = pending.get_mut(&id) {
            *count -= 1;
            if *count == 0 {
                pending.remove(&id);
            }
        }
    }
}

/// Accept connections until accepting fails, reporting refusals on the side.
async fn serve(
    listener: UnixListener,
    ctx: AgentSocketCtx,
    admission: Arc<Admission>,
) -> anyhow::Result<()> {
    tokio::select! {
        result = accept_connections(listener, ctx, Arc::clone(&admission)) => result,
        () = report_refusals(&admission) => Ok(()),
    }
}

async fn accept_connections(
    listener: UnixListener,
    ctx: AgentSocketCtx,
    admission: Arc<Admission>,
) -> anyhow::Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await.context("accept")?;
        let Ok(lookup) = Arc::clone(&admission.lookups).try_acquire_owned() else {
            admission.busy.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let ctx = ctx.clone();
        let admission = Arc::clone(&admission);
        tokio::spawn(async move {
            let peer_cgroup = if ctx.verify_peer_cgroup {
                peer_cgroup(&stream, Some(lookup)).await
            } else {
                drop(lookup);
                None
            };
            if ctx.verify_peer_cgroup && peer_cgroup.is_none() {
                admission.lookup_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let peer_agent = peer_cgroup.as_deref().and_then(peer_agent);
            if ctx.verify_peer_cgroup && peer_agent.is_none() {
                debug!(
                    peer_cgroup = peer_cgroup.as_deref().map(str::trim_end),
                    "closing agent socket connection from outside every agent cgroup"
                );
                admission.foreign.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let Some(slot) = admission.try_admit(peer_agent) else {
                admission.busy.fetch_add(1, Ordering::Relaxed);
                return;
            };
            if let Err(e) = handle_connection(stream, ctx, peer_cgroup, slot).await {
                warn!(error = ?e, "agent connection ended with error");
            }
        });
    }
}

/// Log the refusals counted in each `REFUSAL_WARN_INTERVAL`, at most one
/// warning of each kind per interval.
async fn report_refusals(admission: &Admission) {
    let mut ticks = tokio::time::interval(REFUSAL_WARN_INTERVAL);
    ticks.tick().await;
    loop {
        ticks.tick().await;
        let busy = admission.busy.swap(0, Ordering::Relaxed);
        if busy > 0 {
            warn!(
                refused = busy,
                interval_s = REFUSAL_WARN_INTERVAL.as_secs(),
                "refused agent connections: too many handshakes in progress"
            );
        }
        let foreign = admission.foreign.swap(0, Ordering::Relaxed);
        if foreign > 0 {
            warn!(
                refused = foreign,
                interval_s = REFUSAL_WARN_INTERVAL.as_secs(),
                "refused agent socket connections from processes outside every agent cgroup"
            );
        }
        let failed = admission.lookup_failed.swap(0, Ordering::Relaxed);
        if failed > 0 {
            warn!(
                refused = failed,
                interval_s = REFUSAL_WARN_INTERVAL.as_secs(),
                "refused agent socket connections whose peer cgroup could not be read"
            );
        }
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
///
/// The file is read on a blocking thread that keeps `lookup` (a permit of
/// `MAX_PEER_LOOKUPS`) until the read returns, so a stuck read is given up
/// on after `PEER_LOOKUP_TIMEOUT` but still counts against that cap.
#[cfg(target_os = "linux")]
async fn peer_cgroup(stream: &UnixStream, lookup: Option<OwnedSemaphorePermit>) -> Option<String> {
    let pid = stream.peer_cred().ok()?.pid()?;
    let pin = peer_pidfd(stream);
    if matches!(pin, PeerPidfd::Failed) {
        return None;
    }
    let read = tokio::task::spawn_blocking(move || {
        let _lookup = lookup;
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
    });
    let contents = tokio::time::timeout(PEER_LOOKUP_TIMEOUT, read)
        .await
        .ok()?
        .ok()?
        .ok()?;
    match &pin {
        PeerPidfd::Pinned(fd) if !pidfd_alive(fd) => None,
        _ => Some(contents),
    }
}

#[cfg(not(target_os = "linux"))]
async fn peer_cgroup(
    _stream: &UnixStream,
    _lookup: Option<OwnedSemaphorePermit>,
) -> Option<String> {
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
    contents.and_then(peer_agent) == Some(agent_id)
}

/// The agent whose cgroup `contents` (a `/proc/<pid>/cgroup`) names.
fn peer_agent(contents: &str) -> Option<AgentId> {
    #[cfg(target_os = "linux")]
    {
        cgroup_agent(contents)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = contents;
        None
    }
}

/// Parse `/proc/<pid>/cgroup` (cgroup v2: a single `0::<path>` line, shown
/// relative to the gateway's cgroup root, the same root the spawner
/// creates `havn` under) and accept exactly an agent's cgroup.
/// Comparing the whole path matters: a cgroup the agent creates below its
/// own, even one named after another agent, does not match. Neither does
/// `/havn.slice/agent-<id>`, where gateways before #66 put agents: those
/// belong to a gateway that is gone.
#[cfg(target_os = "linux")]
fn cgroup_agent(contents: &str) -> Option<AgentId> {
    // `/havn/agent-`: the path of every agent cgroup without its id.
    let prefix = havn_core::agent_cgroup("");
    contents
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .find_map(|path| {
            let id = path.trim_end().strip_prefix(&prefix)?;
            AgentId::from_str(id)
                .ok()
                .filter(|parsed| parsed.to_string() == id)
        })
}

/// At most 64 chars of a peer-supplied string, for logging.
fn clip(s: &str) -> &str {
    s.char_indices().nth(64).map_or(s, |(end, _)| &s[..end])
}

async fn handle_connection(
    stream: UnixStream,
    ctx: AgentSocketCtx,
    peer_cgroup: Option<String>,
    handshake: HandshakeSlot,
) -> anyhow::Result<()> {
    // Every sandbox sees the same socket, so the agent id in Hello is only
    // a claim. The peer's process comes from the kernel and cannot be
    // forged; `peer_cgroup` was looked up before anything was read from it.
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    // 1. Handshake: read Hello
    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_frame::<_, AgentToGateway>(&mut reader))
        .await
        .map_err(|_| anyhow::anyhow!("no Hello within {}s", HELLO_TIMEOUT.as_secs()))?
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

    // A runtime can say Hello before the spawn that started it has
    // registered its handle. Give the registration a moment, then check
    // the peer again: it must still be a live process in the agent's
    // cgroup, not one a clean-up killed while we waited.
    if !ctx.registry.is_registered(agent_id).await {
        if !ctx
            .registry
            .wait_registered(agent_id, REGISTRATION_WAIT)
            .await
        {
            anyhow::bail!("refusing connection for agent {agent_id}: not registered");
        }
        if ctx.verify_peer_cgroup {
            let now_in = self::peer_cgroup(reader.get_ref().as_ref(), None).await;
            if !peer_cgroup_is_agents(now_in.as_deref(), agent_id) {
                warn!(
                    %agent_id,
                    peer_cgroup = now_in.as_deref().map(str::trim_end),
                    "refusing agent connection: the peer left the agent's cgroup while waiting"
                );
                anyhow::bail!("agent {agent_id}: connecting process is no longer in its cgroup");
            }
        }
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
    let (policy, model, max_tokens, agent_name, gateway_tools, models, language, secrets): (
        Policy,
        Option<String>,
        Option<u32>,
        Option<String>,
        Vec<GatewayToolSpec>,
        Vec<havn_proto::ModelChoice>,
        havn_core::Language,
        // The agent's secrets, and its own MCP servers, which get them.
        (havn_proto::Secrets, Vec<String>),
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
            let mut policy = policy_resolver::for_session(&ctx.db, &agent, ctx.multi_user).await;
            // The model that titles chats, as the runtime uses it.
            policy.chat_titles.model = title_model(&policy, &ctx.models.load());
            let gateway_tools = gateway_tools_for(&ctx, &agent, &policy).await;
            let models = model_choices(&policy, &ctx.models.load());
            let language = havn_core::Language::of_config(&agent.config);
            // Due OAuth tokens are refreshed first, waited for at most 10 s
            // so a slow provider does not hold the start.
            ctx.secrets
                .refresh_oauth(&ctx.registry, agent.id, std::time::Duration::from_secs(10))
                .await;
            let secrets = (
                ctx.secrets.for_runtime(&agent, &policy).await,
                crate::secrets::own_mcp_servers(&agent.config, &policy),
            );
            (
                policy,
                model,
                max_tokens,
                name,
                gateway_tools,
                models,
                language,
                secrets,
            )
        }
        Ok(None) => {
            warn!(%agent_id, "agent not found at handshake; sending default policy");
            (
                Policy::default(),
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                havn_core::Language::English,
                (havn_proto::Secrets::default(), Vec::new()),
            )
        }
        Err(e) => {
            warn!(%agent_id, error = %e, "policy lookup failed; sending default policy");
            (
                Policy::default(),
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                havn_core::Language::English,
                (havn_proto::Secrets::default(), Vec::new()),
            )
        }
    };
    let (secrets, own_mcp_servers) = secrets;

    let session_token = Uuid::now_v7().to_string();
    let welcome = GatewayToAgent::Welcome {
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
        egress_proxy: egress_proxy_for(&ctx, agent_id).await,
        gateway_tools,
        timezone: Some(crate::timezone::for_agent(&ctx.db, agent_id).await),
        models,
        language,
        cuts_calls: true,
        secrets,
        own_mcp_servers,
    };
    let write_welcome = write_frame(&mut write, &welcome);
    let sent = match tokio::time::timeout(AGENT_WRITE_TIMEOUT, write_welcome).await {
        Ok(sent) => sent.context("sending Welcome"),
        Err(_) => Err(anyhow::anyhow!("sending Welcome timed out")),
    };
    if let Err(e) = sent {
        ctx.registry.detach(agent_id, &tx).await;
        return Err(e);
    }
    drop(handshake);

    info!(%agent_id, "agent connected");

    // 4. Writer task: drain mpsc → wire
    let disconnect = ctx
        .registry
        .disconnect_signal(agent_id, &tx)
        .await
        .unwrap_or_default();
    let mut writer_task = tokio::spawn(run_writer(write, rx, agent_id, ctx.clone(), disconnect));

    // 5. Reader loop; the connection also ends when the writer gives up.
    let connection = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
    let reader_result = tokio::select! {
        result = CONNECTION.scope(connection, run_reader(&mut reader, agent_id, &tx, &ctx)) => Some(result),
        _ = &mut writer_task => None,
    };

    // 6. Cleanup
    ctx.registry.detach(agent_id, &tx).await;
    end_chat_replies(agent_id, connection, &ctx).await;
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

/// Numbers the connections of agents: the replies a connection's runtime
/// writes in chat apps are its own (issue #325).
static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    /// The connection whose frames are being read.
    static CONNECTION: u64;
}

/// End what chat apps were shown of the replies `connection`'s runtime
/// was writing (issue #325), with nothing more said: it is gone and will
/// not send their last parts. A connection that took its place writes its
/// own.
async fn end_chat_replies(agent_id: AgentId, connection: u64, ctx: &AgentSocketCtx) {
    for (channel_id, reply) in ctx.chat_replies.end_connection(agent_id, connection) {
        end_chat_reply(&ctx.channel_router, &channel_id, reply).await;
    }
}

/// Tell the adapter of `channel_id`'s account that `reply` ended, with
/// nothing more to show (issue #325).
async fn end_chat_reply(
    router: &crate::channel_router::ChannelRouter,
    channel_id: &str,
    reply: String,
) {
    let Some((channel, account_id, to)) = crate::channel_id_codec::decode(channel_id) else {
        return;
    };
    let ended = router
        .deliver_with(
            channel,
            account_id,
            to,
            None,
            havn_proto::channel::ChannelMessageContent::Text {
                text: String::new(),
            },
            Vec::new(),
            None,
            false,
            Some(reply),
        )
        .await;
    if let Err(e) = ended {
        debug!(channel, account = account_id, error = %e, "could not end a reply being written");
    }
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
                trimmed: false,
                fallback: None,
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
                            conversation: msg.conversation,
                            channel_id: msg.channel_id,
                            content: havn_core::MessageContent::Text { text: notice },
                            reply_to: None,
                            buttons: Vec::new(),
                            poll: None,
                            partial: false,
                            last: false,
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
    tx: &mpsc::Sender<GatewayToAgent>,
    ctx: &AgentSocketCtx,
) -> Result<(), FrameError>
where
    R: AsyncBufRead + Unpin,
{
    while let Some(frame) = read_frame::<_, AgentToGateway>(reader).await? {
        // Every frame is a sign of life. While a frame is being handled
        // (a delivery can wait on a slow client) the agent is not judged
        // for the pings this reader is not reading.
        let ping = matches!(frame, AgentToGateway::LivenessPing);
        ctx.registry.frame_received(agent_id, tx, ping).await;
        match frame {
            // Kept for this connection only: a status is the runtime's.
            AgentToGateway::McpStatus(status) => {
                ctx.registry
                    .set_mcp_status(agent_id, tx, bounded_mcp_status(status))
                    .await;
            }
            frame => dispatch(agent_id, frame, ctx).await,
        }
        ctx.registry.frame_handled(agent_id, tx).await;
    }
    Ok(())
}

/// MCP servers, and tools of each, a status keeps at most.
const MAX_MCP_STATUS_SERVERS: usize = 64;
const MAX_MCP_STATUS_TOOLS: usize = 256;
/// Characters of a tool's or server's name a status keeps.
const MAX_MCP_STATUS_NAME: usize = 128;

/// `status` within the bounds above, its errors cut.
fn bounded_mcp_status(mut status: havn_proto::McpStatus) -> havn_proto::McpStatus {
    let cut = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    status.servers.truncate(MAX_MCP_STATUS_SERVERS);
    for server in &mut status.servers {
        server.name = cut(&server.name, MAX_MCP_STATUS_NAME);
        server.tools.truncate(MAX_MCP_STATUS_TOOLS);
        for tool in &mut server.tools {
            *tool = cut(tool, MAX_MCP_STATUS_NAME);
        }
        if let Some(error) = &mut server.error {
            *error = cut(error, havn_proto::MAX_MCP_ERROR_CHARS + 1);
        }
    }
    status
}

async fn dispatch(agent_id: AgentId, frame: AgentToGateway, ctx: &AgentSocketCtx) {
    match frame {
        AgentToGateway::Hello { .. } => {
            warn!(%agent_id, "duplicate Hello after handshake — ignoring");
        }
        AgentToGateway::OutboundMessage(msg) => {
            route_outbound(agent_id, from_agent(msg), ctx).await;
        }
        AgentToGateway::ChatTitled(titled) => {
            if let Ok(session) = titled.channel_id.parse::<crate::webchat::SessionId>()
                && crate::channel_id_codec::decode(&titled.channel_id).is_none()
            {
                let title: String = titled
                    .title
                    .chars()
                    .take(havn_db::agent::conversations::MAX_TITLE_CHARS)
                    .collect();
                if let Err(e) = ctx
                    .webchat_router
                    .chat_titled(agent_id, session, titled.conversation, title)
                    .await
                {
                    debug!(%agent_id, error = ?e, "no tab took a chat's title");
                }
            }
        }
        AgentToGateway::CommandReply(msg) => {
            let msg = from_agent(msg);
            // A web chat shows it as a notice; a channel as any message.
            match msg.channel_id.parse::<crate::webchat::SessionId>() {
                Ok(session) if crate::channel_id_codec::decode(&msg.channel_id).is_none() => {
                    if !ctx.webchat_router.take_command_reply(agent_id, session) {
                        warn!(%agent_id, "dropping a command answer no command asked for");
                        return;
                    }
                    let text = match msg.content {
                        havn_core::MessageContent::Text { text } => text,
                        _ => return,
                    };
                    if let Err(e) = ctx
                        .webchat_router
                        .command_notice(agent_id, session, msg.conversation, text)
                        .await
                    {
                        debug!(%agent_id, error = ?e, "no tab took a command's answer");
                    }
                }
                _ => route_outbound(agent_id, msg, ctx).await,
            }
        }
        AgentToGateway::ApprovalRequest(request) => ask_for_approval(agent_id, request, ctx).await,
        AgentToGateway::MemoryChanged(changed) => {
            // Only a dashboard session shows it; a channel's sender is not
            // told what the agent remembers.
            if changed
                .channel_id
                .parse::<crate::webchat::SessionId>()
                .is_ok()
                && let Err(e) = ctx.webchat_router.memory_changed(agent_id, changed).await
            {
                debug!(%agent_id, error = %e, "memory changes not shown");
            }
        }
        AgentToGateway::LlmCancel { request_id, keep } => {
            let streams = ctx
                .reply_streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(asked) = streams.get(&(agent_id, request_id)) {
                if keep {
                    asked.cut.cancel();
                } else {
                    asked
                        .dropped
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        AgentToGateway::Step(step) => {
            // A channel's sender sees the adapter's "typing" instead.
            if step.channel_id.parse::<crate::webchat::SessionId>().is_ok()
                && let Err(e) = ctx.webchat_router.step(agent_id, step).await
            {
                debug!(%agent_id, error = %e, "step not shown");
            }
        }
        AgentToGateway::ApprovalClosed { request_id } => {
            // The egress proxy's questions are closed by the proxy only.
            if !request_id.starts_with(crate::egress_proxy::REQUEST_PREFIX) {
                let _ = ctx
                    .webchat_router
                    .close_approval(agent_id, &request_id)
                    .await;
            }
        }
        AgentToGateway::LlmRequest(req) => {
            if req.request_id.len() > MAX_REQUEST_ID_BYTES {
                warn!(%agent_id, len = req.request_id.len(), "dropping LlmRequest with an oversized request_id");
                return;
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
                    trimmed: false,
                    fallback: None,
                });
                refuse(ctx, agent_id, refusal).await;
                return;
            };
            // Listed before the next frame is read: a cancel that follows
            // finds it.
            let listed = Listed::open(&ctx.reply_streams, agent_id, &req);
            let ctx_cloned = ctx.clone();
            tokio::spawn(async move {
                handle_llm_request(agent_id, req, ctx_cloned, listed).await;
                drop(permit);
            });
        }
        // Recorded by the reader loop, like every frame.
        AgentToGateway::LivenessPing => {}
        AgentToGateway::CronResult(result) => {
            let job_id = result.job_id.clone();
            match ctx.cron_scheduler.record_result(agent_id, result).await {
                Ok(Some(message)) => route_outbound(agent_id, message, ctx).await,
                Ok(None) => {}
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
                return;
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
                return;
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
        AgentToGateway::GatewayToolCall(call) => {
            if call.request_id.len() > MAX_REQUEST_ID_BYTES {
                warn!(%agent_id, len = call.request_id.len(), "dropping GatewayToolCall with an oversized request_id");
                return;
            }
            let Some(permit) = ctx.registry.try_reserve(agent_id, Slot::Tool).await else {
                warn!(%agent_id, limit = MAX_INFLIGHT_TOOL_CALLS, "too many gateway tool calls in flight; refusing one");
                let refusal = GatewayToAgent::GatewayToolResult(GatewayToolResult {
                    request_id: call.request_id,
                    outcome: GatewayToolOutcome::Error {
                        message: format!(
                            "this agent already has {MAX_INFLIGHT_TOOL_CALLS} gateway tool calls in flight; retry when one finishes"
                        ),
                    },
                });
                refuse(ctx, agent_id, refusal).await;
                return;
            };
            let ctx_cloned = ctx.clone();
            tokio::spawn(async move {
                handle_gateway_tool_call(agent_id, call, ctx_cloned).await;
                drop(permit);
            });
        }
        // The protocol enum is `#[non_exhaustive]`; future variants land
        // here so older gateways gracefully ignore frames they don't know.
        other => {
            warn!(%agent_id, ?other, "unknown frame variant — ignoring");
        }
    }
}

/// Longest conversation id of an approval request.
const MAX_APPROVAL_CONVERSATION: usize = 512;
/// Longest input an approval request may show, as the runtime allows.
const MAX_APPROVAL_INPUT: usize = 4000;

/// Ask the agent's owner to approve a tool call: every tab of the agent's
/// dashboard chat shows it, also tabs opened later, and a channel
/// conversation it came from is told it waits. Only the owner opens the
/// dashboard chat, so a channel's sender (a customer, say) cannot approve
/// the agent's actions. Audited as `tool.approval.requested`.
async fn ask_for_approval(
    agent_id: AgentId,
    mut request: havn_proto::ApprovalRequest,
    ctx: &AgentSocketCtx,
) {
    if request.request_id.len() > MAX_REQUEST_ID_BYTES
        || request.channel_id.len() > MAX_APPROVAL_CONVERSATION
    {
        warn!(%agent_id, "dropping an approval request with oversized ids");
        return;
    }
    // The egress proxy's questions carry this prefix; an agent's must not
    // pass for one.
    if request
        .request_id
        .starts_with(crate::egress_proxy::REQUEST_PREFIX)
    {
        warn!(%agent_id, "dropping an approval request with a reserved id");
        return;
    }
    let refuse = |request: &havn_proto::ApprovalRequest| {
        GatewayToAgent::ApprovalDecision(havn_proto::ApprovalDecision {
            channel_id: request.channel_id.clone(),
            request_id: request.request_id.clone(),
            approve: false,
            remember: false,
            note: None,
        })
    };
    request.tool = request.tool.chars().take(200).collect();
    // A kind the gateway knows, or none: a runtime's must not dress a card
    // as another.
    // A runtime of before issue #255 asks about memory as `memory`.
    if request.kind == "memory" {
        request.kind = "result".into();
    }
    if !matches!(request.kind.as_str(), "call" | "result") {
        request.kind.clear();
    }
    request.reason = request.reason.chars().take(300).collect();
    if request.input.chars().count() > MAX_APPROVAL_INPUT
        || !ctx
            .webchat_router
            .ask_approval(agent_id, request.clone())
            .await
    {
        warn!(%agent_id, "refusing an approval request: too long, or too many waiting");
        let _ = ctx.registry.send(agent_id, refuse(&request)).await;
        return;
    }
    if let Ok(Some(agent)) = agents_repo::find_by_id(&ctx.db, agent_id).await {
        crate::audit::record(
            &ctx.db,
            agent.owner_id,
            agent.team_id,
            Some(agent_id),
            "tool.approval.requested",
            // What a call gave and what someone else wrote are not for the
            // audit log, which team admins read (issues #249 and #255).
            if request.kind.is_empty() {
                serde_json::json!({
                    "request_id": request.request_id,
                    "tool": request.tool,
                    "input": request.input,
                    "conversation": request.channel_id,
                })
            } else {
                serde_json::json!({
                    "request_id": request.request_id,
                    "tool": request.tool,
                    "kind": request.kind,
                    "input_chars": request.input.chars().count(),
                    "conversation": request.channel_id,
                })
            },
        )
        .await;
    }
    if !request.kind.is_empty() {
        ask_owner_for_someone_else(agent_id, &request, ctx).await;
        return;
    }
    if let Some((channel, account, sender)) = crate::channel_id_codec::decode(&request.channel_id) {
        let admins_of_account = ctx
            .channels
            .load_full()
            .get(channel)
            .and_then(|entry| entry.accounts.iter().find(|a| a.id == account))
            .map(|acct| acct.admins.clone())
            .unwrap_or_default();
        // The agent's owner with a chat account of this channel connected
        // is an admin there too (issue #182).
        let (owner, language) = match havn_db::repo::agents::find_by_id(&ctx.db, agent_id).await {
            Ok(Some(agent)) => (
                Some(agent.owner_id),
                havn_core::Language::of_config(&agent.config),
            ),
            _ => (None, havn_core::Language::English),
        };
        let owner_linked = match owner {
            Some(owner) => havn_db::repo::channel_access::list_links(&ctx.db, Some(owner))
                .await
                .is_ok_and(|links| links.iter().any(|l| l.channel == channel)),
            None => false,
        };
        let admins = !admins_of_account.is_empty() || owner_linked;
        // The person in this chat decides with a press when they may
        // (issue #203); the text says how by command either way.
        let sender_admin = admins_of_account.iter().any(|id| id == sender)
            || match owner {
                Some(owner) => havn_db::repo::channel_access::linked_user(&ctx.db, channel, sender)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|link| link.user_id == owner),
                None => false,
            };
        let buttons = if sender_admin {
            approval_buttons(&request.request_id)
        } else {
            Vec::new()
        };
        let message = OutboundMessage {
            conversation: None,
            channel_id: request.channel_id,
            content: havn_core::MessageContent::Text {
                text: language.pick(
                    format!(
                        "(A step of this reply waits for approval: {}. {}the agent's owner can \
                         decide in the web chat.)",
                        request.tool,
                        if admins {
                            "An admin can send /approve or /deny here, or "
                        } else {
                            ""
                        }
                    ),
                    format!(
                        "（这条回复的一个步骤在等待批准：{}。{}agent 的所有者在网页聊天里决定。）",
                        request.tool,
                        if admins {
                            "管理员可以在这里发 /approve 或 /deny，或者由"
                        } else {
                            "由"
                        }
                    ),
                ),
            },
            reply_to: None,
            buttons,
            poll: None,
            partial: false,
            last: false,
        };
        route_outbound(agent_id, message, ctx).await;
    }
}

/// A call in a conversation that is not the owner's (issue #255): the
/// conversation is told the agent checks with its owner, and the owner is
/// asked, shown all of it, in the web chat (as any approval) and in their
/// own chat with the same bot when they linked one.
async fn ask_owner_for_someone_else(
    agent_id: AgentId,
    request: &havn_proto::ApprovalRequest,
    ctx: &AgentSocketCtx,
) {
    // A chat app's conversation is told the owner is asked, there; a
    // webhook's call (issue #236) has no one to tell, and the owner is asked
    // on each of the agent's bots.
    let decoded = crate::channel_id_codec::decode(&request.channel_id);
    let bots: Vec<(String, String)> = match decoded {
        Some((channel, account, _)) => vec![(channel.to_string(), account.to_string())],
        None if request.channel_id.starts_with(WEBHOOK_CONVERSATION) => {
            // Only for a webhook of this agent's.
            let theirs = request.channel_id[WEBHOOK_CONVERSATION.len()..]
                .parse::<uuid::Uuid>()
                .ok();
            let found = match theirs {
                Some(id) => havn_db::repo::webhooks::find(&ctx.db, id)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            if found.is_none_or(|hook| hook.agent_id != agent_id) {
                warn!(%agent_id, "an approval for a webhook that is not the agent's; not asked");
                return;
            }
            let agent = agent_id.to_string();
            ctx.bindings
                .iter()
                .filter(|b| b.agent_id == agent)
                .map(|b| (b.channel.clone(), b.account_id.clone()))
                .collect()
        }
        None => return,
    };
    let (owner, language) = match havn_db::repo::agents::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => (
            Some(agent.owner_id),
            havn_core::Language::of_config(&agent.config),
        ),
        _ => (None, havn_core::Language::English),
    };
    let tell =
        |channel_id: String, text: String, buttons: Vec<Vec<havn_core::Button>>| OutboundMessage {
            conversation: None,
            channel_id,
            content: havn_core::MessageContent::Text { text },
            reply_to: None,
            buttons,
            poll: None,
            partial: false,
            last: false,
        };
    if decoded.is_some() {
        route_outbound(
            agent_id,
            tell(
                request.channel_id.clone(),
                language
                    .pick(
                        "(Checking with the agent's owner first; a moment, please.)",
                        "（正在先和 agent 的所有者确认，请稍等。）",
                    )
                    .to_string(),
                Vec::new(),
            ),
            ctx,
        )
        .await;
    }
    let Some(owner) = owner else { return };
    let Ok(links) = havn_db::repo::channel_access::list_links(&ctx.db, Some(owner)).await else {
        return;
    };
    let mut shown = question(request, language);
    if decoded.is_none() {
        shown = format!(
            "{}\n\n{shown}",
            language.pick(
                "(This came with a call of a webhook: always allowing it there allows it for every \
                 later call of that webhook, from whoever holds its secret.)",
                "（这来自一次 webhook 调用：在那里永久允许，就是对这个 webhook 之后的每次调用都允许，\
                 不论调用的是谁，只要持有它的密钥。）",
            )
        );
    }
    for (channel, account) in &bots {
        for link in links.iter().filter(|l| &l.channel == channel) {
            let dm = crate::channel_id_codec::encode(channel, account, &link.sender_id);
            if dm == request.channel_id {
                continue;
            }
            route_outbound(
                agent_id,
                tell(dm, shown.clone(), owner_buttons(request, language)),
                ctx,
            )
            .await;
        }
    }
}

/// The conversation of a webhook's calls in the runtime (issue #236): this
/// prefix and the webhook's id.
pub const WEBHOOK_CONVERSATION: &str = "webhook:";

/// The owner's question about a call in someone else's conversation:
/// who asked and what, the call, what it gave when asked after, and how
/// to answer.
fn question(request: &havn_proto::ApprovalRequest, language: havn_core::Language) -> String {
    let shown = serde_json::from_str::<serde_json::Value>(&request.input).unwrap_or_default();
    let field = |name: &str| -> String {
        shown
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    // What someone else wrote on one line and quoted, so it cannot pass
    // for the parts around it; the call and what it gave in blocks of
    // their own.
    let line = |text: &str| {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("\"{}\"", flat.replace('`', "'"))
    };
    let block = |text: &str| format!("```\n{}\n```", text.trim().replace("```", "'''"));
    let (from, said, tool) = (
        line(&field("from")),
        line(&field("said")),
        line(&request.tool),
    );
    let input = block(
        &shown
            .get("input")
            .and_then(|input| serde_json::to_string_pretty(input).ok())
            .unwrap_or_default(),
    );
    let id = &request.request_id;
    // The owner's own turn in a group (issue #297): others read it, and
    // always allowing there allows it for each of them.
    let owner = shown
        .get("owner")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let asked = if owner {
        language.pick(
            "You asked in a group, which the others there read; always allowing it there \
             allows it for each of them too",
            "你在群里提问，群里其他人都看得到；在那里永久允许，也就对他们每个人都允许了",
        )
    } else {
        language.pick("Someone other than you asked", "有不是你的人提问")
    };
    if request.kind == "call" {
        return language.pick(
            format!(
                "{asked}, and the agent wants to run {tool}.\n\nFrom: \
                 {from}\nThey wrote: {said}\n\nIt would run with:\n{input}\nAllow it once, \
                 always allow this exact call in that conversation, or deny with the buttons. To \
                 add what not to say, send /approve {id} followed by it (or /approve always {id} \
                 ...)."
            ),
            format!(
                "{asked}，agent 想运行 {tool}。\n\n来自：{from}\n对方写的：{said}\n\n要运行\
                 的内容：\n{input}\n用按钮选择：允许这一次、在那个对话里永久允许这一条，或拒绝。要附一\
                 句不能说什么，发 /approve {id} 后面跟上说明（或者 /approve always {id} ……）。"
            ),
        );
    }
    let found = block(&field("found"));
    language.pick(
        format!(
            "{asked}, and the agent used {tool}.\n\nFrom: {from}\nThey \
             wrote: {said}\nWith:\n{input}\n\nIt gave (the agent gets exactly this):\n{found}\n\
             Use it, always allow {tool} in that conversation, or deny with the buttons. To add \
             what not to say, send /approve {id} followed by it (or /approve always {id} ...)."
        ),
        format!(
            "{asked}，agent 用了 {tool}。\n\n来自：{from}\n对方写的：{said}\n输入：\n\
             {input}\n\n得到的内容（agent 会原样拿到这些）：\n{found}\n用按钮选择：使用、在那个对话\
             里永久允许 {tool}，或拒绝。要附一句不能说什么，发 /approve {id} 后面跟上说明（或者 \
             /approve always {id} ……）。"
        ),
    )
}

/// Allow (or use), always allow and deny, for the owner's question.
fn owner_buttons(
    request: &havn_proto::ApprovalRequest,
    language: havn_core::Language,
) -> Vec<Vec<havn_core::Button>> {
    let mut rows = approval_buttons(&request.request_id);
    let labels = if request.kind == "call" {
        language.pick(
            ["Allow once", "Always allow this call there", "Deny"],
            ["允许这一次", "在那里永久允许这一条", "拒绝"],
        )
    } else {
        language.pick(
            ["Use", "Always allow there", "Deny"],
            ["使用", "在那里永久允许", "拒绝"],
        )
    };
    for (button, label) in rows.iter_mut().flatten().zip(labels) {
        button.label = label.to_string();
    }
    rows
}

/// A message an agent sent, without buttons or a poll: they are the
/// gateway's, which
/// makes each stand for a command the person can see (issue #203); an
/// agent's could pass an approval off as something else.
fn from_agent(mut msg: OutboundMessage) -> OutboundMessage {
    msg.buttons.clear();
    msg.poll = None;
    msg
}

/// Approve, approve for the chat and deny, for approval request `id`:
/// each its command naming the request; none when the id (the runtime's)
/// would not read back as one, or not fit a button.
fn approval_buttons(id: &str) -> Vec<Vec<havn_core::Button>> {
    let plain = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-'))
        && !id.eq_ignore_ascii_case("always");
    if !plain {
        return Vec::new();
    }
    let button = |label: &str, data: String| havn_core::Button {
        label: label.to_string(),
        data,
    };
    let rows = vec![
        vec![
            button("Approve", format!("/approve {id}")),
            button("Approve for this chat", format!("/approve always {id}")),
        ],
        vec![button("Deny", format!("/deny {id}"))],
    ];
    if rows
        .iter()
        .flatten()
        .any(|b| b.data.len() > havn_core::MAX_BUTTON_DATA)
    {
        return Vec::new();
    }
    rows
}

/// The models an agent with `policy` may use, with the thinking each takes
/// (issue #222).
pub(crate) fn model_choices(
    policy: &Policy,
    catalog: &crate::model_catalog::ModelCatalog,
) -> Vec<havn_proto::ModelChoice> {
    catalog
        .models
        .iter()
        .filter(|m| llm_proxy::model_allowed(&policy.allowed_models, &m.id))
        .map(|m| havn_proto::ModelChoice {
            id: m.id.clone(),
            label: m.label.clone(),
            thinking: m
                .thinking
                .as_ref()
                .map(crate::model_catalog::ThinkingSpec::choices)
                .unwrap_or_default(),
        })
        .collect()
}

/// The model that writes chat titles for an agent with `policy`: its
/// team's, else the gateway's `[llm] title_model`, when listed and allowed
/// to the agent; empty for the agent's own model.
pub(crate) fn title_model(policy: &Policy, catalog: &crate::model_catalog::ModelCatalog) -> String {
    let named = Some(policy.chat_titles.model.as_str())
        .filter(|m| !m.is_empty())
        .or(catalog.title_model.as_deref());
    match named {
        Some(model)
            if catalog.get(model).is_some()
                && llm_proxy::model_allowed(&policy.allowed_models, model) =>
        {
            model.to_string()
        }
        Some(model) => {
            warn!(
                model,
                "the title model is not listed or not allowed to this agent; its own model titles chats"
            );
            String::new()
        }
        None => String::new(),
    }
}

/// The owner's answer to approval request `id` of `agent_id`: sent to the
/// agent for the request's conversation, closed in every tab, and audited
/// as `tool.approval.decided` with who decided. A request no longer
/// waiting is ignored, and false.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site; a parameter struct would only rename them"
)]
pub async fn forward_decision(
    registry: &AgentRegistry,
    db: &SqlitePool,
    router: &WebChatRouter,
    agent_id: AgentId,
    decided_by: &str,
    id: &str,
    approve: bool,
    remember: bool,
    note: Option<String>,
) -> bool {
    let Some(request) = router.close_approval(agent_id, id).await else {
        return false;
    };
    // The gateway's own question (the egress proxy) is answered here and
    // audited where it was asked.
    if let Some(waiter) = router.take_gateway_waiter(id) {
        let _ = waiter.send((approve, remember, decided_by.to_string()));
        return true;
    }
    if let Ok(Some(agent)) = agents_repo::find_by_id(db, agent_id).await {
        crate::audit::record(
            db,
            agent.owner_id,
            agent.team_id,
            Some(agent_id),
            "tool.approval.decided",
            serde_json::json!({
                "request_id": id,
                "tool": request.tool,
                "approve": approve,
                "remember": remember,
                "note_chars": note.as_deref().map_or(0, |n| n.chars().count()),
                "decided_by": decided_by,
                "conversation": request.channel_id,
            }),
        )
        .await;
    }
    let decision = havn_proto::ApprovalDecision {
        channel_id: request.channel_id,
        request_id: request.request_id,
        approve,
        remember,
        note: note.filter(|_| approve && !request.kind.is_empty()),
    };
    if let Err(e) = registry
        .send(agent_id, GatewayToAgent::ApprovalDecision(decision))
        .await
    {
        warn!(%agent_id, error = ?e, "failed to forward an approval decision");
    }
    true
}

/// Send `msg` from `agent_id` where its channel id points, with the checks
/// every agent message gets.
async fn route_outbound(agent_id: AgentId, msg: OutboundMessage, ctx: &AgentSocketCtx) {
    let _ = deliver_outbound(agent_id, msg, ctx).await;
}

/// Whether agent `agent_id` may send to channel sender `(channel,
/// account, sender)`: its binding allows it, or the sender was admitted.
async fn channel_send_permitted(
    ctx: &AgentSocketCtx,
    agent_id: AgentId,
    channel: &str,
    account_id: &str,
    sender_id: &str,
) -> Result<(), crate::channel_router::OutboundRefusal> {
    crate::channel_router::send_permitted(
        &ctx.db,
        &ctx.bindings,
        &ctx.channels.load_full(),
        agent_id,
        channel,
        account_id,
        sender_id,
    )
    .await
}

/// Whether `url` names a file this gateway put in agent `agent_id`'s
/// outbox, under `prefix` (`/api/v1/channel/files` or `/agents`): the
/// only file links passed on, so an agent cannot have an adapter fetch
/// (with its token) or a dashboard link to a URL of its choosing.
fn minted_file_url(url: &str, prefix: &str, agent_id: AgentId) -> bool {
    let Some(rest) = url.strip_prefix(&format!("{prefix}/{agent_id}/")) else {
        return false;
    };
    let rest = rest.strip_prefix("outbox/").unwrap_or(rest);
    uuid::Uuid::parse_str(rest).is_ok_and(|id| id.to_string() == rest)
}

/// Route `msg` to its conversation; whether it reached it (an open tab, a
/// connected adapter).
async fn deliver_outbound(agent_id: AgentId, msg: OutboundMessage, ctx: &AgentSocketCtx) -> bool {
    // Three dispatch cases by channel_id shape:
    //  1. "broadcast" sentinel — fan out to all open WebChat
    //     sessions for this agent (heartbeat/cron output).
    //  2. `ch|<channel>|<account>|<sender>` — channel adapter
    //     codec (spec §3, channel_id_codec). Decode +
    //     deliver via channel_router to the right WS.
    //  3. Anything else — assumed to be a WebChat session
    //     UUID; route via webchat_router.
    //  4. `broadcast:<session>` — the same as 1, for one session's tabs.
    if msg.channel_id == "broadcast" {
        let n = ctx.webchat_router.broadcast(agent_id, msg).await;
        debug!(%agent_id, sessions = n, "broadcast outbound delivered");
        n > 0
    } else if let Some(session) = msg.channel_id.strip_prefix(havn_proto::BROADCAST_TO) {
        let Ok(session) = session.parse::<crate::webchat::SessionId>() else {
            warn!(%agent_id, "dropping an outbound for a malformed web chat");
            return false;
        };
        let n = ctx
            .webchat_router
            .broadcast_to(agent_id, session, msg)
            .await;
        debug!(%agent_id, tabs = n, "outbound for one web chat delivered");
        n > 0
    } else if let Some((channel, account_id, sender_id)) =
        crate::channel_id_codec::decode(&msg.channel_id)
    {
        // A part of the reply being written there, or its end (issue #325).
        let reply = msg
            .conversation
            .as_deref()
            .filter(|_| msg.partial || msg.last)
            .and_then(|chat| {
                ctx.chat_replies
                    .of(agent_id, &msg.channel_id, chat, msg.last)
            });
        let refusal = channel_send_permitted(ctx, agent_id, channel, account_id, sender_id)
            .await
            .err();
        if let Some(refusal) = refusal {
            warn!(
                %agent_id,
                channel,
                account = account_id,
                reason = refusal.as_str(),
                "dropping channel-adapter outbound"
            );
            // What the adapter showed of the reply ends all the same.
            if msg.last
                && let Some(reply) = reply
            {
                end_chat_reply(&ctx.channel_router, &msg.channel_id, reply).await;
            }
            return false;
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
            MessageContent::File {
                url,
                filename,
                caption,
            } if minted_file_url(&url, "/api/v1/channel/files", agent_id) => {
                Some(ChannelMessageContent::File {
                    url,
                    filename,
                    caption,
                })
            }
            MessageContent::File { url, .. } => {
                warn!(%agent_id, url, "dropping a file link the gateway did not make");
                None
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
            Some(content) => match ctx
                .channel_router
                .deliver_with(
                    channel,
                    account_id,
                    sender_id,
                    msg.reply_to,
                    content,
                    msg.buttons,
                    msg.poll,
                    msg.partial,
                    reply,
                )
                .await
            {
                Ok(_) => true,
                Err(e) => {
                    warn!(
                        %agent_id,
                        channel,
                        account = account_id,
                        error = %e,
                        "could not deliver outbound to channel adapter (offline?)"
                    );
                    // It is kept for the adapter's resume.
                    true
                }
            },
            None => {
                warn!(%agent_id, "dropping channel-adapter outbound: unsupported MessageContent variant");
                if msg.last
                    && let Some(reply) = reply
                {
                    end_chat_reply(&ctx.channel_router, &msg.channel_id, reply).await;
                }
                false
            }
        }
    } else {
        if let havn_core::MessageContent::File { url, .. } = &msg.content
            && !minted_file_url(url, "/agents", agent_id)
        {
            warn!(%agent_id, url, "dropping a file link the gateway did not make");
            return false;
        }
        match ctx.webchat_router.deliver(agent_id, msg).await {
            Ok(()) => true,
            Err(e) => {
                warn!(%agent_id, error = %e, "could not deliver outbound to webchat");
                false
            }
        }
    }
}

/// The proxy URL for `agent_id`'s tools, when its policy sends egress
/// through the egress proxy and the proxy runs.
async fn egress_proxy_for(ctx: &AgentSocketCtx, agent_id: AgentId) -> Option<String> {
    let (pool, port) = ctx.egress_proxy?;
    let agent = agents_repo::find_by_id(&ctx.db, agent_id).await.ok()??;
    let slot = agents_repo::link_slot_of(&ctx.db, agent_id).await.ok()??;
    let policy = policy_resolver::for_session(&ctx.db, &agent, ctx.multi_user).await;
    let net = &policy.network_policy;
    (policy.permissions.can_access_network && net.egress_allowed && net.egress_proxy)
        .then(|| crate::egress_proxy::EgressProxy::url_for(pool, slot, port))
}

/// The gateway-run tools `agent` may use: `web_search` when its policy
/// allows it and its owner has a search backend, `web_fetch` when its
/// policy allows it.
async fn gateway_tools_for(
    ctx: &AgentSocketCtx,
    agent: &havn_db::repo::agents::Agent,
    policy: &Policy,
) -> Vec<GatewayToolSpec> {
    // Every agent may set its owner's time zone and send files to the
    // conversation it answers.
    let mut tools = vec![
        crate::timezone::spec(),
        crate::outbox::spec(),
        crate::choice::spec(),
    ];
    if policy.permissions.can_search_web {
        match ctx
            .search
            .backend_for(&ctx.db, agent.owner_id, agent.team_id, &ctx.models.load())
            .await
        {
            Ok(Some(backend)) => {
                debug!(agent_id = %agent.id, backend = backend.name(), "offering web_search");
                tools.push(ctx.search.spec());
            }
            Ok(None) => {}
            Err(e) => {
                warn!(agent_id = %agent.id, error = %e, "search credential lookup failed; not offering web_search");
            }
        }
    }
    if policy.permissions.can_fetch_web {
        tools.push(ctx.fetch.spec());
    }
    if policy.permissions.can_schedule_cron {
        tools.push(crate::schedule::spec());
    }
    // Secrets reach shell commands and MCP servers; the owner can set
    // them only with HAVN_AGE_KEY.
    if ctx.secrets.encrypts()
        && (policy.permissions.can_use_shell || policy.permissions.can_use_mcp)
    {
        tools.push(crate::secrets::spec());
    }
    tools
}

async fn handle_gateway_tool_call(agent_id: AgentId, call: GatewayToolCall, ctx: AgentSocketCtx) {
    let outcome = match run_gateway_tool(agent_id, &call, &ctx).await {
        Ok(content) => GatewayToolOutcome::Ok { content },
        Err(message) => GatewayToolOutcome::Error { message },
    };
    let result = GatewayToolResult {
        request_id: call.request_id,
        outcome,
    };
    if let Err(e) = ctx
        .registry
        .send(agent_id, GatewayToAgent::GatewayToolResult(result))
        .await
    {
        warn!(%agent_id, error = %e, "could not deliver GatewayToolResult");
    }
}

async fn run_gateway_tool(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    match call.name.as_str() {
        crate::timezone::TOOL_NAME => run_set_timezone(agent_id, call, ctx).await,
        crate::schedule::TOOL_NAME => run_schedule(agent_id, call, ctx).await,
        crate::web_search::TOOL_NAME => run_web_search(agent_id, call, ctx).await,
        crate::web_fetch::TOOL_NAME => run_web_fetch(agent_id, call, ctx).await,
        crate::outbox::TOOL_NAME => run_send_file(agent_id, call, ctx).await,
        crate::choice::TOOL_NAME => run_ask_choice(agent_id, call, ctx).await,
        crate::secrets::TOOL_NAME => run_secret_request(agent_id, call, ctx).await,
        other => Err(format!("the gateway has no tool named {other:?}")),
    }
}

/// `secret_request` (issue #233): the agent asks its owner for a secret it
/// lacks, which the owner enters on the dashboard. Audited as
/// `secret.requested`.
async fn run_secret_request(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    if !ctx.secrets.encrypts() {
        return Err("secrets cannot be set on this gateway (no HAVN_AGE_KEY)".into());
    }
    let agent = match agents_repo::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => agent,
        Ok(None) => return Err(format!("agent {agent_id} not found")),
        Err(e) => return Err(format!("agent lookup failed: {e}")),
    };
    let (text, was) = crate::secrets::run_request(&ctx.db, &agent, call).await?;
    // One entry per request: asking again for one still waiting only
    // counts on it.
    if was == havn_db::repo::agent_secrets::Was::Absent {
        let field = |key: &str| {
            call.input
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(|s| s.trim().chars().take(500).collect::<String>())
        };
        let (by, place, by_owner) = crate::secrets::asked_by(&ctx.db, &agent, call).await;
        crate::audit::record(
            &ctx.db,
            agent.owner_id,
            agent.team_id,
            Some(agent_id),
            "secret.requested",
            serde_json::json!({
                "name": field("name"),
                "reason": field("reason"),
                "env": field("env"),
                "asked_by": by,
                "asked_in": place,
                "by_owner": by_owner,
            }),
        )
        .await;
    }
    Ok(text)
}

/// `send_file`: copy the file into the agent's outbox and send its
/// conversation a [`havn_core::MessageContent::File`] naming where to get
/// it. A channel adapter fetches it with its token; the dashboard shows a
/// download.
async fn run_send_file(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    let channel_id = call
        .conversation
        .clone()
        .ok_or("send_file works only while answering someone's message")?;
    // Somewhere it can go, before anything is kept.
    if let Some((channel, account, sender)) = crate::channel_id_codec::decode(&channel_id) {
        channel_send_permitted(ctx, agent_id, channel, account, sender)
            .await
            .map_err(|r| format!("this conversation cannot be sent files ({})", r.as_str()))?;
    } else {
        let session = channel_id
            .parse::<crate::webchat::SessionId>()
            .map_err(|_| "send_file works only while answering someone's message".to_string())?;
        if ctx.webchat_router.agent_for_session(session).await != Some(agent_id) {
            return Err("the conversation has no open dashboard tab to send the file to".into());
        }
    }
    let path = call
        .input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing required field: path")?
        .to_string();
    let caption = call
        .input
        .get("caption")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let workspace = ctx
        .workspace_root
        .join(agent_id.to_string())
        .join("workspace");
    let outbox = crate::outbox::dir(&ctx.workspace_root, agent_id);
    let chat = call.chat.clone();
    let to = channel_id.clone();
    let sent = tokio::task::spawn_blocking(move || {
        crate::outbox::send(
            &workspace,
            &outbox,
            &path,
            caption.as_deref(),
            &to,
            chat.as_deref(),
        )
    })
    .await
    .map_err(|e| format!("sending the file failed: {e}"))??;
    // Where the conversation's channel gets it.
    let url = if crate::channel_id_codec::decode(&channel_id).is_some() {
        format!("/api/v1/channel/files/{agent_id}/{}", sent.id)
    } else {
        format!("/agents/{agent_id}/outbox/{}", sent.id)
    };
    let delivered = deliver_outbound(
        agent_id,
        OutboundMessage {
            conversation: call.chat.clone(),
            channel_id,
            content: havn_core::MessageContent::File {
                url,
                filename: sent.name.clone(),
                caption: sent.caption.clone(),
            },
            reply_to: None,
            buttons: Vec::new(),
            poll: None,
            partial: false,
            last: false,
        },
        ctx,
    )
    .await;
    if !delivered {
        let outbox = crate::outbox::dir(&ctx.workspace_root, agent_id);
        let id = sent.id.clone();
        let _ = tokio::task::spawn_blocking(move || crate::outbox::remove(&outbox, &id)).await;
        return Err(
            "the file could not be delivered: the conversation's app is not connected".into(),
        );
    }
    if let Ok(Some(agent)) = agents_repo::find_by_id(&ctx.db, agent_id).await {
        crate::audit::record_agent_action(
            &ctx.db,
            agent.owner_id,
            agent_id,
            "file.sent",
            serde_json::json!({"id": sent.id, "name": sent.name, "size": sent.size}),
        )
        .await;
    }
    Ok(format!(
        "sent {:?} ({} bytes) to the conversation",
        sent.name, sent.size
    ))
}

/// `ask_choice` (issue #204): keep the question and send its conversation
/// the options, as buttons or a poll where the app shows them.
async fn run_ask_choice(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    let asked = crate::choice::read(&call.input)?;
    let channel_id = call
        .conversation
        .clone()
        .ok_or("ask_choice works only while answering someone's message")?;
    if let Some((channel, account, sender)) = crate::channel_id_codec::decode(&channel_id) {
        channel_send_permitted(ctx, agent_id, channel, account, sender)
            .await
            .map_err(|r| format!("this conversation cannot be asked ({})", r.as_str()))?;
    } else if channel_id.parse::<crate::webchat::SessionId>().is_err() {
        return Err("ask_choice works only while answering someone's message".into());
    }
    let id = ctx
        .webchat_router
        .ask_choice(agent_id, channel_id.clone(), asked.clone());
    let delivered = deliver_outbound(
        agent_id,
        OutboundMessage {
            conversation: call.chat.clone(),
            channel_id,
            content: havn_core::MessageContent::Text {
                text: crate::choice::text(&asked),
            },
            reply_to: None,
            buttons: crate::choice::buttons(&asked, &id),
            poll: crate::choice::poll(&asked, &id),
            partial: false,
            last: false,
        },
        ctx,
    )
    .await;
    if !delivered {
        ctx.webchat_router.forget_choice(&id);
        return Err(
            "the question could not be delivered: the conversation's app is not \
                    connected"
                .into(),
        );
    }
    Ok(
        "Asked. Their choice comes as their next message, marked as one; end your turn \
        now, without answering for them."
            .into(),
    )
}

async fn run_set_timezone(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    let owner = match agents_repo::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => agent.owner_id,
        Ok(None) => return Err(format!("agent {agent_id} not found")),
        Err(e) => return Err(format!("agent lookup failed: {e}")),
    };
    crate::timezone::run_tool(
        &ctx.db,
        &ctx.registry,
        &ctx.timezone_rate,
        owner,
        agent_id,
        &call.input,
    )
    .await
}

async fn run_schedule(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    let agent = match agents_repo::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => agent,
        Ok(None) => return Err(format!("agent {agent_id} not found")),
        Err(e) => return Err(format!("agent lookup failed: {e}")),
    };
    // The policy the tool was offered under, checked again per call.
    let policy = policy_resolver::for_session(&ctx.db, &agent, ctx.multi_user).await;
    if !policy.permissions.can_schedule_cron {
        return Err("this agent's policy does not allow scheduled tasks".into());
    }
    let result = crate::schedule::run(
        &ctx.db,
        agent.owner_id,
        agent_id,
        call.conversation.as_deref(),
        call.chat.as_deref(),
        &call.input,
    )
    .await;
    let action = call
        .input
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if action != "list" {
        let field = |key: &str| {
            call.input
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(|s| s.chars().take(200).collect::<String>())
        };
        let mut details = serde_json::json!({
            "action": action,
            "id": field("id"),
            "name": field("name"),
            "at": field("at"),
            "every": field("every"),
            "cron": field("cron"),
            "in_minutes": call.input.get("in_minutes").cloned(),
        });
        if let Err(message) = &result {
            details["error"] = serde_json::Value::String(message.clone());
        }
        crate::audit::record(
            &ctx.db,
            agent.owner_id,
            agent.team_id,
            Some(agent_id),
            "agent.schedule",
            details,
        )
        .await;
    }
    result
}

async fn run_web_fetch(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    let agent = match agents_repo::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => agent,
        Ok(None) => return Err(format!("agent {agent_id} not found")),
        Err(e) => return Err(format!("agent lookup failed: {e}")),
    };
    // Counted before any other work, as for web_search.
    if !ctx.fetch.admit(agent.owner_id).await {
        return Err(format!(
            "this user's agents have read {} pages with web_fetch in the last minute; wait and retry",
            ctx.fetch.per_user_per_minute()
        ));
    }
    let policy = policy_resolver::for_session(&ctx.db, &agent, ctx.multi_user).await;
    if !policy.permissions.can_fetch_web {
        return Err("this agent's policy does not allow reading web pages".into());
    }
    let agent_model = agent
        .config
        .get("model")
        .and_then(serde_json::Value::as_str);
    let billing = llm_proxy::Billing {
        user_id: agent.owner_id,
        agent_id: Some(agent_id),
        agent_team: agent.team_id,
        budget: policy.budget.clone(),
    };
    // No request leaves the host unless an answer is possible.
    let ready = ctx
        .fetch
        .answer_models(&ctx.models.load(), agent_model, &policy)
        .and_then(|models| match ctx.keyring.as_ref() {
            Some(ring) => Ok((models, ring)),
            None => Err(
                "credential encryption is not configured: set HAVN_AGE_KEY and \
                         restart the gateway"
                    .to_string(),
            ),
        });
    let result = match ready {
        Ok((models, keyring)) => {
            let run = crate::web_fetch::Call {
                db: &ctx.db,
                http: &ctx.http,
                keyring,
                llm: &ctx.llm,
                billing: &billing,
                models: &models,
                policy: &policy,
                known: ctx.registry.known_urls(),
            };
            ctx.fetch.run(&run, &call.input).await
        }
        Err(message) => Err(message),
    };
    let url: String = call
        .input
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(2048)
        .collect();
    let details = match &result {
        Ok(answer) => serde_json::json!({
            "url": url,
            "read": answer.url,
            "chars": answer.chars,
            "model": answer.model,
        }),
        Err(message) => serde_json::json!({ "url": url, "error": message }),
    };
    crate::audit::record(
        &ctx.db,
        agent.owner_id,
        agent.team_id,
        Some(agent_id),
        "web.fetch",
        details,
    )
    .await;
    result.map(|answer| answer.text)
}

async fn run_web_search(
    agent_id: AgentId,
    call: &GatewayToolCall,
    ctx: &AgentSocketCtx,
) -> Result<String, String> {
    let agent = match agents_repo::find_by_id(&ctx.db, agent_id).await {
        Ok(Some(agent)) => agent,
        Ok(None) => return Err(format!("agent {agent_id} not found")),
        Err(e) => return Err(format!("agent lookup failed: {e}")),
    };
    // First, before any other work: every call counts against the owner's
    // rate, and refusals here are not audited, so a runtime cannot make the
    // gateway query or write without bound.
    if !ctx.search.admit(agent.owner_id).await {
        return Err(format!(
            "this user's agents have made {} web_search calls in the last minute; wait and retry",
            ctx.search.per_user_per_minute()
        ));
    }
    // The policy may have changed since Welcome offered the tool.
    let policy = policy_resolver::for_session(&ctx.db, &agent, ctx.multi_user).await;
    if !policy.permissions.can_search_web {
        return Err("this agent's policy does not allow web search".into());
    }
    let billing = llm_proxy::Billing {
        user_id: agent.owner_id,
        agent_id: Some(agent_id),
        agent_team: agent.team_id,
        budget: policy.budget.clone(),
    };
    let result = ctx
        .search
        .run(
            &ctx.db,
            &ctx.http,
            ctx.keyring.as_ref(),
            &ctx.models.load(),
            &billing,
            &call.input,
        )
        .await;
    let query: String = call
        .input
        .get("query")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(400)
        .collect();
    let details = match &result {
        Ok(report) => serde_json::json!({
            "query": query,
            "backend": report.backend.name(),
            "results": report.hits.len(),
        }),
        Err(message) => serde_json::json!({ "query": query, "error": message }),
    };
    crate::audit::record(
        &ctx.db,
        agent.owner_id,
        agent.team_id,
        Some(agent_id),
        "web.search",
        details,
    )
    .await;
    if let Ok(report) = &result {
        // Result links come from the search backend, not the model, so the
        // agent may read them.
        for hit in &report.hits {
            if let Ok(url) = crate::web_fetch::normalize(&hit.url) {
                ctx.registry.known_urls().record(agent_id, &url);
            }
        }
    }
    result.map(|report| crate::web_search::render(&report, chrono::Utc::now()))
}

async fn handle_llm_request(
    agent_id: AgentId,
    req: LlmRequest,
    ctx: AgentSocketCtx,
    listed: Option<Listed>,
) {
    let request_id = req.request_id.clone();
    // Where the call's text is shown as it is written, once it is taken.
    let mut reply = None;
    // The runtime was told its stopped call wrote nothing (issue #305).
    let mut answered = false;

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
                trimmed: false,
                fallback: None,
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
                trimmed: false,
                fallback: None,
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
        Ok(mut ar) => {
            let resolved = ctx.models.load().resolve(&req.model).cloned();
            let entry = match resolved {
                Ok(entry) => entry,
                Err(message) => {
                    warn!(%agent_id, model = ?req.model, "refusing LLM request for a model the gateway does not list");
                    let _ = ctx
                        .registry
                        .send(
                            agent_id,
                            GatewayToAgent::LlmResponse(LlmResponse {
                                request_id,
                                outcome: LlmOutcome::Error { message },
                                provider_response: serde_json::Value::Null,
                                usage: None,
                                trimmed: false,
                                fallback: None,
                            }),
                        )
                        .await;
                    return;
                }
            };
            if !llm_proxy::model_allowed(&policy.allowed_models, &entry.id) {
                warn!(%agent_id, model = ?entry.id, "refusing LLM request for a model the policy does not allow");
                let _ = ctx
                    .registry
                    .send(
                        agent_id,
                        GatewayToAgent::LlmResponse(LlmResponse {
                            request_id,
                            outcome: LlmOutcome::Error {
                                message: format!(
                                    "model {:?} is not allowed by this agent's policy",
                                    entry.id
                                ),
                            },
                            provider_response: serde_json::Value::Null,
                            usage: None,
                            trimmed: false,
                            fallback: None,
                        }),
                    )
                    .await;
                return;
            }
            ar.model = entry.model().to_string();
            // How much the model thinks (issue #190): the chat's choice, or
            // the model's default, within what the model takes.
            ar.reasoning = entry.thinking.as_ref().map(|spec| {
                spec.resolve(
                    req.options
                        .get("thinking")
                        .and_then(serde_json::Value::as_str),
                )
            });
            // A thinking budget on Anthropic counts within `max_tokens`:
            // ask for room for the reply after it (the clamps below still
            // apply, and the request then shrinks the budget).
            if ctx.llm.endpoints.api_of(entry.provider()) == Some("anthropic")
                && let Some(llm_proxy::Reasoning::Budget(budget)) = &ar.reasoning
            {
                ar.max_tokens = ar.max_tokens.max(budget.saturating_add(4096));
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
                            trimmed: false,
                            fallback: None,
                        }),
                    )
                    .await;
                return;
            }
            // Images the turn names in the agent's inbox, read where it is
            // sent: shown to a model that takes them, tried with one whose
            // entry does not say (issue #202).
            let workspace = ctx
                .workspace_root
                .join(agent_id.to_string())
                .join("workspace");
            let key = vision_key(&entry);
            let refused_before = ctx
                .vision_refused
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&key);
            let show = entry.vision.unwrap_or(!refused_before);
            let try_blind = entry.vision.is_none();
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
                            trimmed: false,
                            fallback: None,
                        }),
                    )
                    .await;
                return;
            };
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
            if let Some(listed) = &listed {
                reply = ReplyText::open(&ctx, agent_id, &req, listed).await;
            }
            let cut = listed.as_ref().map(|listed| &listed.asked.cut);
            let streamed = reply.as_ref().zip(cut);
            let ctx_ref = &ctx;
            let (billing_ref, entry_ref) = (&billing, &entry);
            let send = move |request: AnthropicRequest| async move {
                match streamed {
                    // Shown as it is written: the turn's stop cuts it short
                    // (issue #305).
                    Some((reply, cut)) => {
                        llm_proxy::stream_with_provider(
                            &ctx_ref.db,
                            &ctx_ref.http,
                            ring,
                            billing_ref,
                            entry_ref.provider(),
                            entry_ref.max_output_tokens,
                            &request,
                            &ctx_ref.llm,
                            reply.sink.as_ref(),
                            cut,
                        )
                        .await
                    }
                    None => {
                        llm_proxy::complete_with_provider(
                            &ctx_ref.db,
                            &ctx_ref.http,
                            ring,
                            billing_ref,
                            entry_ref.provider(),
                            entry_ref.max_output_tokens,
                            &request,
                            &ctx_ref.llm,
                        )
                        .await
                    }
                }
            };
            let limits = (**ctx.attachments.load()).clone();
            let context = entry.context();
            // What the model is asked to write, as it will be sent: the
            // room for its input is the rest of the window.
            let mut ar = ar;
            ar.max_tokens = llm_proxy::provider::clamp_max_tokens(
                &ar.model,
                entry.max_output_tokens,
                ar.max_tokens,
                ctx.llm.max_tokens_ceiling,
            );
            let seeing =
                llm_proxy::send_seeing(ar, workspace, limits, context, show, try_blind, send);
            // A call shown to nobody is not cut short: it runs on and
            // records what it used, as before issue #305, and the turn's
            // stop is answered at once with nothing written.
            let seen = match cut.filter(|_| streamed.is_none()) {
                Some(cut) => {
                    tokio::pin!(seeing);
                    tokio::select! {
                        biased;
                        seen = &mut seeing => seen,
                        () = cut.cancelled() => {
                            answered = true;
                            let stopped = serde_json::json!({
                                "id": "",
                                "model": entry.model(),
                                "role": "assistant",
                                "content": [],
                                "stop_reason": llm_proxy::STOPPED,
                                "usage": {"input_tokens": 0, "output_tokens": 0},
                            });
                            let _ = ctx
                                .registry
                                .send(
                                    agent_id,
                                    GatewayToAgent::LlmResponse(LlmResponse {
                                        request_id: request_id.clone(),
                                        outcome: LlmOutcome::Ok,
                                        provider_response: stopped,
                                        usage: None,
                                        trimmed: false,
                                        fallback: None,
                                    }),
                                )
                                .await;
                            seeing.await
                        }
                    }
                }
                None => seeing.await,
            };
            let (result, trimmed) = match seen {
                Ok(seen) => {
                    if seen.model_takes_none
                        && ctx
                            .vision_refused
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(key)
                    {
                        warn!(
                            model = %entry.id,
                            "the provider refused images for this model; it is sent a note in their \
                             place from now on (until its entry changes or the gateway restarts). \
                             Set `vision = false` on it in [[llm.models]] to skip the first try"
                        );
                    }
                    if seen.trimmed {
                        info!(
                            %agent_id,
                            model = %entry.id,
                            context,
                            "left part of an LLM request out to fit the model's context window"
                        );
                    }
                    (seen.result, seen.trimmed)
                }
                Err(e) => {
                    warn!(%agent_id, error = %e, "could not read the files of an LLM request");
                    if answered {
                        return;
                    }
                    let _ = ctx
                        .registry
                        .send(
                            agent_id,
                            GatewayToAgent::LlmResponse(LlmResponse {
                                request_id,
                                outcome: LlmOutcome::Error {
                                    message: format!("reading the turn's files failed: {e}"),
                                },
                                provider_response: serde_json::Value::Null,
                                usage: None,
                                trimmed: false,
                                fallback: None,
                            }),
                        )
                        .await;
                    return;
                }
            };
            match result {
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
                        trimmed,
                        fallback: None,
                    }
                }
                Err(e) => {
                    // Down for now on every credential: a model of its
                    // fallback list the policy allows goes on (issue #216).
                    let fallback = matches!(e, llm_proxy::ProxyError::Unavailable { .. })
                        .then(|| {
                            let catalog = ctx.models.load();
                            entry
                                .fallback
                                .iter()
                                .find(|id| {
                                    *id != &entry.id
                                        && catalog.models.iter().any(|m| &m.id == *id)
                                        && llm_proxy::model_allowed(&policy.allowed_models, id)
                                })
                                .cloned()
                        })
                        .flatten();
                    if let Some(next) = &fallback {
                        warn!(%agent_id, model = %entry.id, fallback = %next, error = %e, "model unavailable; its fallback goes on");
                    }
                    LlmResponse {
                        request_id,
                        outcome: LlmOutcome::Error {
                            message: e.to_string(),
                        },
                        provider_response: serde_json::Value::Null,
                        usage: None,
                        // Refused as too long even fitted: compacting the
                        // chat is what is left (issue #214).
                        trimmed: trimmed || llm_proxy::too_long(&e),
                        fallback,
                    }
                }
            }
        }
        Err(e) => LlmResponse {
            request_id,
            outcome: LlmOutcome::Error {
                message: format!("invalid request: {e}"),
            },
            provider_response: serde_json::Value::Null,
            usage: None,
            trimmed: false,
            fallback: None,
        },
    };

    // The text shown arrives before the reply the runtime sends next.
    if let Some(reply) = reply {
        reply.finish().await;
    }
    // The runtime was answered when its turn stopped.
    if answered {
        return;
    }
    if let Err(e) = ctx
        .registry
        .send(agent_id, GatewayToAgent::LlmResponse(response))
        .await
    {
        warn!(%agent_id, error = %e, "could not deliver LlmResponse");
    }
}

/// How often the text of a streamed call goes to the tabs, at most.
const REPLY_TEXT_EVERY: std::time::Duration = std::time::Duration::from_millis(80);
/// How often the whole text so far goes instead of what it added, so a
/// tab that missed a piece, or opened during the call, catches up.
const REPLY_TEXT_WHOLE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// The LLM calls of each agent's turns answering someone, by request id,
/// and what the runtime asked of them.
pub type ReplyStreams =
    Arc<std::sync::Mutex<std::collections::HashMap<(AgentId, String), Arc<Asked>>>>;

/// What the runtime asked of an LLM call of a turn answering someone.
#[derive(Debug, Default)]
pub struct Asked {
    /// Stop passing its text on: the runtime no longer waits for it.
    dropped: std::sync::atomic::AtomicBool,
    /// End it and answer with what it had written (issue #305).
    cut: tokio_util::sync::CancellationToken,
}

/// A call's entry in [`ReplyStreams`], removed when dropped.
struct Listed {
    asked: Arc<Asked>,
    key: (AgentId, String),
    streams: ReplyStreams,
    /// The connection it came over.
    connection: u64,
}

impl Listed {
    /// For a request of a turn answering someone; `None` for any other.
    fn open(streams: &ReplyStreams, agent_id: AgentId, req: &LlmRequest) -> Option<Self> {
        req.reply.as_ref()?;
        let asked = Arc::new(Asked::default());
        let key = (agent_id, req.request_id.clone());
        streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), Arc::clone(&asked));
        Some(Self {
            asked,
            key,
            streams: Arc::clone(streams),
            connection: CONNECTION.try_with(|c| *c).unwrap_or(0),
        })
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        let mut streams = self
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A request id used again is another call's.
        if streams
            .get(&self.key)
            .is_some_and(|asked| Arc::ptr_eq(asked, &self.asked))
        {
            streams.remove(&self.key);
        }
    }
}

enum ReplyPiece {
    Text(String),
    Reasoning(String),
    Restart,
}

type PieceSender = Arc<std::sync::Mutex<Option<mpsc::UnboundedSender<ReplyPiece>>>>;

/// A streamed call's text on its way to the tabs of the session its turn
/// answers (issue #162), or to the chat app's conversation it answers
/// (issue #325). Dropping it, on any path, forgets the call.
struct ReplyText {
    sink: Box<llm_proxy::TextSink<'static>>,
    sender: PieceSender,
    forward: Option<tokio::task::JoinHandle<()>>,
}

impl ReplyText {
    /// For a request whose turn answers a dashboard session of this agent,
    /// or a chat app's conversation whose adapter shows replies as they
    /// are written; `None` for any other.
    async fn open(
        ctx: &AgentSocketCtx,
        agent_id: AgentId,
        req: &LlmRequest,
        listed: &Listed,
    ) -> Option<Self> {
        let asked = &listed.asked;
        let target = req.reply.as_ref()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let forward = match target.channel_id.parse::<crate::webchat::SessionId>() {
            Ok(session) => {
                if ctx.webchat_router.agent_for_session(session).await != Some(agent_id) {
                    return None;
                }
                let conversation = target
                    .conversation
                    .clone()
                    .filter(|c| c.len() <= crate::webchat::MAX_CONVERSATION_ID);
                tokio::spawn(forward_reply_text(
                    ctx.webchat_router.clone(),
                    agent_id,
                    session,
                    conversation,
                    req.request_id.clone(),
                    rx,
                    Arc::clone(asked),
                ))
            }
            Err(_) => {
                let shown = ChatWriting::open(ctx, agent_id, listed.connection, target).await?;
                tokio::spawn(forward_chat_text(shown, rx, Arc::clone(asked)))
            }
        };
        let sender: PieceSender = Arc::new(std::sync::Mutex::new(Some(tx)));
        let pieces = Arc::clone(&sender);
        let asked = Arc::clone(asked);
        let sink: Box<llm_proxy::TextSink<'static>> = Box::new(move |event| {
            if asked.dropped.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let piece = match event {
                llm_proxy::provider::TextEvent::Text(text) => ReplyPiece::Text(text.to_string()),
                llm_proxy::provider::TextEvent::Reasoning(text) => {
                    ReplyPiece::Reasoning(text.to_string())
                }
                llm_proxy::provider::TextEvent::Restart => ReplyPiece::Restart,
                llm_proxy::provider::TextEvent::ToolInput(_) => return,
            };
            let pieces = pieces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(tx) = pieces.as_ref() {
                let _ = tx.send(piece);
            }
        });
        Some(Self {
            sink,
            sender,
            forward: Some(forward),
        })
    }

    /// No more text: the forwarder sends what is left and ends.
    fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Pass on what is left and stop.
    async fn finish(mut self) {
        self.close();
        if let Some(forward) = self.forward.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), forward).await;
        }
    }
}

impl Drop for ReplyText {
    fn drop(&mut self) {
        self.close();
    }
}

/// One text a call streams, its reply or its reasoning, and how much of
/// it the tabs were sent, in bytes and in the UTF-16 units a browser
/// counts.
#[derive(Default)]
struct Streamed {
    whole: String,
    sent_bytes: usize,
    sent_units: usize,
    restart: bool,
}

impl Streamed {
    /// What to send now, as `(at, text)`: what was added since, or all of
    /// it after a restart or when `whole_due`.
    fn next(&mut self, whole_due: bool) -> Option<(usize, String)> {
        let (at, text) = if self.restart || (whole_due && !self.whole.is_empty()) {
            (0, self.whole.clone())
        } else if self.whole.len() > self.sent_bytes {
            (self.sent_units, self.whole[self.sent_bytes..].to_string())
        } else {
            return None;
        };
        self.sent_units = if at == 0 {
            self.whole.encode_utf16().count()
        } else {
            self.sent_units + text.encode_utf16().count()
        };
        self.sent_bytes = self.whole.len();
        self.restart = false;
        Some((at, text))
    }
}

/// Send a call's text and reasoning to the session's tabs, gathered for
/// [`REPLY_TEXT_EVERY`] at a time: what each added, after the length the
/// tabs were last sent, or the whole of it at least every
/// [`REPLY_TEXT_WHOLE_EVERY`] and after a restart.
async fn forward_reply_text(
    router: WebChatRouter,
    agent_id: AgentId,
    session: crate::webchat::SessionId,
    conversation: Option<String>,
    call: String,
    mut rx: mpsc::UnboundedReceiver<ReplyPiece>,
    asked: Arc<Asked>,
) {
    let mut text = Streamed::default();
    let mut reasoning = Streamed::default();
    let mut last_whole = std::time::Instant::now();
    let mut open = true;
    while open {
        let Some(first) = rx.recv().await else {
            break;
        };
        let add = |piece: ReplyPiece, text: &mut Streamed, reasoning: &mut Streamed| match piece {
            ReplyPiece::Text(piece) => text.whole.push_str(&piece),
            ReplyPiece::Reasoning(piece) => reasoning.whole.push_str(&piece),
            ReplyPiece::Restart => {
                for s in [&mut *text, &mut *reasoning] {
                    s.whole.clear();
                    s.restart = true;
                }
            }
        };
        add(first, &mut text, &mut reasoning);
        let flush = tokio::time::sleep(REPLY_TEXT_EVERY);
        tokio::pin!(flush);
        loop {
            tokio::select! {
                () = &mut flush => break,
                piece = rx.recv() => match piece {
                    Some(piece) => add(piece, &mut text, &mut reasoning),
                    None => {
                        open = false;
                        break;
                    }
                },
            }
        }
        if asked.dropped.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        let whole_due = last_whole.elapsed() >= REPLY_TEXT_WHOLE_EVERY;
        if whole_due {
            last_whole = std::time::Instant::now();
        }
        for (stream, thinking) in [(&mut reasoning, true), (&mut text, false)] {
            if let Some((at, piece)) = stream.next(whole_due) {
                let _ = router
                    .reply_text(
                        agent_id,
                        session,
                        conversation.clone(),
                        &call,
                        piece,
                        at,
                        thinking,
                    )
                    .await;
            }
        }
    }
}

/// How often a chat app is shown what a call has written, at most (issue
/// #325): its adapter edits a message for each, and chat apps allow about
/// one edit a second.
const CHAT_TEXT_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Replies being written in chat apps that one agent has at most (issue
/// #325): past them, a reply is shown once written.
const MAX_CHAT_REPLIES: usize = 32;

/// How long the end of a reply written in a chat app is remembered (issue
/// #325): a call of its turn that comes after it is not shown.
const CHAT_REPLY_ENDED_KEPT: std::time::Duration = std::time::Duration::from_secs(300);

/// How often a chat app is told a reply is still being written while
/// nothing new is shown (issue #325): its turn thinks, writes a tool's
/// input, runs a tool or waits for an approval. The adapter keeps it,
/// with its Stop button, as long as these come.
const CHAT_REPLY_ALIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// A reply a chat app is shown as it is written (issue #325).
#[derive(Debug, Clone)]
struct ChatReply {
    /// The id the adapter knows it by.
    reply: String,
    /// The runtime's turn it is the reply of.
    turn: String,
    /// The connection whose runtime writes it.
    connection: u64,
    /// When it ended; `None` while it is written.
    ended: Option<std::time::Instant>,
    /// Whether the adapter is told it is still being written.
    kept_alive: bool,
}

/// An agent, a conversation and one of its chats.
type ChatKey = (AgentId, String, String);

/// What [`ChatReplies`] holds.
#[derive(Debug, Default)]
struct ChatRepliesHeld {
    replies: HashMap<ChatKey, ChatReply>,
    /// The agents that write [`MAX_CHAT_REPLIES`], said once.
    full: std::collections::HashSet<AgentId>,
}

/// The replies chat apps are shown as they are written (issue #325), by
/// agent, conversation and chat: one per turn, from its first call until
/// the runtime sends its last part or its connection ends.
#[derive(Debug, Clone, Default)]
pub struct ChatReplies(Arc<std::sync::Mutex<ChatRepliesHeld>>);

impl ChatReplies {
    fn lock(&self) -> std::sync::MutexGuard<'_, ChatRepliesHeld> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The id of the reply `turn` writes in chat `chat` of `channel_id`
    /// over `connection`: the one an earlier call of the turn was given, or
    /// a new one; `None` when that reply ended, came over another
    /// connection, or the agent writes [`MAX_CHAT_REPLIES`] already. With
    /// it, the id of an earlier turn's reply there that never ended, which
    /// ends now.
    fn open(
        &self,
        agent_id: AgentId,
        connection: u64,
        channel_id: &str,
        chat: &str,
        turn: &str,
    ) -> (Option<String>, Option<String>) {
        let now = std::time::Instant::now();
        let mut held = self.lock();
        held.replies.retain(|_, r| {
            r.ended
                .is_none_or(|at| now.duration_since(at) < CHAT_REPLY_ENDED_KEPT)
        });
        let key = (agent_id, channel_id.to_string(), chat.to_string());
        let mut stale = None;
        if let Some(found) = held.replies.get(&key) {
            if found.turn == turn {
                let open = found.ended.is_none() && found.connection == connection;
                return (open.then(|| found.reply.clone()), None);
            }
            if found.ended.is_none() {
                stale = Some(found.reply.clone());
            }
            held.replies.remove(&key);
        }
        let written = held
            .replies
            .iter()
            .filter(|((agent, _, _), r)| *agent == agent_id && r.ended.is_none())
            .count();
        if written >= MAX_CHAT_REPLIES {
            if held.full.insert(agent_id) {
                warn!(
                    %agent_id,
                    max = MAX_CHAT_REPLIES,
                    "the agent writes as many replies in chat apps as it may at once; \
                     the next are shown once written"
                );
            }
            return (None, stale);
        }
        held.full.remove(&agent_id);
        let reply = Uuid::now_v7().to_string();
        held.replies.insert(
            key,
            ChatReply {
                reply: reply.clone(),
                turn: turn.to_string(),
                connection,
                ended: None,
                kept_alive: false,
            },
        );
        (Some(reply), stale)
    }

    /// Whether `reply` is still being written there.
    fn is_open(&self, agent_id: AgentId, channel_id: &str, chat: &str, reply: &str) -> bool {
        self.lock()
            .replies
            .get(&(agent_id, channel_id.to_string(), chat.to_string()))
            .is_some_and(|r| r.reply == reply && r.ended.is_none())
    }

    /// Whether the adapter is to be told, from now on, that `reply` is
    /// still being written: true once, while it is.
    fn keep_alive(&self, agent_id: AgentId, channel_id: &str, chat: &str, reply: &str) -> bool {
        self.lock()
            .replies
            .get_mut(&(agent_id, channel_id.to_string(), chat.to_string()))
            .filter(|r| r.reply == reply && r.ended.is_none())
            .is_some_and(|r| !std::mem::replace(&mut r.kept_alive, true))
    }

    /// The id of the reply being written there, if any; it ends when this
    /// is its `last` part.
    fn of(&self, agent_id: AgentId, channel_id: &str, chat: &str, last: bool) -> Option<String> {
        let key = (agent_id, channel_id.to_string(), chat.to_string());
        let mut held = self.lock();
        let found = held.replies.get_mut(&key).filter(|r| r.ended.is_none())?;
        if last {
            found.ended = Some(std::time::Instant::now());
        }
        Some(found.reply.clone())
    }

    /// The replies `connection`'s runtime was writing, as `(channel_id,
    /// reply)`: they end.
    fn end_connection(&self, agent_id: AgentId, connection: u64) -> Vec<(String, String)> {
        let now = std::time::Instant::now();
        self.lock()
            .replies
            .iter_mut()
            .filter(|((agent, _, _), r)| {
                *agent == agent_id && r.connection == connection && r.ended.is_none()
            })
            .map(|((_, channel_id, _), r)| {
                r.ended = Some(now);
                (channel_id.clone(), r.reply.clone())
            })
            .collect()
    }
}

/// What a chat app's conversation is shown of a call of the reply being
/// written there (issue #325).
#[derive(Clone)]
struct ChatWriting {
    router: crate::channel_router::ChannelRouter,
    replies: ChatReplies,
    agent_id: AgentId,
    channel: String,
    channel_id: String,
    chat: String,
    frame: havn_proto::channel::ChannelWriting,
    /// Shown while the call has written nothing.
    waiting: String,
}

impl ChatWriting {
    /// For a call of a turn that marks its reply's last part, over
    /// `connection`, answering a chat app's conversation the agent may send
    /// to, whose adapter shows replies as they are written; `None` for any
    /// other. The first call of a reply has the adapter told, every
    /// [`CHAT_REPLY_ALIVE_EVERY`], that it is still being written.
    async fn open(
        ctx: &AgentSocketCtx,
        agent_id: AgentId,
        connection: u64,
        target: &havn_proto::ReplyStream,
    ) -> Option<Self> {
        if !target.marks_last {
            return None;
        }
        let turn = target.turn.as_deref()?;
        let chat = target
            .conversation
            .clone()
            .filter(|c| c.len() <= crate::webchat::MAX_CONVERSATION_ID)?;
        let (channel, account_id, to) = crate::channel_id_codec::decode(&target.channel_id)?;
        let key = crate::channel_router::AccountKey::new(channel, account_id);
        if !ctx.channel_router.takes_writing(&key).await {
            return None;
        }
        channel_send_permitted(ctx, agent_id, channel, account_id, to)
            .await
            .ok()?;
        let language = agents_repo::find_by_id(&ctx.db, agent_id)
            .await
            .ok()
            .flatten()
            .map(|agent| havn_core::Language::of_config(&agent.config))
            .unwrap_or_default();
        let (reply, stale) =
            ctx.chat_replies
                .open(agent_id, connection, &target.channel_id, &chat, turn);
        if let Some(stale) = stale {
            end_chat_reply(&ctx.channel_router, &target.channel_id, stale).await;
        }
        let shown = Self {
            router: ctx.channel_router.clone(),
            replies: ctx.chat_replies.clone(),
            agent_id,
            channel: channel.to_string(),
            channel_id: target.channel_id.clone(),
            chat,
            frame: havn_proto::channel::ChannelWriting {
                account_id: account_id.to_string(),
                to: to.to_string(),
                in_reply_to: target.answers.clone(),
                reply: reply?,
                text: String::new(),
                buttons: vec![vec![havn_core::Button {
                    label: language.pick("Stop", "停止").to_string(),
                    data: "/stop".to_string(),
                }]],
                waiting: false,
                alive: false,
            },
            waiting: language.pick("Replying…", "正在回复…").to_string(),
        };
        if shown
            .replies
            .keep_alive(agent_id, &shown.channel_id, &shown.chat, &shown.frame.reply)
        {
            tokio::spawn(keep_chat_reply_alive(shown.clone()));
        }
        Some(shown)
    }

    /// Queue `frame` for the adapter while its reply is still being
    /// written (the runtime did not send its last part): whether it is.
    /// That is checked as the frame is queued, so none goes after the
    /// reply's end.
    async fn send(&self, frame: havn_proto::channel::ChannelWriting) -> bool {
        let (replies, agent_id) = (&self.replies, self.agent_id);
        let (channel_id, chat, reply) = (&self.channel_id, &self.chat, &self.frame.reply);
        let mut open = true;
        self.router
            .writing(&self.channel, frame, || {
                open = replies.is_open(agent_id, channel_id, chat, reply);
                open
            })
            .await;
        open
    }

    /// Show `text`, or that the reply is being written when it is empty;
    /// whether its reply is still being written.
    async fn show(&mut self, text: &str) -> bool {
        self.frame.waiting = text.trim().is_empty();
        self.frame.text = if self.frame.waiting {
            self.waiting.clone()
        } else {
            text.to_string()
        };
        self.send(self.frame.clone()).await
    }
}

/// Tell the adapter of `shown`'s reply, every [`CHAT_REPLY_ALIVE_EVERY`],
/// that it is still being written, until it is not (issue #325).
async fn keep_chat_reply_alive(shown: ChatWriting) {
    let alive = havn_proto::channel::ChannelWriting {
        text: String::new(),
        buttons: Vec::new(),
        waiting: false,
        alive: true,
        ..shown.frame.clone()
    };
    loop {
        tokio::time::sleep(CHAT_REPLY_ALIVE_EVERY).await;
        // Asked each time, whether or not the frame can go: a reply that
        // ended while its adapter was away, or did not take such frames,
        // ends this too.
        let open = shown.replies.is_open(
            shown.agent_id,
            &shown.channel_id,
            &shown.chat,
            &shown.frame.reply,
        );
        if !open || !shown.send(alive.clone()).await {
            return;
        }
    }
}
/// Show a call's text in a chat app (issue #325): at once that the reply
/// is being written, then all the call has written, at most every
/// [`CHAT_TEXT_EVERY`]. What it wrote last is not shown when it ends: the
/// part of the reply the runtime sends next takes its place.
async fn forward_chat_text(
    mut shown: ChatWriting,
    mut rx: mpsc::UnboundedReceiver<ReplyPiece>,
    asked: Arc<Asked>,
) {
    let add = |piece: ReplyPiece, text: &mut String| match piece {
        ReplyPiece::Text(piece) => text.push_str(&piece),
        ReplyPiece::Reasoning(_) => {}
        ReplyPiece::Restart => text.clear(),
    };
    if !shown.show("").await {
        return;
    }
    let (mut text, mut sent) = (String::new(), String::new());
    while let Some(first) = rx.recv().await {
        add(first, &mut text);
        let flush = tokio::time::sleep(CHAT_TEXT_EVERY);
        tokio::pin!(flush);
        loop {
            tokio::select! {
                () = &mut flush => break,
                piece = rx.recv() => match piece {
                    Some(piece) => add(piece, &mut text),
                    None => return,
                },
            }
        }
        if asked.dropped.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if text != sent {
            if !shown.show(&text).await {
                return;
            }
            sent.clone_from(&text);
        }
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
        reasoning: None,
        messages,
        system_stable: req
            .options
            .get("system_stable")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0),
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

    #[test]
    fn approval_buttons_name_their_request() {
        let id = "0198e1c2-7a4b-7c3d-9e8f-0a1b2c3d4e5f";
        let rows = approval_buttons(id);
        let data: Vec<&str> = rows.iter().flatten().map(|b| b.data.as_str()).collect();
        assert_eq!(
            data,
            [
                format!("/approve {id}"),
                format!("/approve always {id}"),
                format!("/deny {id}")
            ]
        );
        assert!(data.iter().all(|d| d.len() <= havn_core::MAX_BUTTON_DATA));
        assert!(
            approval_buttons(&"x".repeat(60)).is_empty(),
            "too long for a button"
        );
        for odd in ["always", "ALWAYS", "a b", "a\nb", ""] {
            assert!(approval_buttons(odd).is_empty(), "{odd:?}");
        }
        let mut msg = OutboundMessage {
            channel_id: "ch|telegram|bot|tg:1".into(),
            content: havn_core::MessageContent::Text {
                text: "Done!".into(),
            },
            reply_to: None,
            conversation: None,
            buttons: approval_buttons(id),
            poll: None,
            partial: false,
            last: false,
        };
        msg = from_agent(msg);
        assert!(msg.buttons.is_empty(), "an agent's buttons are dropped");
    }
    use havn_db::connect_in_memory;
    use havn_db::repo::agents::{NewAgent, create as create_agent};
    use havn_db::repo::users::{NewUser, create as create_user};
    use havn_proto::AgentQueryOutcome;
    use havn_spawner::AgentHandle;
    use std::time::Duration;

    #[test]
    fn a_streamed_text_is_sent_in_pieces_and_whole_after_a_restart() {
        let mut s = Streamed::default();
        assert_eq!(s.next(false), None, "nothing yet");
        assert_eq!(s.next(true), None, "an empty text is not sent whole");
        s.whole.push_str("héllo");
        assert_eq!(s.next(false), Some((0, "héllo".into())));
        s.whole.push_str(" wörld");
        assert_eq!(
            s.next(false),
            Some((5, " wörld".into())),
            "after the UTF-16 units sent"
        );
        assert_eq!(s.next(false), None);
        assert_eq!(s.next(true), Some((0, "héllo wörld".into())));
        s.whole.clear();
        s.restart = true;
        assert_eq!(
            s.next(false),
            Some((0, String::new())),
            "a restart clears the tabs"
        );
        assert_eq!(s.next(false), None);
    }

    #[test]
    fn the_title_model_is_the_teams_then_the_gateways_when_allowed() {
        let entry = |id: &str| -> crate::model_catalog::ModelEntry {
            serde_json::from_value(serde_json::json!({ "id": id })).expect("entry")
        };
        let mut catalog = crate::model_catalog::ModelCatalog {
            default_model: None,
            title_model: Some("openai:small".into()),
            models: vec![
                entry("openai:small"),
                entry("openai:big"),
                entry("openai:team"),
            ],
        };
        let mut policy = Policy::default();
        assert_eq!(title_model(&policy, &catalog), "openai:small");
        policy.chat_titles.model = "openai:team".into();
        assert_eq!(title_model(&policy, &catalog), "openai:team");
        // Not allowed to the agent, or not listed: its own model.
        policy.allowed_models = vec!["openai:big".into()];
        assert_eq!(title_model(&policy, &catalog), "");
        policy.allowed_models = vec!["*".into()];
        policy.chat_titles.model = "openai:gone".into();
        assert_eq!(title_model(&policy, &catalog), "");
        policy.chat_titles.model.clear();
        catalog.title_model = None;
        assert_eq!(title_model(&policy, &catalog), "");
    }

    async fn ctx_with(multi_user: bool, verify_peer_cgroup: bool) -> AgentSocketCtx {
        let db = connect_in_memory().await.expect("db");
        let registry = AgentRegistry::new();
        let webchat_router = WebChatRouter::new();
        let secrets = crate::secrets::SecretStore::new(db.clone(), None);
        AgentSocketCtx {
            vision_refused: VisionRefused::default(),
            attachments: Arc::default(),
            workspace_root: std::env::temp_dir(),
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
            channels: Default::default(),
            admin_rpc: crate::admin_rpc::AdminRpc::new(),
            query_broker: crate::query_broker::QueryBroker::new(),
            embedding_config: Arc::new(arc_swap::ArcSwap::from_pointee(
                serde_json::json!({"provider": "disabled"}),
            )),
            multi_user,
            reply_streams: ReplyStreams::default(),
            chat_replies: ChatReplies::default(),
            llm: Arc::new(crate::config::LlmConfig::default()),
            models: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::model_catalog::ModelCatalog::default(),
            )),
            verify_peer_cgroup,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
            keyring: None,
            secrets,
            search: Arc::new(crate::web_search::WebSearch::new(
                crate::config::SearchConfig::default(),
                &crate::config::LlmConfig::default(),
            )),
            fetch: Arc::new(
                crate::web_fetch::WebFetch::new(crate::config::FetchConfig::default())
                    .expect("fetch client"),
            ),
            timezone_rate: Arc::new(crate::web_search::UserRate::new(
                crate::timezone::TOOL_PER_USER_PER_MINUTE,
            )),
            egress_proxy: None,
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
        ctx.registry.register(AgentHandle::new(agent.id, 1)).await;
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

    async fn add_openrouter_key(ctx: &AgentSocketCtx, owner: UserId) {
        havn_db::repo::credentials::create(
            &ctx.db,
            havn_db::repo::credentials::NewCredential {
                scope: havn_db::repo::credentials::CredentialScope::User,
                scope_id: &owner.to_string(),
                provider: "llm:openrouter",
                name: None,
                api_key_ciphertext: b"unused",
                priority: 0,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("credential");
        ctx.models
            .store(Arc::new(crate::model_catalog::ModelCatalog {
                default_model: None,
                title_model: None,
                models: vec![crate::model_catalog::ModelEntry {
                    id: "openrouter:cheap/model".into(),
                    label: None,
                    max_output_tokens: None,
                    vision: None,
                    thinking: None,
                    context_window: None,
                    fallback: Vec::new(),
                    price: None,
                }],
            }));
    }

    fn policy_without_search() -> serde_json::Value {
        let mut p = Policy::default();
        p.permissions.can_search_web = false;
        p.permissions.can_fetch_web = false;
        serde_json::json!({ "policy": p })
    }

    #[tokio::test]
    async fn web_search_is_offered_with_a_backend_and_the_permission() {
        let ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, _rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        let (denied, _rx2) = connected_agent(&ctx, owner, "b", policy_without_search()).await;
        let tools_of = |id: AgentId| {
            let ctx = ctx.clone();
            async move {
                let agent = agents_repo::find_by_id(&ctx.db, id)
                    .await
                    .expect("lookup")
                    .expect("agent");
                let policy = policy_resolver::for_session(&ctx.db, &agent, false).await;
                gateway_tools_for(&ctx, &agent, &policy)
                    .await
                    .into_iter()
                    .map(|t| t.name)
                    .collect::<Vec<_>>()
            }
        };

        assert_eq!(
            tools_of(agent).await,
            [
                "set_timezone",
                "send_file",
                "ask_choice",
                "web_fetch",
                "schedule"
            ],
            "no search backend yet"
        );
        add_openrouter_key(&ctx, owner).await;
        assert_eq!(
            tools_of(agent).await,
            [
                "set_timezone",
                "send_file",
                "ask_choice",
                "web_search",
                "web_fetch",
                "schedule"
            ]
        );
        assert_eq!(
            tools_of(denied).await,
            ["set_timezone", "send_file", "ask_choice", "schedule"],
            "policy denies search and fetch"
        );
    }

    #[tokio::test]
    async fn web_fetch_calls_are_checked_and_audited() {
        let ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, _rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        let (denied, _rx2) = connected_agent(&ctx, owner, "b", policy_without_search()).await;
        let call = |url: &str| GatewayToolCall {
            request_id: "f1".into(),
            name: "web_fetch".into(),
            input: serde_json::json!({"url": url, "prompt": "what is it"}),
            conversation: None,
            chat: None,
            sender: None,
            from_owner: false,
            in_group: false,
        };

        let refused = run_gateway_tool(denied, &call("https://example.com/"), &ctx).await;
        assert!(refused.expect_err("denied").contains("does not allow"));
        // No model is listed, so even a fetchable URL stops there; a local
        // one is refused the same way once a model exists (web_fetch tests).
        let no_model = run_gateway_tool(agent, &call("https://example.com/"), &ctx).await;
        assert!(no_model.expect_err("no model").contains("no model"));

        let audited: Vec<String> =
            sqlx::query_scalar("SELECT details FROM audit_log WHERE action = 'web.fetch'")
                .fetch_all(&ctx.db)
                .await
                .expect("audit");
        assert_eq!(audited.len(), 1, "a refusal by policy is not audited");
        assert!(audited[0].contains("example.com"), "{}", audited[0]);
    }

    #[tokio::test]
    async fn gateway_tool_calls_are_checked_and_audited() {
        let ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, mut rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        let (denied, _rx2) = connected_agent(&ctx, owner, "b", policy_without_search()).await;
        let call = |name: &str| GatewayToolCall {
            request_id: "t1".into(),
            name: name.into(),
            input: serde_json::json!({"query": "havn"}),
            conversation: None,
            chat: None,
            sender: None,
            from_owner: false,
            in_group: false,
        };

        let unknown = run_gateway_tool(agent, &call("shell"), &ctx).await;
        assert!(unknown.expect_err("unknown").contains("no tool named"));
        let refused = run_gateway_tool(denied, &call("web_search"), &ctx).await;
        assert!(refused.expect_err("denied").contains("does not allow"));

        handle_gateway_tool_call(agent, call("web_search"), ctx.clone()).await;
        match rx.try_recv().expect("a result") {
            GatewayToAgent::GatewayToolResult(r) => {
                assert_eq!(r.request_id, "t1");
                let GatewayToolOutcome::Error { message } = r.outcome else {
                    panic!("expected an error: {:?}", r.outcome);
                };
                assert!(message.contains("not set up"), "{message}");
            }
            other => panic!("unexpected frame {other:?}"),
        }
        let audited: Vec<(String, String)> =
            sqlx::query_as("SELECT agent_id, details FROM audit_log WHERE action = 'web.search'")
                .fetch_all(&ctx.db)
                .await
                .expect("audit rows");
        assert_eq!(audited.len(), 1, "{audited:?}");
        assert_eq!(audited[0].0, agent.to_string());
        assert!(
            audited[0].1.contains("\"query\":\"havn\""),
            "{}",
            audited[0].1
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

    async fn llm_error_for(
        catalog: &[&str],
        agent_config: serde_json::Value,
        model: &str,
    ) -> String {
        let ctx = ctx_with(false, false).await;
        ctx.models
            .store(Arc::new(crate::model_catalog::ModelCatalog {
                default_model: None,
                title_model: None,
                models: catalog
                    .iter()
                    .map(|id| crate::model_catalog::ModelEntry {
                        id: (*id).into(),
                        label: None,
                        max_output_tokens: None,
                        vision: None,
                        thinking: None,
                        context_window: None,
                        fallback: Vec::new(),
                        price: None,
                    })
                    .collect(),
            }));
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, mut rx) = connected_agent(&ctx, owner, "a", agent_config).await;
        let mut req = llm_request("r1");
        req.model = model.into();
        handle_llm_request(agent, req, ctx.clone(), None).await;
        match rx.try_recv().expect("a reply") {
            GatewayToAgent::LlmResponse(LlmResponse {
                outcome: LlmOutcome::Error { message },
                ..
            }) => message,
            other => panic!("unexpected frame {other:?}"),
        }
    }

    #[tokio::test]
    async fn llm_requests_need_a_listed_model() {
        let none = llm_error_for(&[], serde_json::json!({}), "").await;
        assert!(none.contains("no model is configured"), "{none}");
        let unlisted =
            llm_error_for(&["openai:gpt-5"], serde_json::json!({}), "openai:gpt-4o").await;
        assert!(
            unlisted.contains("not in the gateway's model list"),
            "{unlisted}"
        );
        let bare = llm_error_for(&["openai:gpt-5"], serde_json::json!({}), "gpt-5").await;
        assert!(bare.contains("not in the gateway's model list"), "{bare}");
    }

    #[tokio::test]
    async fn the_policy_narrows_the_model_list() {
        let config = serde_json::json!({"policy": {"allowed_models": ["anthropic:claude-x"]}});
        let refused = llm_error_for(
            &["openai:gpt-5", "anthropic:claude-x"],
            config.clone(),
            "openai:gpt-5",
        )
        .await;
        assert!(
            refused.contains("not allowed by this agent's policy"),
            "{refused}"
        );
        // A listed, allowed model gets past both checks to the credential
        // lookup, which fails here only because the test has no keyring.
        let passed = llm_error_for(
            &["openai:gpt-5", "anthropic:claude-x"],
            config,
            "anthropic:claude-x",
        )
        .await;
        assert!(passed.contains("credential encryption"), "{passed}");
    }

    #[tokio::test]
    async fn an_empty_model_takes_the_current_default() {
        let config = serde_json::json!({"policy": {"allowed_models": ["anthropic:claude-x"]}});
        let first =
            llm_error_for(&["anthropic:claude-x", "openai:gpt-5"], config.clone(), "").await;
        assert!(first.contains("credential encryption"), "{first}");
        let other = llm_error_for(&["openai:gpt-5", "anthropic:claude-x"], config, "").await;
        assert!(
            other.contains("not allowed by this agent's policy"),
            "{other}"
        );
    }

    fn llm_request(request_id: &str) -> LlmRequest {
        LlmRequest {
            request_id: request_id.into(),
            model: "claude-test".into(),
            messages: serde_json::json!([{ "role": "user", "content": "hi" }]),
            options: serde_json::json!({}),
            billing_user_id: None,
            reply: None,
        }
    }

    #[tokio::test]
    async fn a_cancel_that_keeps_its_call_cuts_it_and_one_that_does_not_only_hides_its_text() {
        let ctx = ctx_with(false, false).await;
        let agent = AgentId::new();
        let of_a_turn = |request_id: &str| LlmRequest {
            reply: Some(havn_proto::ReplyStream {
                channel_id: "session".into(),
                conversation: None,
                answers: None,
                marks_last: false,
                turn: None,
            }),
            ..llm_request(request_id)
        };
        let dropped = Listed::open(&ctx.reply_streams, agent, &of_a_turn("r1")).expect("listed");
        let cut = Listed::open(&ctx.reply_streams, agent, &of_a_turn("r2")).expect("listed");
        // As a runtime before issue #305 sends it.
        dispatch(
            agent,
            serde_json::from_str(r#"{"type":"llm_cancel","request_id":"r1"}"#).expect("frame"),
            &ctx,
        )
        .await;
        dispatch(
            agent,
            AgentToGateway::LlmCancel {
                request_id: "r2".into(),
                keep: true,
            },
            &ctx,
        )
        .await;
        let hidden = |listed: &Listed| {
            listed
                .asked
                .dropped
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        assert!(hidden(&dropped) && !dropped.asked.cut.is_cancelled());
        assert!(!hidden(&cut) && cut.asked.cut.is_cancelled());
        // Another agent's cancel does not reach them.
        let other = Listed::open(&ctx.reply_streams, agent, &of_a_turn("r3")).expect("listed");
        dispatch(
            AgentId::new(),
            AgentToGateway::LlmCancel {
                request_id: "r3".into(),
                keep: true,
            },
            &ctx,
        )
        .await;
        assert!(!other.asked.cut.is_cancelled());
        drop((dropped, cut, other));
        assert!(ctx.reply_streams.lock().unwrap().is_empty(), "forgotten");
        assert!(
            Listed::open(&ctx.reply_streams, agent, &llm_request("r4")).is_none(),
            "a call of no turn cannot be stopped"
        );
    }

    #[tokio::test]
    async fn a_stopped_call_answers_with_the_text_and_reasoning_the_tabs_were_shown() {
        // A provider that writes some reasoning and text, then nothing more.
        let events = [
            r#"{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Which first?"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Half of"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"shell","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"x\"}"}}"#,
        ];
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || async move {
                let written = futures_util::stream::iter([Ok::<_, std::convert::Infallible>(body)]);
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(futures_util::StreamExt::chain(
                        written,
                        futures_util::stream::pending(),
                    )),
                )
            }),
        );
        let (ctx, server) = ctx_calling(app).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, mut rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        let session = Uuid::now_v7();
        let (tab_tx, mut tab) = mpsc::channel(64);
        let _tab = ctx.webchat_router.register(session, agent, tab_tx).await;

        let req = LlmRequest {
            model: "lan:m".into(),
            reply: Some(havn_proto::ReplyStream {
                channel_id: session.to_string(),
                conversation: None,
                answers: None,
                marks_last: true,
                turn: None,
            }),
            ..llm_request("r1")
        };
        dispatch(agent, AgentToGateway::LlmRequest(req), &ctx).await;
        // The tab is shown what it wrote.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(crate::webchat::ServerMessage::ReplyText { text, thinking, .. }) =
                    tab.recv().await
                    && !thinking
                    && text.contains("Half of")
                {
                    break;
                }
            }
        })
        .await
        .expect("the text is shown");
        dispatch(
            agent,
            AgentToGateway::LlmCancel {
                request_id: "r1".into(),
                keep: true,
            },
            &ctx,
        )
        .await;
        let answer = next_llm_response(&mut rx).await;
        server.abort();
        assert_eq!(answer.request_id, "r1");
        assert!(
            matches!(answer.outcome, LlmOutcome::Ok),
            "{:?}",
            answer.outcome
        );
        assert_eq!(answer.provider_response["stop_reason"], llm_proxy::STOPPED);
        assert_eq!(
            answer.provider_response["content"],
            serde_json::json!([
                {"type": "thinking", "thinking": "Which first?"},
                {"type": "text", "text": "Half of"}
            ])
        );
        forgotten(&ctx).await;
        // Its use as estimated: what it wrote, the tool call's input too.
        let used: Vec<(i64,)> = sqlx::query_as("SELECT tokens_out FROM credential_usages")
            .fetch_all(&ctx.db)
            .await
            .expect("rows");
        assert_eq!(used, vec![(14,)]);
    }

    #[tokio::test]
    async fn a_stopped_call_shown_to_nobody_runs_on_and_records_what_it_used() {
        // A provider that answers whole, once let.
        let (asked, answer_now) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post({
                let (asked, answer_now) = (Arc::clone(&asked), Arc::clone(&answer_now));
                move || async move {
                    asked.notify_one();
                    answer_now.notified().await;
                    axum::Json(serde_json::json!({
                        "id": "m1",
                        "model": "m",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "The whole answer."}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 5, "output_tokens": 7}
                    }))
                }
            }),
        );
        let (ctx, server) = ctx_calling(app).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, mut rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        // A chat app's conversation: its reply is not shown as it is written.
        let req = LlmRequest {
            model: "lan:m".into(),
            reply: Some(havn_proto::ReplyStream {
                channel_id: "telegram:123".into(),
                conversation: None,
                answers: None,
                marks_last: true,
                turn: None,
            }),
            ..llm_request("r1")
        };
        dispatch(agent, AgentToGateway::LlmRequest(req), &ctx).await;
        tokio::time::timeout(Duration::from_secs(10), asked.notified())
            .await
            .expect("the provider is asked");
        dispatch(
            agent,
            AgentToGateway::LlmCancel {
                request_id: "r1".into(),
                keep: true,
            },
            &ctx,
        )
        .await;
        // Answered at once, with nothing written.
        let answer = next_llm_response(&mut rx).await;
        assert!(
            matches!(answer.outcome, LlmOutcome::Ok),
            "{:?}",
            answer.outcome
        );
        assert_eq!(answer.provider_response["stop_reason"], llm_proxy::STOPPED);
        assert_eq!(answer.provider_response["content"], serde_json::json!([]));
        // The call runs on and records what it used, not its reservation.
        answer_now.notify_one();
        forgotten(&ctx).await;
        server.abort();
        let used: Vec<(i64, i64)> =
            sqlx::query_as("SELECT tokens_in, tokens_out FROM credential_usages")
                .fetch_all(&ctx.db)
                .await
                .expect("rows");
        assert_eq!(used, vec![(5, 7)]);
        // The runtime is not answered twice.
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|frame| matches!(frame, GatewayToAgent::LlmResponse(_))),
            "a second answer"
        );
    }

    /// Bind `agent` to Telegram account `bot`, open to anyone, with an
    /// adapter that shows replies as they are written when `writing`: what
    /// it is sent.
    async fn telegram_adapter(
        ctx: &mut AgentSocketCtx,
        agent: AgentId,
        writing: bool,
    ) -> mpsc::Receiver<havn_proto::channel::ChannelFrame> {
        ctx.bindings = Arc::new(vec![crate::config::ChannelBindingConfig {
            agent_id: agent.to_string(),
            channel: "telegram".into(),
            account_id: "bot".into(),
        }]);
        let mut channels = std::collections::BTreeMap::new();
        channels.insert(
            "telegram".to_string(),
            crate::config::ChannelEntry {
                accounts: vec![crate::config::ChannelAccount {
                    id: "bot".into(),
                    adapter_token_ref: "secret:channel:telegram:bot".into(),
                    dm_policy: crate::config::DmPolicy::Open,
                    allow_from: Vec::new(),
                    admins: Vec::new(),
                    bot_username: None,
                    group_policy: crate::config::GroupPolicy::Off,
                    allow_groups: Vec::new(),
                }],
            },
        );
        ctx.channels.store(Arc::new(channels));
        let (adapter_tx, adapter) = mpsc::channel(64);
        ctx.channel_router
            .register_as(
                crate::channel_router::AccountKey::new("telegram", "bot"),
                adapter_tx,
                false,
                writing,
            )
            .await;
        adapter
    }

    /// The next frame an adapter is sent, within 10 seconds.
    async fn next_frame(
        adapter: &mut mpsc::Receiver<havn_proto::channel::ChannelFrame>,
    ) -> havn_proto::channel::ChannelFrame {
        tokio::time::timeout(Duration::from_secs(10), adapter.recv())
            .await
            .expect("a frame")
            .expect("open")
    }

    #[tokio::test]
    async fn a_chat_app_is_shown_the_reply_as_it_is_written_and_a_stop_keeps_it() {
        use havn_proto::channel::ChannelFrame;
        // A provider that writes some text, then nothing more.
        let events = [
            r#"{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Half of"}}"#,
        ];
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || async move {
                let written = futures_util::stream::iter([Ok::<_, std::convert::Infallible>(body)]);
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(futures_util::StreamExt::chain(
                        written,
                        futures_util::stream::pending(),
                    )),
                )
            }),
        );
        let (mut ctx, server) = ctx_calling(app).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, mut rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        let mut adapter = telegram_adapter(&mut ctx, agent, true).await;
        let channel_id = crate::channel_id_codec::encode("telegram", "bot", "tg:1");
        let req = LlmRequest {
            model: "lan:m".into(),
            reply: Some(havn_proto::ReplyStream {
                channel_id: channel_id.clone(),
                conversation: Some("c1".into()),
                answers: None,
                marks_last: true,
                turn: Some("t1".into()),
            }),
            ..llm_request("r1")
        };
        dispatch(agent, AgentToGateway::LlmRequest(req), &ctx).await;
        // That it is being written, at once, with a stop button; then what
        // was written.
        let ChannelFrame::Writing(first) = next_frame(&mut adapter).await else {
            panic!("not a reply being written");
        };
        assert_eq!(
            (first.to.as_str(), first.text.as_str()),
            ("tg:1", "Replying…")
        );
        assert_eq!(first.buttons[0][0].data, "/stop");
        let ChannelFrame::Writing(written) = next_frame(&mut adapter).await else {
            panic!("not a reply being written");
        };
        assert_eq!(written.text, "Half of");
        assert!(first.waiting && !written.waiting && !written.alive);
        assert_eq!(written.reply, first.reply);
        dispatch(
            agent,
            AgentToGateway::LlmCancel {
                request_id: "r1".into(),
                keep: true,
            },
            &ctx,
        )
        .await;
        let answer = next_llm_response(&mut rx).await;
        server.abort();
        assert_eq!(answer.provider_response["stop_reason"], llm_proxy::STOPPED);
        assert_eq!(
            answer.provider_response["content"],
            serde_json::json!([{"type": "text", "text": "Half of"}])
        );
        // The reply's last part ends what was shown, once.
        let end = |text: &str| {
            AgentToGateway::OutboundMessage(OutboundMessage {
                channel_id: channel_id.clone(),
                content: havn_core::MessageContent::Text { text: text.into() },
                buttons: Vec::new(),
                poll: None,
                partial: false,
                last: true,
                reply_to: None,
                conversation: Some("c1".into()),
            })
        };
        dispatch(agent, end("Half of\n\n(stopped)"), &ctx).await;
        let ChannelFrame::Outbound(o) = next_frame(&mut adapter).await else {
            panic!("not a reply");
        };
        assert_eq!(o.reply.as_deref(), Some(first.reply.as_str()));
        dispatch(agent, end("the next one"), &ctx).await;
        let ChannelFrame::Outbound(o) = next_frame(&mut adapter).await else {
            panic!("not a reply");
        };
        assert_eq!(o.reply, None, "a reply not shown as it was written");
        forgotten(&ctx).await;
    }

    #[tokio::test]
    async fn a_reply_is_shown_as_it_is_written_only_where_both_ends_mark_it() {
        let mut ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, _rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        let target = |marks_last: bool, chat: &str, turn: Option<&str>| havn_proto::ReplyStream {
            channel_id: crate::channel_id_codec::encode("telegram", "bot", "tg:1"),
            conversation: Some(chat.into()),
            answers: Some("tg:chat:1:msg:7".into()),
            marks_last,
            turn: turn.map(Into::into),
        };
        async fn open(
            ctx: &AgentSocketCtx,
            agent: AgentId,
            target: havn_proto::ReplyStream,
        ) -> Option<ChatWriting> {
            ChatWriting::open(ctx, agent, 1, &target).await
        }
        // An adapter before issue #325.
        let _older = telegram_adapter(&mut ctx, agent, false).await;
        assert!(
            open(&ctx, agent, target(true, "c1", Some("t1")))
                .await
                .is_none()
        );
        let _adapter = telegram_adapter(&mut ctx, agent, true).await;
        // A runtime before issue #325, which does not mark its reply's end
        // nor name its turn.
        assert!(
            open(&ctx, agent, target(false, "c1", Some("t1")))
                .await
                .is_none()
        );
        assert!(open(&ctx, agent, target(true, "c1", None)).await.is_none());
        // A chat id longer than any the runtime gives.
        let long = "c".repeat(crate::webchat::MAX_CONVERSATION_ID + 1);
        assert!(
            open(&ctx, agent, target(true, &long, Some("t1")))
                .await
                .is_none()
        );
        let shown = open(&ctx, agent, target(true, "c1", Some("t1")))
            .await
            .expect("shown");
        assert_eq!(shown.frame.in_reply_to.as_deref(), Some("tg:chat:1:msg:7"));
        // A conversation the agent may not send to.
        assert!(
            ChatWriting::open(&ctx, AgentId::new(), 1, &target(true, "c1", Some("t1")))
                .await
                .is_none()
        );
        // An agent writes so many at most.
        for n in 2..=MAX_CHAT_REPLIES {
            let chat = format!("c{n}");
            assert!(
                open(&ctx, agent, target(true, &chat, Some("t1")))
                    .await
                    .is_some(),
                "{n}"
            );
        }
        assert!(
            open(&ctx, agent, target(true, "one more", Some("t1")))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn each_turn_has_a_reply_of_its_own_and_a_call_after_its_end_is_not_shown() {
        let replies = ChatReplies::default();
        let agent = AgentId::new();
        let (first, stale) = replies.open(agent, 1, "ch", "c1", "t1");
        let first = first.expect("open");
        assert_eq!(stale, None);
        // The turn's next call shows in the same reply.
        assert_eq!(
            replies.open(agent, 1, "ch", "c1", "t1"),
            (Some(first.clone()), None)
        );
        assert_eq!(replies.of(agent, "ch", "c1", false), Some(first.clone()));
        // Its end, once; a call of the turn after it shows nothing.
        assert_eq!(replies.of(agent, "ch", "c1", true), Some(first.clone()));
        assert_eq!(replies.of(agent, "ch", "c1", true), None);
        assert!(!replies.is_open(agent, "ch", "c1", &first));
        assert_eq!(replies.open(agent, 1, "ch", "c1", "t1"), (None, None));
        // The next turn gets another.
        let (second, stale) = replies.open(agent, 1, "ch", "c1", "t2");
        let second = second.expect("open");
        assert_ne!(second, first);
        assert_eq!(stale, None);
        // A turn whose end never came is ended by the next one.
        let (third, stale) = replies.open(agent, 1, "ch", "c1", "t3");
        assert_eq!(stale, Some(second));
        assert!(third.is_some());
        // A call of the turn over another connection is not shown.
        assert_eq!(replies.open(agent, 2, "ch", "c1", "t3"), (None, None));
        // Past the most an agent writes at once, none is shown, and that is
        // said once; another's are.
        for n in 1..MAX_CHAT_REPLIES {
            assert!(
                replies
                    .open(agent, 1, "ch", &format!("more{n}"), "t")
                    .0
                    .is_some()
            );
        }
        assert_eq!(replies.open(agent, 1, "ch", "one more", "t"), (None, None));
        assert!(replies.lock().full.contains(&agent));
        assert!(replies.open(AgentId::new(), 1, "ch", "c1", "t").0.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_shown_as_it_is_written_is_kept_alive_while_it_is() {
        use havn_proto::channel::ChannelFrame;
        let router = crate::channel_router::ChannelRouter::new();
        let (tx, mut adapter) = mpsc::channel(8);
        router
            .register_as(
                crate::channel_router::AccountKey::new("telegram", "bot"),
                tx,
                false,
                true,
            )
            .await;
        let replies = ChatReplies::default();
        let agent = AgentId::new();
        let (reply, _) = replies.open(agent, 1, "ch", "c1", "t1");
        let reply = reply.expect("open");
        assert!(replies.keep_alive(agent, "ch", "c1", &reply));
        assert!(!replies.keep_alive(agent, "ch", "c1", &reply), "once");
        let shown = ChatWriting {
            router,
            replies: replies.clone(),
            agent_id: agent,
            channel: "telegram".into(),
            channel_id: "ch".into(),
            chat: "c1".into(),
            frame: havn_proto::channel::ChannelWriting {
                account_id: "bot".into(),
                to: "tg:1".into(),
                in_reply_to: None,
                reply: reply.clone(),
                text: "Half of".into(),
                buttons: Vec::new(),
                waiting: false,
                alive: false,
            },
            waiting: "Replying…".into(),
        };
        let alive = tokio::spawn(keep_chat_reply_alive(shown));
        tokio::time::sleep(CHAT_REPLY_ALIVE_EVERY + Duration::from_secs(1)).await;
        let Ok(ChannelFrame::Writing(w)) = adapter.try_recv() else {
            panic!("not told it is alive");
        };
        assert!(w.alive && w.text.is_empty() && w.buttons.is_empty() && w.reply == reply);
        // Once its last part came, it is not, and the telling stops.
        replies.of(agent, "ch", "c1", true);
        tokio::time::sleep(CHAT_REPLY_ALIVE_EVERY + Duration::from_secs(1)).await;
        assert!(adapter.try_recv().is_err());
        assert!(alive.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_ending_while_no_adapter_takes_its_frames_stops_being_kept_alive() {
        // No adapter connected for the account, then one that does not show
        // replies as they are written: no frame can go.
        let router = crate::channel_router::ChannelRouter::new();
        let replies = ChatReplies::default();
        let agent = AgentId::new();
        let (reply, _) = replies.open(agent, 1, "ch", "c1", "t1");
        let shown = ChatWriting {
            router: router.clone(),
            replies: replies.clone(),
            agent_id: agent,
            channel: "telegram".into(),
            channel_id: "ch".into(),
            chat: "c1".into(),
            frame: havn_proto::channel::ChannelWriting {
                account_id: "bot".into(),
                to: "tg:1".into(),
                in_reply_to: None,
                reply: reply.expect("open"),
                text: String::new(),
                buttons: Vec::new(),
                waiting: true,
                alive: false,
            },
            waiting: "Replying…".into(),
        };
        let alive = tokio::spawn(keep_chat_reply_alive(shown));
        tokio::time::sleep(CHAT_REPLY_ALIVE_EVERY + Duration::from_secs(1)).await;
        assert!(!alive.is_finished(), "kept while it is written");
        let (older, mut older_rx) = mpsc::channel(8);
        router
            .register(
                crate::channel_router::AccountKey::new("telegram", "bot"),
                older,
            )
            .await;
        replies.of(agent, "ch", "c1", true);
        tokio::time::sleep(CHAT_REPLY_ALIVE_EVERY + Duration::from_secs(1)).await;
        assert!(alive.is_finished(), "ended with its reply");
        assert!(older_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn replies_being_written_end_with_the_connection_that_wrote_them() {
        use havn_proto::channel::ChannelFrame;
        let mut ctx = ctx_with(false, false).await;
        let agent = AgentId::new();
        let mut adapter = telegram_adapter(&mut ctx, agent, true).await;
        let channel_id = crate::channel_id_codec::encode("telegram", "bot", "tg:1");
        let (gone, _) = ctx.chat_replies.open(agent, 1, &channel_id, "c1", "t1");
        // A connection that took its place, and another agent, go on.
        let (newer, _) = ctx.chat_replies.open(agent, 2, &channel_id, "c2", "t2");
        let (other, _) = ctx
            .chat_replies
            .open(AgentId::new(), 1, &channel_id, "c3", "t3");
        end_chat_replies(agent, 1, &ctx).await;
        let ChannelFrame::Outbound(o) = next_frame(&mut adapter).await else {
            panic!("not a reply");
        };
        assert_eq!(o.reply, gone);
        assert_eq!(
            o.content,
            havn_proto::channel::ChannelMessageContent::Text {
                text: String::new()
            }
        );
        assert!(adapter.try_recv().is_err());
        assert_eq!(ctx.chat_replies.of(agent, &channel_id, "c2", false), newer);
        assert!(
            ctx.chat_replies
                .lock()
                .replies
                .values()
                .any(|r| Some(&r.reply) == other.as_ref() && r.ended.is_none())
        );
        // Ended, a late call of its turn is not shown.
        assert_eq!(
            ctx.chat_replies.open(agent, 1, &channel_id, "c1", "t1"),
            (None, None)
        );
    }

    #[tokio::test]
    async fn a_reply_end_the_conversation_no_longer_takes_still_ends_what_was_shown() {
        use havn_proto::channel::ChannelFrame;
        let mut ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, _rx) = connected_agent(&ctx, owner, "a", serde_json::json!({})).await;
        let mut adapter = telegram_adapter(&mut ctx, agent, true).await;
        // The agent is bound elsewhere now.
        ctx.bindings = Arc::new(Vec::new());
        let channel_id = crate::channel_id_codec::encode("telegram", "bot", "tg:1");
        let (reply, _) = ctx.chat_replies.open(agent, 0, &channel_id, "c1", "t1");
        dispatch(
            agent,
            AgentToGateway::OutboundMessage(OutboundMessage {
                channel_id: channel_id.clone(),
                content: havn_core::MessageContent::Text {
                    text: "the end".into(),
                },
                buttons: Vec::new(),
                poll: None,
                partial: false,
                last: true,
                reply_to: None,
                conversation: Some("c1".into()),
            }),
            &ctx,
        )
        .await;
        let ChannelFrame::Outbound(o) = next_frame(&mut adapter).await else {
            panic!("not a reply");
        };
        assert_eq!(o.reply, reply);
        assert_eq!(
            o.content,
            havn_proto::channel::ChannelMessageContent::Text {
                text: String::new()
            },
            "its text is not sent"
        );
    }

    /// The next LLM answer the agent is sent, within 10 seconds.
    async fn next_llm_response(rx: &mut mpsc::Receiver<GatewayToAgent>) -> LlmResponse {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(GatewayToAgent::LlmResponse(answer)) = rx.recv().await {
                    break answer;
                }
            }
        })
        .await
        .expect("answered at once")
    }

    /// Wait until no call is listed any more.
    async fn forgotten(ctx: &AgentSocketCtx) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ctx.reply_streams.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the call is forgotten");
    }

    /// A context whose model `lan:m` is served by `app`, and its server.
    async fn ctx_calling(
        app: axum::Router,
    ) -> (AgentSocketCtx, tokio::task::JoinHandle<std::io::Result<()>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let mut ctx = ctx_with(false, false).await;
        let mut endpoints = crate::config::Endpoints::default();
        endpoints.0.insert(
            "lan".into(),
            crate::config::Endpoint {
                api: crate::config::EndpointApi::Anthropic,
                base_url: base,
            },
        );
        ctx.llm = Arc::new(crate::config::LlmConfig {
            endpoints,
            ..crate::config::LlmConfig::default()
        });
        ctx.keyring = Some(crate::keyring::KeyRing::from_passphrase("stop-tests"));
        ctx.models
            .store(Arc::new(crate::model_catalog::ModelCatalog {
                default_model: None,
                title_model: None,
                models: vec![crate::model_catalog::ModelEntry {
                    id: "lan:m".into(),
                    label: None,
                    max_output_tokens: None,
                    vision: None,
                    thinking: None,
                    context_window: None,
                    fallback: Vec::new(),
                    price: None,
                }],
            }));
        (ctx, server)
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
        let (conn, _conn_rx) = mpsc::channel(1);
        run_reader(&mut BufReader::new(bytes.as_slice()), agent, &conn, &ctx)
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
                trimmed: false,
                fallback: None,
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
            trimmed: false,
            fallback: None,
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
            trimmed: false,
            fallback: None,
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
        ctx.registry.register(AgentHandle::new(agent_id, 1)).await;
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
        let cgroup = if ctx.verify_peer_cgroup {
            peer_cgroup(&server, None).await
        } else {
            None
        };
        let slot = admission(1).try_admit(None).expect("free slot");
        let gateway = tokio::spawn(handle_connection(server, ctx.clone(), cgroup, slot));
        let (client_read, mut client_write) = client.into_split();
        send_hello(&mut client_write, agent_id, version).await;
        let mut reader = BufReader::new(client_read);
        let reply = read_reply(&mut reader).await;
        drop(client_write);
        drop(reader);
        let verdict = gateway.await.expect("join");
        (verdict, reply)
    }

    fn admission(handshakes: usize) -> Arc<Admission> {
        Arc::new(Admission::new(
            MAX_PEER_LOOKUPS,
            handshakes,
            MAX_PENDING_HANDSHAKES_PER_AGENT,
        ))
    }

    async fn send_hello<W>(write: &mut W, agent_id: AgentId, version: &str)
    where
        W: AsyncWrite + Unpin,
    {
        write_frame(
            write,
            &AgentToGateway::Hello {
                agent_id: agent_id.to_string(),
                version: version.into(),
            },
        )
        .await
        .expect("send hello");
    }

    async fn read_reply<R>(reader: &mut R) -> Option<GatewayToAgent>
    where
        R: AsyncBufRead + Unpin,
    {
        tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<_, GatewayToAgent>(reader),
        )
        .await
        .expect("gateway answers or closes")
        .ok()
        .flatten()
    }

    async fn wait_for_free_slots(slots: &Semaphore, free: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while slots.available_permits() != free {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("slot count settles");
    }

    #[tokio::test]
    async fn silent_peer_is_dropped_after_the_hello_timeout() {
        let ctx = ctx_with(false, false).await;
        tokio::time::pause();
        let admission = admission(1);
        let slot = admission.try_admit(None).expect("free slot");
        let (client, server) = UnixStream::pair().expect("socket pair");
        let started = tokio::time::Instant::now();

        let verdict = handle_connection(server, ctx, None, slot).await;

        let err = verdict.expect_err("a peer that never says Hello is dropped");
        assert!(err.to_string().contains("no Hello"), "{err:#}");
        assert!(started.elapsed() >= HELLO_TIMEOUT);
        assert_eq!(admission.handshakes.available_permits(), 1);
        drop(client);
    }

    #[tokio::test]
    async fn connections_over_the_handshake_cap_are_refused() {
        let ctx = ctx_with(false, false).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let admission = admission(1);
        let server = tokio::spawn(serve(listener, ctx.clone(), Arc::clone(&admission)));

        // One connection that has not said Hello takes the only slot.
        let pending = UnixStream::connect(&path).await.expect("connect");
        wait_for_free_slots(&admission.handshakes, 0).await;

        // Closed at once, not left waiting for a Hello it never sends.
        let extra = UnixStream::connect(&path).await.expect("connect");
        let reply = read_reply(&mut BufReader::new(extra)).await;
        assert!(
            reply.is_none(),
            "an extra connection must be closed: {reply:?}"
        );
        assert_eq!(admission.handshakes.available_permits(), 0);
        assert_eq!(admission.busy.load(Ordering::Relaxed), 1);

        // Once the pending connection is welcomed, its slot is free again.
        let (pending_read, mut pending_write) = pending.into_split();
        let agent = registered_agent(&ctx).await;
        send_hello(&mut pending_write, agent, PROTOCOL_VERSION).await;
        let mut pending_read = BufReader::new(pending_read);
        let reply = read_reply(&mut pending_read).await;
        assert!(
            matches!(reply, Some(GatewayToAgent::Welcome { .. })),
            "{reply:?}"
        );
        wait_for_free_slots(&admission.handshakes, 1).await;

        let next = UnixStream::connect(&path).await.expect("connect");
        let (next_read, mut next_write) = next.into_split();
        let agent = registered_agent(&ctx).await;
        send_hello(&mut next_write, agent, PROTOCOL_VERSION).await;
        let reply = read_reply(&mut BufReader::new(next_read)).await;
        assert!(
            matches!(reply, Some(GatewayToAgent::Welcome { .. })),
            "{reply:?}"
        );
        server.abort();
    }

    #[test]
    fn one_agent_cannot_take_every_handshake_slot() {
        let admission = Admission::new(
            MAX_PEER_LOOKUPS,
            MAX_PENDING_HANDSHAKES,
            MAX_PENDING_HANDSHAKES_PER_AGENT,
        );
        let greedy = AgentId::new();
        let held: Vec<HandshakeSlot> = (0..MAX_PENDING_HANDSHAKES)
            .filter_map(|_| admission.try_admit(Some(greedy)))
            .collect();
        assert_eq!(held.len(), MAX_PENDING_HANDSHAKES_PER_AGENT);

        let other = admission.try_admit(Some(AgentId::new()));
        assert!(other.is_some(), "another agent must still get a slot");

        drop(held);
        assert!(admission.try_admit(Some(greedy)).is_some());
        drop(other);
        assert_eq!(
            admission.handshakes.available_permits(),
            MAX_PENDING_HANDSHAKES
        );
        assert!(
            admission
                .per_agent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn peer_outside_every_agent_cgroup_is_closed_before_taking_a_slot() {
        let ctx = ctx_with(false, true).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let admission = admission(1);
        let server = tokio::spawn(serve(listener, ctx, Arc::clone(&admission)));

        // The test process is not in an agent cgroup.
        let peer = UnixStream::connect(&path).await.expect("connect");
        let reply = read_reply(&mut BufReader::new(peer)).await;

        assert!(reply.is_none(), "the peer must be closed: {reply:?}");
        assert_eq!(admission.foreign.load(Ordering::Relaxed), 1);
        assert_eq!(admission.lookup_failed.load(Ordering::Relaxed), 0);
        assert_eq!(admission.busy.load(Ordering::Relaxed), 0);
        assert_eq!(admission.handshakes.available_permits(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn handshake_slot_is_held_through_the_registration_wait() {
        let ctx = ctx_with(false, false).await;
        tokio::time::pause();
        let admission = admission(1);
        let slot = admission.try_admit(None).expect("free slot");
        let (client, server) = UnixStream::pair().expect("socket pair");
        let started = tokio::time::Instant::now();
        let gateway = tokio::spawn(handle_connection(server, ctx.clone(), None, slot));
        let (_client_read, mut client_write) = client.into_split();

        // The agent is never registered: only the registration wait keeps
        // the connection open this long and then refuses it as unregistered.
        send_hello(&mut client_write, AgentId::new(), PROTOCOL_VERSION).await;
        tokio::time::sleep(REGISTRATION_WAIT - Duration::from_secs(1)).await;
        assert!(!gateway.is_finished());
        assert_eq!(admission.handshakes.available_permits(), 0);

        let err = gateway
            .await
            .expect("join")
            .expect_err("an unregistered agent is refused");
        assert!(err.to_string().contains("not registered"), "{err:#}");
        assert!(started.elapsed() >= REGISTRATION_WAIT);
        assert_eq!(admission.handshakes.available_permits(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_path_must_name_the_agent() {
        fn cgroup_belongs_to_agent(contents: &str, agent_id: AgentId) -> bool {
            cgroup_agent(contents) == Some(agent_id)
        }
        let id = AgentId::new();
        let other = AgentId::new();
        let own = format!("0::/havn/agent-{id}\n");
        assert!(cgroup_belongs_to_agent(&own, id));
        let braced = format!("0::/havn/agent-{{{id}}}\n");
        assert_eq!(cgroup_agent(&braced), None, "only the canonical id form");
        assert!(!cgroup_belongs_to_agent(
            &format!("0::/elsewhere/havn/agent-{id}\n"),
            id
        ));
        assert!(
            !cgroup_belongs_to_agent(&format!("0::/havn/agent-{other}/agent-{id}\n"), id),
            "a cgroup created inside another agent's cgroup must not match"
        );
        assert!(!cgroup_belongs_to_agent(&own, other));
        assert!(!cgroup_belongs_to_agent(
            &format!("0::/havn/agent-{id}-x\n"),
            id
        ));
        assert!(!cgroup_belongs_to_agent(
            &format!("0::/havn/agent-{id}/nested\n"),
            id
        ));
        assert!(!cgroup_belongs_to_agent(
            &format!("1:name=systemd:/havn/agent-{id}\n"),
            id
        ));
        assert!(
            !cgroup_belongs_to_agent(&format!("0::/havn.slice/agent-{id}\n"), id),
            "the cgroup of gateways before #66 must not match"
        );
        assert!(!cgroup_belongs_to_agent("0::/user.slice\n", id));
        assert!(!cgroup_belongs_to_agent("", id));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn peer_cgroup_reads_the_connecting_process() {
        let (ours, theirs) = UnixStream::pair().expect("socket pair");
        let seen = peer_cgroup(&ours, None).await.expect("peer cgroup");
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
            peer_cgroup(&stream, None).await
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
            .send(agent, GatewayToAgent::HeartbeatTick(Default::default()))
            .await
            .expect("send");
        assert!(matches!(
            live_rx.recv().await,
            Some(GatewayToAgent::HeartbeatTick(_))
        ));
    }

    #[tokio::test]
    async fn hello_before_registration_waits_for_it() {
        let ctx = ctx_with(false, false).await;
        let agent_id = AgentId::new();
        // The runtime connects before the spawn that started it returns.
        let registry = ctx.registry.clone();
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            registry.register(AgentHandle::new(agent_id, 1)).await;
        });

        let (_, reply) = hello(&ctx, agent_id).await;

        late.await.expect("join");
        assert!(
            matches!(reply, Some(GatewayToAgent::Welcome { .. })),
            "{reply:?}"
        );
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

    #[tokio::test]
    async fn outbound_reaches_invited_recipients_until_revoked() {
        use havn_db::repo::channel_access;
        use havn_proto::channel::ChannelFrame;

        let mut ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, _rx) = connected_agent(&ctx, owner, "bot", serde_json::json!({})).await;
        ctx.bindings = Arc::new(vec![crate::config::ChannelBindingConfig {
            agent_id: agent.to_string(),
            channel: "telegram".into(),
            account_id: "bot".into(),
        }]);
        let mut channels = std::collections::BTreeMap::new();
        channels.insert(
            "telegram".to_string(),
            crate::config::ChannelEntry {
                accounts: vec![crate::config::ChannelAccount {
                    id: "bot".into(),
                    adapter_token_ref: "secret:channel:telegram:bot".into(),
                    dm_policy: crate::config::DmPolicy::Allowlist,
                    allow_from: Vec::new(),
                    admins: Vec::new(),
                    bot_username: None,
                    group_policy: crate::config::GroupPolicy::Off,
                    allow_groups: Vec::new(),
                }],
            },
        );
        ctx.channels.store(Arc::new(channels));
        let (adapter_tx, mut adapter) = mpsc::channel(8);
        ctx.channel_router
            .register(
                crate::channel_router::AccountKey::new("telegram", "bot"),
                adapter_tx,
            )
            .await;
        let invite =
            channel_access::create_invite(&ctx.db, "telegram", "bot", channel_access::DEFAULT_TTL)
                .await
                .expect("invite");
        channel_access::redeem(
            &ctx.db,
            "telegram",
            "bot",
            "tg:1",
            &invite.code,
            chrono::Utc::now(),
            false,
        )
        .await
        .expect("redeem")
        .expect("valid");

        let (conn, _conn_rx) = mpsc::channel(1);
        let outbound_to = |sender: &str| {
            AgentToGateway::OutboundMessage(havn_core::OutboundMessage {
                conversation: None,
                channel_id: crate::channel_id_codec::encode("telegram", "bot", sender),
                content: havn_core::MessageContent::Text { text: "hi".into() },
                reply_to: None,
                buttons: Vec::new(),
                poll: None,
                partial: false,
                last: false,
            })
        };
        let mut bytes = Vec::new();
        for sender in ["tg:1", "tg:2"] {
            write_frame(&mut bytes, &outbound_to(sender))
                .await
                .expect("encode");
        }
        run_reader(&mut BufReader::new(bytes.as_slice()), agent, &conn, &ctx)
            .await
            .expect("reader");
        let mut delivered = Vec::new();
        while let Ok(ChannelFrame::Outbound(o)) = adapter.try_recv() {
            delivered.push(o.to);
        }
        assert_eq!(delivered, vec!["tg:1".to_string()]);

        channel_access::revoke_sender(&ctx.db, "telegram", "bot", "tg:1")
            .await
            .expect("revoke");
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &outbound_to("tg:1"))
            .await
            .expect("encode");
        run_reader(&mut BufReader::new(bytes.as_slice()), agent, &conn, &ctx)
            .await
            .expect("reader");
        assert!(adapter.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_outbound_for_one_web_chat_reaches_only_its_tabs() {
        let ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, _rx) = connected_agent(&ctx, owner, "bot", serde_json::json!({})).await;
        let mine = crate::webchat::session_for(&owner.to_string(), agent);
        let theirs = crate::webchat::session_for("someone else", agent);
        let (mine_tx, mut mine_rx) = mpsc::channel(8);
        let (theirs_tx, mut theirs_rx) = mpsc::channel(8);
        ctx.webchat_router.register(mine, agent, mine_tx).await;
        ctx.webchat_router.register(theirs, agent, theirs_tx).await;

        let (conn, _conn_rx) = mpsc::channel(1);
        let mut bytes = Vec::new();
        for channel_id in [
            format!("{}{mine}", havn_proto::BROADCAST_TO),
            format!("{}x", havn_proto::BROADCAST_TO),
        ] {
            write_frame(
                &mut bytes,
                &AgentToGateway::OutboundMessage(havn_core::OutboundMessage {
                    conversation: Some("chat".into()),
                    channel_id,
                    content: havn_core::MessageContent::Text {
                        text: "pulse".into(),
                    },
                    reply_to: None,
                    buttons: Vec::new(),
                    poll: None,
                    partial: false,
                    last: false,
                }),
            )
            .await
            .expect("encode");
        }
        run_reader(&mut BufReader::new(bytes.as_slice()), agent, &conn, &ctx)
            .await
            .expect("reader");
        assert!(matches!(
            mine_rx.try_recv(),
            Ok(crate::webchat::ServerMessage::AgentMessage { broadcast: true, ref content, .. })
                if content == "pulse"
        ));
        assert!(
            mine_rx.try_recv().is_err(),
            "a malformed web chat gets nothing"
        );
        assert!(theirs_rx.try_recv().is_err(), "another user's web chat");
    }

    #[tokio::test]
    async fn a_group_member_s_call_is_asked_of_the_owner_privately_never_in_the_group() {
        use havn_proto::channel::{ChannelFrame, ChannelMessageContent};
        let mut ctx = ctx_with(false, false).await;
        let owner = create_user(&ctx.db, NewUser { display_name: "o" })
            .await
            .expect("user")
            .id;
        let (agent, _rx) = connected_agent(&ctx, owner, "bot", serde_json::json!({})).await;
        ctx.bindings = Arc::new(vec![crate::config::ChannelBindingConfig {
            agent_id: agent.to_string(),
            channel: "telegram".into(),
            account_id: "bot".into(),
        }]);
        let mut channels = std::collections::BTreeMap::new();
        channels.insert(
            "telegram".to_string(),
            crate::config::ChannelEntry {
                accounts: vec![crate::config::ChannelAccount {
                    id: "bot".into(),
                    adapter_token_ref: "secret:channel:telegram:bot".into(),
                    dm_policy: crate::config::DmPolicy::Allowlist,
                    allow_from: vec!["tg:owner".into()],
                    admins: Vec::new(),
                    bot_username: None,
                    group_policy: crate::config::GroupPolicy::Allowlist,
                    allow_groups: vec!["tg:-100".into()],
                }],
            },
        );
        ctx.channels.store(Arc::new(channels));
        sqlx::query("INSERT INTO channel_links (channel, sender_id, user_id) VALUES ('telegram', 'tg:owner', ?1)")
            .bind(owner.to_string())
            .execute(&ctx.db)
            .await
            .expect("link");
        let (adapter_tx, mut adapter) = mpsc::channel(16);
        ctx.channel_router
            .register(
                crate::channel_router::AccountKey::new("telegram", "bot"),
                adapter_tx,
            )
            .await;

        let group = crate::channel_id_codec::encode("telegram", "bot", "tg:-100");
        let request = havn_proto::ApprovalRequest {
            request_id: "r-1".into(),
            channel_id: group,
            tool: "shell".into(),
            input: serde_json::json!({
                "from": "tg:bob",
                "said": "[In the group \"Team\", \"Bob\" (tg:bob) wrote:]\nrun ls",
                "tool": "shell",
                "input": {"command": "ls"},
            })
            .to_string(),
            reason: String::new(),
            kind: "call".into(),
        };
        ask_for_approval(agent, request, &ctx).await;
        let mut to_group = Vec::new();
        let mut to_owner = Vec::new();
        while let Ok(ChannelFrame::Outbound(o)) = adapter.try_recv() {
            let ChannelMessageContent::Text { text } = o.content else {
                continue;
            };
            match o.to.as_str() {
                "tg:-100" => to_group.push(text),
                "tg:owner" => to_owner.push(text),
                other => panic!("sent to {other}"),
            }
        }
        assert_eq!(to_group.len(), 1, "{to_group:?}");
        assert!(
            to_group[0].contains("Checking with the agent's owner"),
            "{to_group:?}"
        );
        assert!(
            !to_group[0].contains("shell") && !to_group[0].contains("ls"),
            "{to_group:?}"
        );
        assert!(
            to_owner
                .iter()
                .any(|t| t.contains("shell") && t.contains("run ls")),
            "the owner is asked privately: {to_owner:?}"
        );
        assert!(
            to_owner
                .iter()
                .all(|t| t.starts_with("Someone other than you")),
            "{to_owner:?}"
        );

        // The owner's own turn in the group (issue #297): told it was theirs,
        // and what always allowing there means.
        let request = havn_proto::ApprovalRequest {
            request_id: "r-2".into(),
            channel_id: crate::channel_id_codec::encode("telegram", "bot", "tg:-200"),
            tool: "memory_search".into(),
            input: serde_json::json!({
                "from": "tg:owner",
                "said": "when do I fly home?",
                "tool": "memory_search",
                "input": {"query": "fly home"},
                "found": "2026-12-26",
                "owner": true,
            })
            .to_string(),
            reason: String::new(),
            kind: "result".into(),
        };
        ask_for_approval(agent, request, &ctx).await;
        let mut to_owner = Vec::new();
        while let Ok(ChannelFrame::Outbound(o)) = adapter.try_recv() {
            if let ChannelMessageContent::Text { text } = o.content
                && o.to == "tg:owner"
            {
                to_owner.push(text);
            }
        }
        assert!(
            to_owner
                .iter()
                .any(|t| t.starts_with("You asked in a group")
                    && t.contains("for each of them")
                    && !t.contains("Someone other than you")),
            "{to_owner:?}"
        );
    }
}
