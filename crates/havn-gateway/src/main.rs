//! havn gateway — message routing, auth, channel adapters, agent lifecycle,
//! credential resolver, LLM proxy, cron scheduler, config hot reload.
//!
//! Phase 1 progress (this binary):
//! - HTTP server: `/healthz`, `/credentials`, `/agents`, `/v1/llm/anthropic`.
//! - Unix-socket listener for connecting agent runtime processes
//!   ([`agent_socket`]); handshake, registry attach, message dispatch.
//! - In-memory agent registry ([`agent_registry`]) tracking spawned and
//!   connected agents.
//! - LLM proxy with Anthropic Messages API + 401/429/402/5xx fallback.
//!
//! Still to land in Phase 1: `WebSocket` `WebChat` adapter, heartbeat tick,
//! cron scheduler, hot reload, real `NamespaceSpawner` isolation.

mod admin_rpc;
mod agent_registry;
mod agent_socket;
mod api;
mod audit;
mod auth;
mod channel_id_codec;
mod channel_router;
mod config;
mod credential_resolver;
mod cron;
mod heartbeat;
mod keyring;
mod llm_proxy;
mod perm;
mod policy_resolver;
mod query_broker;
mod reload;
mod secret_ref;
mod webchat;
mod workspace;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use arc_swap::ArcSwap;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use havn_core::UserId;
use havn_spawner::{AgentSpawner, subprocess::SubprocessSpawner};
use sqlx::SqlitePool;
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::agent_registry::AgentRegistry;
use crate::agent_socket::AgentSocketCtx;
use crate::config::GatewayConfig;
use crate::reload::ReloadHandles;
use crate::webchat::WebChatRouter;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    /// User created by `users::ensure_default` at first startup. The
    /// `auth::AuthedUser` extractor returns this when `trust_header`
    /// is off (single-user loopback mode, spec §1.7).
    pub bootstrap_user_id: UserId,
    /// When true, the `auth::AuthedUser` extractor reads `X-User-ID`
    /// from the request and looks it up in the `users` table; missing
    /// or unknown header → 403. Required (gateway refuses to start)
    /// for any non-loopback listen address — see startup guard in
    /// [`enforce_listen_safety`].
    pub trust_header_enabled: bool,
    pub http: reqwest::Client,
    pub registry: AgentRegistry,
    pub spawner: Arc<dyn AgentSpawner>,
    pub workspace_root: PathBuf,
    pub runtime_binary: PathBuf,
    /// `havn-init` shim path (spec §4.1) — passed to `NamespaceSpawner` so
    /// the runtime ends up as PID 1 in its namespace. None on hosts where
    /// the file is missing; the spawner errors clearly in that case.
    pub init_binary: Option<PathBuf>,
    pub agent_socket_path: PathBuf,
    /// Operator-declared extra bind-mounts (spec §4.1 v0.7). Cloned
    /// from `GatewayConfig::extra_mounts` at startup and threaded into
    /// every `AgentSpawnConfig` plus every Welcome frame. Held by value
    /// (not ArcSwap) — adding/removing mounts requires a gateway
    /// restart anyway because already-running agents wouldn't pick up
    /// new mounts mid-namespace.
    pub extra_mounts: Vec<havn_spawner::ExtraMount>,
    /// Operator-declared in-namespace tmpfs mounts (spec §16.2). Same
    /// lifetime / restart-required semantics as `extra_mounts`.
    pub tmpfs_mounts: Vec<havn_spawner::TmpfsMount>,
    /// Operator-declared seccomp syscall allowances (spec §16.2). Same
    /// lifetime / restart-required semantics as `extra_mounts`. The
    /// runtime validates names against `libc::SYS_*` at startup and
    /// refuses unknown ones.
    pub seccomp_allow_extra: Vec<String>,
    /// DNS servers written into agent ns `/etc/resolv.conf` when the
    /// agent has egress allowed (spec §6.3 v0.7). Operator-configurable
    /// via `[network] agent_dns`.
    pub agent_dns: Vec<String>,
    /// Channel-adapter accounts keyed by channel id (spec §3.4). Source
    /// of truth for "which (channel, account) pairs are allowed to open
    /// a `/api/v1/channel` WS"; the WS handler rejects connections for
    /// pairs not declared here.
    /// Held behind `ArcSwap` so the config watcher can hot-reload channel
    /// accounts without a restart (issue #16) — the WS upgrade handler
    /// `.load()`s the current value per connect.
    pub channels: Arc<ArcSwap<std::collections::BTreeMap<String, config::ChannelEntry>>>,
    /// Cross-channel agent bindings (spec §3.5). The router consults
    /// this to translate inbound `(channel, account)` → `agent_id`. Behind
    /// `ArcSwap` for hot-reload (issue #16); read per inbound message.
    pub bindings: Arc<ArcSwap<Vec<config::ChannelBindingConfig>>>,
    /// Per-(channel, account) outbound mpsc map for connected channel
    /// adapter daemons. Populated when a `/api/v1/channel` WS opens;
    /// drained when an agent replies — see [`crate::channel_router`].
    pub channel_router: channel_router::ChannelRouter,
    pub webchat_router: WebChatRouter,
    /// Origins accepted on the `WebChat` WS endpoint (spec §11). Hot-reloaded
    /// via the config watcher (spec §8.4) — readers swap-load atomically.
    pub allowed_origins: Arc<ArcSwap<Vec<String>>>,
    /// Pending admin RPC senders for `MemoryForget` / `SkillPin` —
    /// the dashboard's write paths. See [`crate::admin_rpc`].
    pub admin_rpc: admin_rpc::AdminRpc,
    /// Per-agent serialization lock for the lazy-spawn path. Two
    /// concurrent WS connect attempts to the same offline agent must
    /// not both spawn — the second one waits on this mutex, then
    /// re-checks `is_connected` and short-circuits. Map-of-mutexes
    /// keeps each agent's start independent (one slow agent doesn't
    /// block other agents from starting).
    pub start_locks: Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<havn_core::AgentId, Arc<tokio::sync::Mutex<()>>>,
        >,
    >,
    /// Active hybrid-retrieval config (spec §9.4 v0.7). Same handle
    /// also handed to each runtime via the Welcome frame; surfacing
    /// it on AppState lets `GET /embedding` answer without depending
    /// on a particular agent connection.
    ///
    /// Held behind `ArcSwap` so `PATCH /embedding` can flip providers
    /// at runtime — new agent spawns pick up the swapped value at
    /// their Welcome handshake; **already-running agents** keep the
    /// snapshot they were welcomed with (spec §9.4 frozen-prompt
    /// invariant). Persistence across gateway restart still happens
    /// via `config.toml` (the file is the canonical operator-facing
    /// source of truth — see spec §1.6 "infrastructure not SaaS").
    pub embedding_config: Arc<ArcSwap<serde_json::Value>>,
    /// Credential encryption-at-rest handle (spec §13 Phase 3).
    /// `None` means the operator hasn't set `HAVN_AGE_KEY` AND the
    /// credentials table is empty — in that mode the gateway runs
    /// but credential POSTs return a clear error. Once any
    /// credential exists the boot guard refuses startup without a
    /// key, so callers can assume `Some` whenever a credential read
    /// happens (the `Credential` row would not exist otherwise).
    pub keyring: Option<keyring::KeyRing>,
}

#[tokio::main]
#[allow(
    clippy::too_many_lines,
    reason = "main wires every subsystem; extracting helpers obscures the startup path"
)]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = GatewayConfig::load().context("loading gateway config")?;
    info!(listen = %cfg.listen, db = %cfg.db_path.display(), "starting havn gateway");

    enforce_listen_safety(&cfg.listen, cfg.trust_header.enabled)
        .context("listen-address safety check (spec §1.7)")?;

    // Spec §3.5 — surface bindings that reference nonexistent channels
    // or accounts. Warn-not-fail because operators commenting out an
    // account temporarily shouldn't crash every other binding's
    // gateway. Agent-id existence is checked at first inbound dispatch
    // (DB lookup unavailable here without a circular dep on the pool).
    for d in config::dangling_bindings(&cfg) {
        warn!(
            index = d.index,
            agent_id = %d.agent_id,
            channel = %d.channel,
            account_id = %d.account_id,
            reason = ?d.reason,
            "dangling binding — config references unknown channel/account"
        );
    }

    if let Some(parent) = cfg.db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating db parent dir {}", parent.display()))?;
    }
    tokio::fs::create_dir_all(&cfg.workspace_root)
        .await
        .with_context(|| format!("creating workspace root {}", cfg.workspace_root.display()))?;

    let db = havn_db::connect(&cfg.db_path)
        .await
        .with_context(|| format!("opening database at {}", cfg.db_path.display()))?;
    info!("database ready");

    let user = havn_db::repo::users::ensure_default(&db)
        .await
        .context("bootstrapping default user")?;
    info!(user_id = %user.id, name = %user.display_name, "single-user mode active");

    // Credential encryption-at-rest boot guard (spec §13 Phase 3).
    // Resolution table:
    //   HAVN_AGE_KEY set    + creds empty  → Some(ring); migration no-ops.
    //   HAVN_AGE_KEY set    + creds exist  → Some(ring); migration encrypts legacy rows.
    //   HAVN_AGE_KEY missing + creds empty → None; warn; first POST will reject.
    //   HAVN_AGE_KEY missing + creds exist → REFUSE START — visible breakage beats
    //                                         silent fallback to plaintext use.
    let keyring = match keyring::KeyRing::load_from_env() {
        Ok(ring) => {
            let migrated = encrypt_legacy_credential_rows(&db, &ring)
                .await
                .context("startup migration: encrypting legacy credential rows")?;
            if migrated > 0 {
                info!(
                    rows = migrated,
                    "credentials migrated to age ciphertext at rest"
                );
            }
            Some(ring)
        }
        Err(_) => {
            let count = havn_db::repo::credentials::count_all(&db)
                .await
                .context("checking credentials table")?;
            if count > 0 {
                anyhow::bail!(
                    "HAVN_AGE_KEY env var is not set, but {count} credential row(s) exist. \
                     Set HAVN_AGE_KEY to the operator passphrase before starting the gateway. \
                     Refusing to boot with credentials in an unmigrated / undecryptable state \
                     (spec §13 Phase 3)."
                );
            }
            tracing::warn!(
                "HAVN_AGE_KEY env var is not set; the gateway will run but credential POSTs \
                 will fail until you set the env and restart."
            );
            None
        }
    };

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")?;

    let registry = AgentRegistry::new();
    let webchat_router = WebChatRouter::new();
    let channel_router_state = channel_router::ChannelRouter::new();
    let cron_scheduler = cron::CronScheduler::new(
        db.clone(),
        registry.clone(),
        webchat_router.clone(),
        cfg.workspace_root.clone(),
    );
    let spawner: Arc<dyn AgentSpawner> = select_spawner(&cfg.network);
    let runtime_binary = cfg
        .resolved_runtime_binary()
        .context("resolving runtime binary path")?;
    info!(runtime = %runtime_binary.display(), "agent runtime binary");
    let init_binary = match cfg.resolved_init_binary() {
        Ok(p) if p.exists() => {
            info!(init = %p.display(), "agent PID-1 init shim available");
            Some(p)
        }
        Ok(p) => {
            tracing::warn!(
                init = %p.display(),
                "havn-init not found at resolved path — NamespaceSpawner will fail \
                 with a clear error if it is selected. Subprocess fallback unaffected."
            );
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not resolve havn-init path");
            None
        }
    };
    let reload_handles = ReloadHandles::from_config(&cfg);
    let allowed_origins = reload_handles.allowed_origins.clone();
    info!(
        allowed_origins = ?allowed_origins.load(),
        "webchat origin allowlist"
    );

    let admin_rpc = admin_rpc::AdminRpc::new();
    let query_broker = query_broker::QueryBroker::new();
    let start_locks = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    // Single shared `Arc<ArcSwap<Value>>` so PATCH /embedding atomically
    // updates both AppState (for GET reads) and AgentSocketCtx (for
    // the next agent's Welcome frame) without a separate plumbing path.
    let embedding_config = Arc::new(ArcSwap::from_pointee(cfg.embedding.clone()));
    let state = AppState {
        db: db.clone(),
        bootstrap_user_id: user.id,
        trust_header_enabled: cfg.trust_header.enabled,
        http: http.clone(),
        registry: registry.clone(),
        spawner,
        workspace_root: cfg.workspace_root.clone(),
        runtime_binary,
        init_binary,
        agent_socket_path: cfg.agent_socket_path.clone(),
        extra_mounts: cfg.extra_mounts.clone(),
        tmpfs_mounts: cfg.tmpfs_mounts.clone(),
        seccomp_allow_extra: cfg.seccomp.allow_extra.clone(),
        agent_dns: cfg.network.agent_dns.clone(),
        channels: reload_handles.channels.clone(),
        bindings: reload_handles.bindings.clone(),
        channel_router: channel_router_state.clone(),
        webchat_router: webchat_router.clone(),
        allowed_origins,
        admin_rpc: admin_rpc.clone(),
        start_locks,
        embedding_config: embedding_config.clone(),
        keyring: keyring.clone(),
    };

    // Background task: agent socket listener.
    //
    // The agent socket no longer carries a fixed `current_user` — LLM
    // calls are billed against the **agent's owner_id**, looked up
    // from the `agents` table when an `LlmRequest` frame arrives.
    // This is what makes multi-user havn correct: agent A's calls
    // bill agent A's owner regardless of which HTTP user happens to
    // be online.
    let socket_ctx = AgentSocketCtx {
        registry: registry.clone(),
        db: db.clone(),
        http,
        webchat_router,
        channel_router: channel_router_state.clone(),
        cron_scheduler: cron_scheduler.clone(),
        admin_rpc,
        query_broker,
        embedding_config: embedding_config.clone(),
        extra_mounts: cfg.extra_mounts.clone(),
        tmpfs_mounts: cfg.tmpfs_mounts.clone(),
        seccomp_allow_extra: cfg.seccomp.allow_extra.clone(),
        keyring: keyring.clone(),
    };
    let socket_path = cfg.agent_socket_path.clone();
    let socket_task = tokio::spawn(async move {
        if let Err(e) = agent_socket::run_listener(&socket_path, socket_ctx).await {
            error!(error = ?e, "agent socket listener crashed");
        }
    });

    // Background task: heartbeat scheduler (spec §9.6).
    let heartbeat = heartbeat::HeartbeatScheduler::new(registry.clone(), db.clone());
    let heartbeat_task = tokio::spawn(async move {
        heartbeat.run().await;
    });

    // Background task: cron scheduler (spec §8.5).
    let cron_task = tokio::spawn(async move {
        cron_scheduler.run().await;
    });
    drop(db);

    // Background task: config watcher (spec §8.4).
    let reload_task = reload::spawn_watcher(
        config::GatewayConfig::resolved_path(),
        cfg.clone(),
        reload_handles,
    );

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/me", get(api::me::get))
        .route("/me/audit-log", get(api::audit::list_for_self))
        .route(
            "/embedding",
            get(api::embedding::status).patch(api::embedding::patch),
        )
        .route(
            "/credentials",
            get(api::credentials::list).post(api::credentials::create),
        )
        .route(
            "/credentials/{id}",
            patch(api::credentials::update).delete(api::credentials::delete),
        )
        .route("/agents", get(api::agents::list).post(api::agents::create))
        .route(
            "/agents/{id}",
            get(api::agents::get)
                .patch(api::agents::patch)
                .delete(api::agents::delete),
        )
        .route("/agents/{id}/start", post(api::agents::start))
        .route("/agents/{id}/stop", post(api::agents::stop))
        .route(
            "/agents/{id}/cron",
            get(api::cron::list).post(api::cron::create),
        )
        .route(
            "/agents/{id}/cron/{job_id}",
            patch(api::cron::update).delete(api::cron::delete),
        )
        .route("/agents/{id}/cron/{job_id}/runs", get(api::cron::list_runs))
        .route("/agents/{id}/conversation", get(api::conversation::list))
        .route(
            "/agents/{id}/bootstrap",
            get(api::bootstrap::get).put(api::bootstrap::put),
        )
        .route(
            "/agents/{id}/mcp",
            get(api::mcp::get).patch(api::mcp::patch),
        )
        .route("/agents/{id}/memory", get(api::memory::list))
        .route(
            "/agents/{id}/memory/{key}",
            axum::routing::delete(api::memory::forget),
        )
        .route("/agents/{id}/skills", get(api::skills::list))
        .route("/agents/{id}/skills/{name}/pin", post(api::skills::pin))
        .route("/agents/{id}/skills/{name}/unpin", post(api::skills::unpin))
        .route(
            "/agents/{id}/curator/reports",
            get(api::skills::curator_reports),
        )
        // Team management surface (spec §10.3 admin views).
        .route("/teams", get(api::teams::list).post(api::teams::create))
        .route(
            "/teams/{id}",
            get(api::teams::get)
                .patch(api::teams::patch)
                .delete(api::teams::delete),
        )
        .route("/teams/{id}/agents", get(api::teams::list_agents))
        .route("/teams/{id}/usage", get(api::teams::usage))
        .route("/teams/{id}/audit-log", get(api::audit::list_for_team))
        .route(
            "/teams/{id}/members",
            get(api::members::list).post(api::members::add),
        )
        .route(
            "/teams/{id}/members/{user_id}",
            patch(api::members::patch).delete(api::members::remove),
        )
        .route(
            "/teams/{id}/roles",
            get(api::roles::list).post(api::roles::create),
        )
        .route(
            "/teams/{id}/roles/{role_id}",
            patch(api::roles::patch).delete(api::roles::delete),
        )
        .route(
            "/teams/{id}/credentials",
            get(api::team_credentials::list).post(api::team_credentials::create),
        )
        .route(
            "/teams/{id}/credentials/{cred_id}",
            patch(api::team_credentials::update).delete(api::team_credentials::delete),
        )
        .route("/ws/chat/{agent_id}", get(api::webchat_ws::handler))
        .route("/api/v1/channel", get(api::channel::handler))
        .route("/v1/llm/anthropic", post(api::llm::anthropic))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = cfg.listen.parse().context("parsing listen address")?;
    let listener = TcpListener::bind(addr).await.context("binding listener")?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP")?;

    socket_task.abort();
    let _ = socket_task.await;
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    cron_task.abort();
    let _ = cron_task.await;
    reload_task.abort();
    let _ = reload_task.await;
    Ok(())
}

/// Pick the strongest agent spawner the host supports.
///
/// On Linux, prefer [`havn_spawner::namespace::NamespaceSpawner`] —
/// cgroup-v2 isolation today, full namespace + Landlock + seccomp in
/// subsequent verticals. Fall back to the unprivileged
/// [`SubprocessSpawner`] when cgroup v2 is unavailable (e.g. dev box
/// without `Delegate=yes`, restricted CI, container without cgroup
/// passthrough). The fallback is logged loudly so operators don't ship
/// it to production by accident.
///
/// Set `HAVN_SPAWNER=subprocess` to force the unprivileged backend even
/// when cgroup v2 is detected — useful in CI smoke tests where
/// `kernel.unprivileged_userns_clone=0` blocks the namespace path at
/// spawn time even though `try_init` succeeds at startup.
fn select_spawner(network: &config::NetworkConfig) -> Arc<dyn AgentSpawner> {
    if matches!(std::env::var("HAVN_SPAWNER").as_deref(), Ok("subprocess")) {
        info!("agent spawner: SubprocessSpawner (forced via HAVN_SPAWNER=subprocess)");
        return Arc::new(SubprocessSpawner::new());
    }

    #[cfg(target_os = "linux")]
    {
        use havn_spawner::namespace::NamespaceSpawner;
        use havn_spawner::network::AgentSubnetPool;
        // Parse the operator's pool CIDR. Bad config = warn + default.
        let pool = match AgentSubnetPool::parse(&network.agent_pool_cidr) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    cidr = %network.agent_pool_cidr,
                    error = %e,
                    "invalid agent_pool_cidr; falling back to 10.42.0.0/16"
                );
                AgentSubnetPool::default()
            }
        };
        // Refuse to start if the pool conflicts with an existing host
        // route — VPN, Docker bridge, k8s pod network. Better a hard
        // fail at startup than silent breakage at first agent spawn.
        if let Err(e) = pool.refuse_on_route_conflict() {
            tracing::error!(
                error = %e,
                "agent pool conflicts with host routing; refusing to start. \
                 Edit `[network] agent_pool_cidr` in gateway config and retry."
            );
            std::process::exit(2);
        }
        match NamespaceSpawner::try_init_with_pool(pool) {
            Ok(s) => {
                info!(pool_cidr = %pool.cidr(), "agent spawner: NamespaceSpawner (cgroup v2 isolation)");
                // Per-agent network (spec §4.1, §6.3 v0.7). Idempotent
                // host-wide setup: MASQUERADE + FORWARD rules +
                // ip_forward sysctl, all targeting the operator-
                // configured pool. Failure is non-fatal — agents that
                // don't need outbound still work.
                match havn_spawner::network::ensure_global_nat(&pool) {
                    Ok(primary) => {
                        info!(primary_iface = %primary, pool_cidr = %pool.cidr(), "per-agent NAT ready");
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "global NAT setup failed; agents will spawn without outbound network. \
                             Tools needing the internet (web_fetch, browser pack, network MCP) will \
                             surface their own errors at use time."
                        );
                    }
                }
                // GC orphaned veth-h-* interfaces from previous gateway
                // runs (clean shutdown reaps them via netns destruction;
                // crash / SIGKILL paths leak them). At startup we hold
                // the pid file lock — no other gateway is alive — so any
                // existing veth-h-* is by definition stale.
                match havn_spawner::network::gc_orphaned_veth() {
                    Ok(0) => {}
                    Ok(n) => info!(
                        reaped = n,
                        "garbage-collected orphan veth from previous run"
                    ),
                    Err(e) => tracing::warn!(error = %e, "veth GC failed; orphans will accumulate"),
                }
                return Arc::new(s);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "NamespaceSpawner unavailable; falling back to SubprocessSpawner WITHOUT isolation. \
                     Production deployments need systemd cgroup delegation (Delegate=yes) or root."
                );
            }
        }
    }
    info!("agent spawner: SubprocessSpawner (no isolation)");
    Arc::new(SubprocessSpawner::new())
}

/// Refuse to start when the gateway is bound to a non-loopback address
/// without explicit trust-header opt-in (spec §1.7). havn does no
/// authentication itself; if you expose it raw to the network you've
/// turned it into an open relay for whichever LLM credentials live
/// in the database. This guard is the loudest no havn can give.
///
/// Returns Ok in two cases:
/// - listen address is loopback (127/8 or [::1]) — any config goes
/// - trust_header.enabled is true (or `HAVN_TRUST_HEADER=1` was set,
///   which the config loader already merged in)
///
/// Walk the credentials table once at startup; for any row whose
/// stored bytes don't carry the age header, encrypt them in place
/// with the operator's `KeyRing`. Idempotent: a second run sees only
/// already-encrypted rows and is a no-op. Returns the count of rows
/// that needed migration so the boot log can surface it.
///
/// Why per-row UPDATE instead of one big transaction: row count is
/// small (single-digit-to-tens for typical havn deployments), and
/// per-row commits keep partial failures recoverable — if the
/// gateway gets SIGKILL'd mid-migration, the next boot picks up
/// whichever rows are still plaintext.
async fn encrypt_legacy_credential_rows(
    db: &SqlitePool,
    ring: &keyring::KeyRing,
) -> anyhow::Result<u64> {
    use sqlx::Row as _;
    let rows: Vec<(String, Vec<u8>)> = sqlx::query("SELECT id, api_key FROM credentials")
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|r| (r.get::<String, _>("id"), r.get::<Vec<u8>, _>("api_key")))
        .collect();
    let mut migrated = 0u64;
    for (id, bytes) in rows {
        if keyring::is_age_ciphertext(&bytes) {
            continue;
        }
        let ciphertext = ring
            .encrypt(&bytes)
            .with_context(|| format!("encrypting credential row {id}"))?;
        sqlx::query("UPDATE credentials SET api_key = ?1 WHERE id = ?2")
            .bind(ciphertext)
            .bind(&id)
            .execute(db)
            .await?;
        migrated += 1;
    }
    Ok(migrated)
}

fn enforce_listen_safety(listen: &str, trust_header_enabled: bool) -> anyhow::Result<()> {
    if trust_header_enabled {
        if !is_loopback(listen) {
            tracing::info!(
                listen,
                "non-loopback listen with trust_header enabled — gateway is \
                 expecting a reverse proxy to set X-User-ID. The gateway will \
                 NOT authenticate users itself; ensure the proxy is the only \
                 way packets reach this address."
            );
        }
        return Ok(());
    }
    if is_loopback(listen) {
        return Ok(());
    }
    anyhow::bail!(
        "gateway is configured to listen on {listen} (not loopback) but \
         trust_header is disabled. havn does no auth on its own — exposing \
         it without an upstream proxy + trust_header would let any caller \
         act as the bootstrap user.\n\
         \n\
         Fix: set [gateway.trust_header] enabled = true in config.toml \
         (or HAVN_TRUST_HEADER=1 in the env), put a reverse proxy that \
         emits X-User-ID in front of the gateway, and provision users via \
         `havn user add <id> --display-name \"...\"`. See spec §1.7."
    );
}

/// Treat 127.0.0.0/8, ::1, and the unspecified-loopback variants as
/// loopback. Anything that parses as a non-loopback IP is a hard
/// error in `enforce_listen_safety`. Hostnames (the literal string
/// `"localhost"` is fine) get a string match.
fn is_loopback(listen: &str) -> bool {
    use std::net::SocketAddr;
    if let Ok(addr) = listen.parse::<SocketAddr>() {
        return addr.ip().is_loopback();
    }
    // Fall back to host-string matching for hostname:port forms.
    let host = listen.rsplit_once(':').map_or(listen, |(h, _)| h);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .init();
}

async fn healthz(State(state): State<AppState>) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await
    {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(e) => {
            error!(error = %e, "healthz: database probe failed");
            (StatusCode::SERVICE_UNAVAILABLE, "db error").into_response()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn loopback_addresses_pass() {
        assert!(is_loopback("127.0.0.1:8080"));
        assert!(is_loopback("localhost:8080"));
        assert!(is_loopback("[::1]:8080"));
        assert!(is_loopback("127.5.6.7:1234"));
    }

    #[test]
    fn external_addresses_fail() {
        assert!(!is_loopback("0.0.0.0:8080"));
        assert!(!is_loopback("192.168.1.5:8080"));
        assert!(!is_loopback("example.com:80"));
    }

    #[test]
    fn external_listen_without_trust_is_an_error() {
        let r = enforce_listen_safety("0.0.0.0:8080", false);
        assert!(r.is_err());
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("trust_header"), "{msg}");
    }

    #[test]
    fn external_listen_with_trust_passes() {
        assert!(enforce_listen_safety("0.0.0.0:8080", true).is_ok());
    }

    #[test]
    fn loopback_listen_passes_either_way() {
        assert!(enforce_listen_safety("127.0.0.1:8080", false).is_ok());
        assert!(enforce_listen_safety("127.0.0.1:8080", true).is_ok());
    }
}

#[allow(
    clippy::expect_used,
    reason = "startup-only signal handler installation"
)]
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("received ctrl-c, shutting down"),
        () = terminate => info!("received SIGTERM, shutting down"),
    }
}
