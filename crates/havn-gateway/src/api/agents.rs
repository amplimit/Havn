//! `/agents` REST endpoints (spec §8.3).
//!
//! Lifecycle:
//! - `POST /agents` — create DB row + workspace dir. Status `created`.
//!   With `team_id`, the agent belongs to that team: the caller must be a
//!   member whose team role allows another agent (`max_agents`), and the
//!   agent then runs under that role's policy.
//! - `POST /agents/{id}/start` — spawn runtime process via the spawner,
//!   register the handle. Status `running` once the agent connects.
//! - `POST /agents/{id}/stop` — request graceful shutdown via socket frame,
//!   then SIGTERM/SIGKILL via spawner. The owner, or an admin of the
//!   agent's team, may stop it.
//! - `DELETE /agents/{id}` — stop if running, remove workspace, delete DB row.
//!
//! Status `running` doubles as "meant to be running": while it is set,
//! [`crate::supervisor`] restarts the agent when its process ends or goes
//! silent, and brings it back after a gateway restart. Starting and
//! stopping an agent's process is serialized per agent by
//! [`start_lock`].

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_core::{AgentId, TeamId};
use havn_db::repo::agents::{self as repo, Agent, AgentStatus, NewAgent};
use havn_proto::GatewayToAgent;
use havn_spawner::AgentSpawnConfig;
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use std::sync::Arc;
use tracing::{info, warn};

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::perm;
use crate::policy_resolver;
use crate::workspace;

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub name: String,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub team_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchAgentRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Optional **partial** config patch. The handler reads the
    /// existing config and shallow-merges these keys on top. Pass
    /// `null` to clear a key, omit to leave it.
    #[serde(default)]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct AgentView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub team_id: Option<String>,
    pub config: serde_json::Value,
    pub pid: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `true` once the runtime process has completed the Hello/Welcome
    /// handshake on the agent socket. Distinct from `status = "running"`
    /// (which only means the process was spawned). The smoke test polls
    /// this to know when the WebSocket endpoint will accept connections.
    pub connected: bool,
}

impl AgentView {
    fn from_agent(a: Agent, connected: bool) -> Self {
        Self {
            id: a.id.to_string(),
            name: a.name,
            status: a.status.as_str().into(),
            team_id: a.team_id.map(|t| t.to_string()),
            config: a.config,
            pid: a.pid,
            created_at: a.created_at,
            updated_at: a.updated_at,
            connected,
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<Vec<AgentView>>, ApiError> {
    let agents = repo::list_for_owner(&state.db, user.id).await?;
    let mut views = Vec::with_capacity(agents.len());
    for a in agents {
        let connected = state.registry.is_connected(a.id).await;
        views.push(AgentView::from_agent(a, connected));
    }
    Ok(Json(views))
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<AgentView>, ApiError> {
    let agent = resolve_agent(&id, &user, &state).await?;
    let connected = state.registry.is_connected(agent.id).await;
    Ok(Json(AgentView::from_agent(agent, connected)))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<CreateAgentRequest>,
) -> Result<(StatusCode, Json<AgentView>), ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must be non-empty".into()));
    }
    if let Some(raw) = req.config.as_ref().and_then(|c| c.get("policy")) {
        check_policy_override(raw)?;
    }

    // Spec §6.3 enforcement: max_agents is checked here, before INSERT.
    // policy_resolver::for_user is the single source of truth for "what
    // is this user allowed to do" (single-user returns a permissive
    // policy; team-aware role walk is Phase 3).
    let user_policy = policy_resolver::for_user(&state.db, user.id).await;
    let current_count = repo::count_for_owner(&state.db, user.id).await?;
    if current_count >= user_policy.max_agents {
        return Err(ApiError::Forbidden(format!(
            "agent quota exhausted: {current_count}/{} agents already created. Raise max_agents in your team role (or HAVN_MAX_AGENTS_PER_USER without teams) or remove an existing agent.",
            user_policy.max_agents
        )));
    }

    let (team_id, max_in_team) = match req.team_id.as_deref() {
        None => (None, 0),
        Some(raw) => {
            let team_id = TeamId::from_str(raw)
                .map_err(|_| ApiError::BadRequest("invalid team id".into()))?;
            perm::require_member(&state.db, user.id, team_id).await?;
            let role_policy = policy_resolver::for_team_member(&state.db, user.id, team_id).await;
            let in_team = repo::count_for_owner_in_team(&state.db, user.id, team_id).await?;
            if in_team >= role_policy.max_agents {
                return Err(ApiError::Forbidden(format!(
                    "your role in team {team_id} allows {} agents and you already have {in_team} there",
                    role_policy.max_agents
                )));
            }
            (Some(team_id), role_policy.max_agents)
        }
    };

    // The checks above give the specific message; this insert re-checks
    // both quotas atomically, so concurrent creates cannot exceed them.
    let agent = repo::create_within_quota(
        &state.db,
        NewAgent {
            owner_id: user.id,
            team_id,
            name: req.name.trim(),
            config: req.config.unwrap_or_else(|| serde_json::json!({})),
        },
        user_policy.max_agents,
        max_in_team,
    )
    .await?
    .ok_or_else(|| {
        ApiError::Forbidden("agent quota exhausted: a concurrent request used the last slot".into())
    })?;

    let workspace = workspace_for(&state, agent.id);
    workspace::ensure(&workspace)
        .await
        .map_err(|e| ApiError::Internal(format!("workspace: {e}")))?;

    info!(agent_id = %agent.id, name = %agent.name, owner = %user.id, "agent created");
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.created",
        serde_json::json!({
            "name": agent.name,
            "team_id": agent.team_id.map(|t| t.to_string()),
        }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(AgentView::from_agent(agent, false)),
    ))
}

/// `PATCH /agents/{id}` — rename + edit config (model, heartbeat,
/// per-agent policy override, …). Shallow-merges the `config` keys
/// the caller sent on top of the existing `agent.config` so a
/// settings UI doesn't have to round-trip the whole blob just to
/// flip one field.
///
/// If the agent is currently running, the new config takes effect
/// on the NEXT spawn — the running runtime keeps its frozen
/// system prompt and Welcome-snapshot policy (spec §9.4 frozen-prompt
/// invariant). The handler logs but doesn't auto-restart; the user
/// can stop+next-message to apply, or just wait for an idle stop.
pub async fn patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<PatchAgentRequest>,
) -> Result<Json<AgentView>, ApiError> {
    let agent = resolve_agent(&id, &user, &state).await?;
    let id = agent.id;

    if let Some(n) = &req.name {
        if n.trim().is_empty() {
            return Err(ApiError::BadRequest("name must be non-empty".into()));
        }
    }

    if let Some(raw) = req.config.as_ref().and_then(|c| c.get("policy")) {
        check_policy_override(raw)?;
    }

    // Shallow-merge config patch on top of existing config.
    let merged_config = req.config.as_ref().map(|patch| {
        let mut base = agent.config.as_object().cloned().unwrap_or_default();
        for (k, v) in patch.iter() {
            // null in the patch clears the key, otherwise overwrite.
            if v.is_null() {
                base.remove(k);
            } else {
                base.insert(k.clone(), v.clone());
            }
        }
        serde_json::Value::Object(base)
    });

    let updated = repo::patch(
        &state.db,
        id,
        req.name.as_deref().map(str::trim),
        merged_config.as_ref(),
    )
    .await?;
    audit::record_agent_action(
        &state.db,
        user.id,
        id,
        "agent.patched",
        serde_json::json!({
            "name": req.name,
            "config_keys": req.config.as_ref().map(|c| c.keys().collect::<Vec<_>>()),
        }),
    )
    .await;
    info!(agent_id = %id, by = %user.id, "agent patched");
    let connected = state.registry.is_connected(id).await;
    Ok(Json(AgentView::from_agent(updated, connected)))
}

/// Rejects a `config.policy` override that the policy resolver would
/// drop as malformed: dropping it would also drop its restrictions.
fn check_policy_override(raw: &serde_json::Value) -> Result<(), ApiError> {
    if raw.is_null() {
        return Ok(());
    }
    let policy = serde_json::from_value::<havn_core::Policy>(raw.clone())
        .map_err(|e| ApiError::BadRequest(format!("config.policy: {e}")))?;
    policy.validate()?;
    Ok(())
}

pub async fn start(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<AgentView>, ApiError> {
    let agent = resolve_agent(&id, &user, &state).await?;
    let id = agent.id;

    // Idempotent — explicit POST /start is now mostly redundant since
    // the WS handler auto-starts on chat connect (spec §4.3 lifecycle
    // is preserved as data-model state, but most users never see it).
    // Operators who want to pre-warm an agent can still POST here. An
    // explicit start also gives an agent the supervisor gave up on a
    // fresh restart budget.
    state.supervisor.forget(id).await;
    ensure_running(&state, user.id, id).await?;

    let agent = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let connected = state.registry.is_connected(id).await;
    Ok(Json(AgentView::from_agent(agent, connected)))
}

/// Per-agent lock held while an agent's process is started or stopped.
/// The map-of-mutexes pattern keeps each agent independent — slow agent A
/// doesn't block agent B.
pub(crate) async fn start_lock(state: &AppState, id: AgentId) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = state.start_locks.lock().await;
    map.entry(id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Who asks for an agent to run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StartBy {
    User(havn_core::UserId),
    /// The supervisor bringing back an agent meant to be running; audited
    /// as its owner. Does nothing once the agent is no longer `running`.
    Supervisor(havn_core::UserId),
}

#[derive(Debug)]
pub(crate) struct StartError {
    /// A process was spawned for this attempt (or one was already starting);
    /// its exit or silence is handled by the supervisor like any other.
    pub(crate) spawned: bool,
    pub(crate) error: ApiError,
}

impl From<StartError> for ApiError {
    fn from(e: StartError) -> Self {
        e.error
    }
}

/// Make `agent_id` runnable and connected. Idempotent and concurrency-
/// safe via per-agent start lock — multiple WS connects to the same
/// offline agent serialize through the lock; only the first spawns,
/// the rest wait for the existing handshake to complete.
///
/// Returns Ok when the agent finished its Hello/Welcome handshake.
/// Returns ApiError::Internal on spawn failure, handshake timeout, or
/// when the process exits before its handshake (caller surfaces a
/// friendlier message at the boundary).
pub async fn ensure_running(
    state: &AppState,
    by_user: havn_core::UserId,
    id: AgentId,
) -> Result<(), ApiError> {
    Ok(start_agent(state, id, StartBy::User(by_user)).await?)
}

/// Steps:
/// 1. Fast path — if already connected, return.
/// 2. Acquire per-agent lock so we don't race a sibling start or a stop.
/// 3. Re-check connected (someone else may have raced ahead while we
///    were waiting on the lock); a supervisor start also re-checks that
///    the agent is still meant to run.
/// 4. Ensure workspace exists, resolve policy via for_session, spawn
///    runtime, set DB status, register handle, start watching for its
///    exit, write audit.
/// 5. Poll `is_connected` for up to `HANDSHAKE_TIMEOUT` so the
///    caller (typically the WS handler) can proceed knowing the
///    agent socket is live.
pub(crate) async fn start_agent(
    state: &AppState,
    id: AgentId,
    by: StartBy,
) -> Result<(), StartError> {
    use std::time::Duration;

    let not_spawned = |error: ApiError| StartError {
        spawned: false,
        error,
    };

    if state.registry.is_connected(id).await {
        return Ok(());
    }

    let agent_lock = start_lock(state, id).await;
    let _guard = agent_lock.lock().await;

    // Re-check after acquiring — a sibling start may have completed
    // while we waited on the lock.
    if state.registry.is_connected(id).await {
        return Ok(());
    }
    if state.supervisor.shutting_down() {
        return Err(not_spawned(ApiError::Conflict(
            "the gateway is shutting down".into(),
        )));
    }
    if let StartBy::Supervisor(_) = by {
        match repo::find_by_id(&state.db, id).await {
            Ok(Some(agent)) if agent.status == AgentStatus::Running => {}
            // Stopped or deleted while a restart was pending.
            Ok(_) => return Ok(()),
            Err(e) => return Err(not_spawned(e.into())),
        }
    }

    // If a handle is registered but the socket hasn't completed
    // handshake yet (a previous attempt is mid-flight), DON'T spawn
    // again — just fall through to the polling loop and wait. A dead
    // instance does not linger here: the supervisor deregisters it as
    // soon as its process exits.
    let mut spawned = state.registry.is_registered(id).await;
    if !spawned {
        let agent_row = repo::find_by_id(&state.db, id)
            .await
            .map_err(|e| not_spawned(e.into()))?
            .ok_or_else(|| not_spawned(ApiError::NotFound))?;
        if let Some(team_id) = agent_row.team_id
            && !havn_db::repo::team_memberships::is_member(&state.db, agent_row.owner_id, team_id)
                .await
                .map_err(|e| not_spawned(e.into()))?
        {
            return Err(not_spawned(ApiError::Forbidden(format!(
                "agent {id} belongs to team {team_id}, which its owner is no longer a member of"
            ))));
        }
        let workspace = workspace_for(state, id);
        workspace::ensure(&workspace)
            .await
            .map_err(|e| not_spawned(ApiError::Internal(format!("workspace: {e}"))))?;

        let cfg = AgentSpawnConfig {
            agent_id: id,
            workspace_dir: workspace,
            runtime_binary: state.runtime_binary.clone(),
            init_binary: state.init_binary.clone(),
            gateway_socket: state.agent_socket_path.clone(),
            is_subagent: false,
            extra_mounts: state.extra_mounts.clone(),
            tmpfs_mounts: state.tmpfs_mounts.clone(),
            seccomp_allow_extra: state.seccomp_allow_extra.clone(),
            agent_dns: state.agent_dns.clone(),
            env_passthrough: state.agent_env_passthrough.as_ref().clone(),
        };
        let policy =
            policy_resolver::for_session(&state.db, &agent_row, state.trusted_proxies.is_some())
                .await;

        let handle = state.spawner.spawn(&cfg, &policy).await.map_err(|e| {
            tracing::error!(agent_id = %id, error = %e, "spawner.spawn failed");
            not_spawned(ApiError::Internal(format!("spawn: {e}")))
        })?;
        info!(agent_id = %id, pid = handle.pid, "agent spawned");

        // Status before the exit watch starts, so an instant crash is
        // judged against `running`.
        if let Err(e) = repo::set_status(&state.db, id, AgentStatus::Running).await {
            if let Err(stop) = state.spawner.stop(&handle).await {
                warn!(agent_id = %id, error = %stop, "spawner.stop failed");
            }
            return Err(not_spawned(e.into()));
        }
        spawned = true;
        let generation = state.registry.register(handle.clone()).await;
        // A shutdown that began while we spawned has already drained the
        // registry; this instance is ours to stop.
        if state.supervisor.shutting_down() {
            if state
                .registry
                .remove_generation(id, generation)
                .await
                .is_some()
                && let Err(e) = state.spawner.stop(&handle).await
            {
                warn!(agent_id = %id, error = %e, "spawner.stop failed");
            }
            return Err(StartError {
                spawned,
                error: ApiError::Conflict("the gateway is shutting down".into()),
            });
        }
        crate::supervisor::watch(state, handle, generation);
        let (actor, action, details) = match by {
            StartBy::User(user) => (user, "agent.started", serde_json::json!({})),
            StartBy::Supervisor(owner) => (
                owner,
                "agent.restarted",
                serde_json::json!({"by": "supervisor"}),
            ),
        };
        audit::record_agent_action(&state.db, actor, id, action, details).await;
    }

    // Poll for handshake. 200ms × 50 = 10s upper bound; subprocess
    // spawner gets there in ~100ms, namespace spawner can take longer
    // on cold cgroup setup.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(200);
    let start = std::time::Instant::now();
    while start.elapsed() < HANDSHAKE_TIMEOUT {
        if state.registry.is_connected(id).await {
            return Ok(());
        }
        if !state.registry.is_registered(id).await {
            return Err(StartError {
                spawned,
                error: ApiError::Internal(format!(
                    "agent {id} exited before completing its handshake"
                )),
            });
        }
        tokio::time::sleep(POLL).await;
    }
    Err(StartError {
        spawned,
        error: ApiError::Internal(format!(
            "agent {id} did not complete its handshake within {}s",
            HANDSHAKE_TIMEOUT.as_secs()
        )),
    })
}

pub async fn stop(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<AgentView>, ApiError> {
    let agent = match resolve_agent(&id, &user, &state).await {
        Err(ApiError::NotFound) => team_agent_for_admin(&id, &user, &state).await?,
        other => other?,
    };
    let id = agent.id;
    stop_agent(&state, id, user.id, serde_json::json!({})).await?;

    let agent = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let connected = state.registry.is_connected(id).await;
    Ok(Json(AgentView::from_agent(agent, connected)))
}

/// An agent `user` does not own but may manage as an admin of the agent's
/// team. `NotFound` otherwise, so existence does not leak.
async fn team_agent_for_admin(
    s: &str,
    user: &AuthedUser,
    state: &AppState,
) -> Result<Agent, ApiError> {
    let id = AgentId::from_str(s.trim()).map_err(|_| ApiError::NotFound)?;
    let agent = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    match agent.team_id {
        Some(team_id) if perm::is_admin(&state.db, user.id, team_id).await? => Ok(agent),
        _ => Err(ApiError::NotFound),
    }
}

/// Stop `id` (graceful shutdown frame, then the spawner), mark it
/// `stopped` and audit `agent.stopped` by `by_user` with `details`.
/// Stopping an agent that is not running only updates its status. Every
/// stop goes through here, so the supervisor leaves the agent down.
pub(crate) async fn stop_agent(
    state: &AppState,
    id: AgentId,
    by_user: havn_core::UserId,
    details: serde_json::Value,
) -> Result<(), ApiError> {
    {
        let agent_lock = start_lock(state, id).await;
        let _guard = agent_lock.lock().await;
        // Stopped first: from here on the supervisor leaves the agent
        // down, including a restart that is already pending.
        repo::set_status(&state.db, id, AgentStatus::Stopped).await?;
        state.supervisor.forget(id).await;

        // Best-effort graceful shutdown via socket; ignore if not connected.
        if state.registry.is_connected(id).await
            && let Err(e) = state.registry.send(id, GatewayToAgent::Shutdown).await
        {
            warn!(agent_id = %id, error = ?e, "shutdown frame send failed");
        }

        if let Some(handle) = state.registry.remove(id).await
            && let Err(e) = state.spawner.stop(&handle).await
        {
            warn!(agent_id = %id, error = %e, "spawner.stop failed; continuing");
        }
    }

    audit::record_agent_action(&state.db, by_user, id, "agent.stopped", details).await;
    info!(agent_id = %id, by = %by_user, "agent stopped");
    Ok(())
}

/// Stop every agent `owner` (or, with `None`, anyone) has in `team_id`.
/// The agents are kept. Used when a member leaves a team and before a team
/// is deleted.
pub(crate) async fn stop_team_agents(
    state: &AppState,
    team_id: TeamId,
    owner: Option<havn_core::UserId>,
    by_user: havn_core::UserId,
    reason: &str,
) -> Result<usize, ApiError> {
    let agents = repo::list_for_team(&state.db, team_id).await?;
    let mut stopped = 0;
    for agent in agents
        .into_iter()
        .filter(|a| owner.is_none_or(|o| a.owner_id == o))
    {
        if agent.status != AgentStatus::Running && !state.registry.is_registered(agent.id).await {
            continue;
        }
        stop_agent(
            state,
            agent.id,
            by_user,
            serde_json::json!({"reason": reason, "team_id": team_id.to_string()}),
        )
        .await?;
        stopped += 1;
    }
    Ok(stopped)
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    let agent = resolve_agent(&id, &user, &state).await?;
    let id = agent.id;

    // Stop first if running. Under the start lock, so no start or restart
    // spawns it again while it is being removed.
    let agent_lock = start_lock(&state, id).await;
    let _guard = agent_lock.lock().await;
    state.supervisor.forget(id).await;
    repo::set_status(&state.db, id, AgentStatus::Stopped).await?;
    if state.registry.is_connected(id).await {
        let _ = state.registry.send(id, GatewayToAgent::Shutdown).await;
    }
    if let Some(handle) = state.registry.remove(id).await
        && let Err(e) = state.spawner.stop(&handle).await
    {
        warn!(agent_id = %id, error = %e, "spawner.stop during delete failed");
    }

    // Audit BEFORE delete so the FK SET NULL on agent_id doesn't lose
    // the breadcrumb. (We still pass `Some(id)` — the FK rewrites to
    // NULL on cascade, but the row exists; the audit page renders
    // "agent (deleted)" gracefully.)
    audit::record_agent_action(
        &state.db,
        user.id,
        id,
        "agent.deleted",
        serde_json::json!({}),
    )
    .await;
    repo::delete(&state.db, id).await?;

    let workspace = workspace_for(&state, id);
    if let Err(e) = tokio::fs::remove_dir_all(&workspace).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(agent_id = %id, error = %e, "workspace removal failed");
    }

    info!(agent_id = %id, "agent deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve a path parameter (UUID or agent name) and verify ownership
/// in a single step. Name lookup is owner-scoped so it doubles as the
/// ownership check; UUID lookup needs an explicit owner_id comparison.
/// Returns 404 for non-existent agents AND for agents owned by someone
/// else — don't leak existence to non-owners.
pub(crate) async fn resolve_agent(
    s: &str,
    user: &AuthedUser,
    state: &AppState,
) -> Result<Agent, ApiError> {
    let s = s.trim();
    if let Ok(id) = AgentId::from_str(s) {
        let agent = repo::find_by_id(&state.db, id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if agent.owner_id != user.id {
            // Don't leak existence to non-owners.
            return Err(ApiError::NotFound);
        }
        return Ok(agent);
    }
    // Name path: find_by_owner_and_name is already owner-scoped.
    repo::find_by_owner_and_name(&state.db, user.id, s)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Read rows from an agent's own database for the dashboard. The agent's
/// runtime runs the query and returns the rows over its socket; the
/// gateway never opens agent.db, which the agent can write. A stopped
/// agent is started only when `start_if_stopped` (the chat view, which
/// starts it anyway); otherwise the read fails with 409, so looking at an
/// agent does not restart one the operator stopped.
pub(crate) async fn agent_data<T: serde::de::DeserializeOwned>(
    state: &AppState,
    by_user: havn_core::UserId,
    id: AgentId,
    query: havn_proto::DataQuery,
    start_if_stopped: bool,
) -> Result<T, ApiError> {
    use havn_proto::{AdminOutcome, DataRequest, GatewayToAgent};
    if start_if_stopped {
        ensure_running(state, by_user, id).await?;
    } else if !state.registry.is_connected(id).await {
        return Err(ApiError::Conflict(
            "agent is not running; start it to see this".into(),
        ));
    }
    let request_id = uuid::Uuid::now_v7().to_string();
    let req = DataRequest {
        request_id: request_id.clone(),
        query,
    };
    let registry = state.registry.clone();
    let reply = state
        .admin_rpc
        .call(
            &request_id,
            id,
            || async move {
                registry
                    .send(id, GatewayToAgent::DataRequest(req))
                    .await
                    .map_err(|e| format!("send: {e}"))
            },
            // A freshly started runtime reads its first frame only after
            // loading embeddings and starting MCP servers; the request
            // waits in the socket until then.
            std::time::Duration::from_secs(60),
        )
        .await
        .map_err(|e| ApiError::Internal(format!("agent did not answer: {e}")))?;
    match reply {
        AdminOutcome::Ok { details } => serde_json::from_value(details)
            .map_err(|e| ApiError::Internal(format!("unexpected reply from agent: {e}"))),
        AdminOutcome::Error { message } => Err(ApiError::Internal(message)),
        other => Err(ApiError::Internal(format!(
            "unexpected reply from agent: {other:?}"
        ))),
    }
}

pub(crate) fn workspace_for(state: &AppState, id: AgentId) -> std::path::PathBuf {
    state.workspace_root.join(id.to_string()).join("workspace")
}

pub(crate) fn embedding_env_passthrough(embedding: &serde_json::Value) -> Vec<String> {
    if embedding
        .get("provider")
        .and_then(serde_json::Value::as_str)
        != Some("openai")
    {
        return Vec::new();
    }
    let name = embedding
        .get("api_key_env")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("OPENAI_API_KEY");
    vec![name.to_string()]
}

#[cfg(test)]
mod tests {
    use super::{check_policy_override, embedding_env_passthrough};
    use serde_json::json;

    #[test]
    fn policy_override_must_parse_and_validate() {
        assert!(check_policy_override(&json!(null)).is_ok());
        assert!(
            check_policy_override(&json!({
                "permissions": { "can_use_shell": false },
                "hooks": { "pre_tool_use": [{ "tool": "shell", "deny": ["^echo"] }] }
            }))
            .is_ok()
        );
        for bad in [
            json!({ "permissions": { "can_use_shell": "no" } }),
            json!({ "hooks": { "pre_tool_use": [{ "tool": "shell", "deny": ["[unclosed"] }] } }),
            json!({ "hooks": { "pre_tool_use": [{ "tool": "", "deny": ["x"] }] } }),
        ] {
            assert!(check_policy_override(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn passthrough_is_empty_unless_openai_embeddings() {
        assert!(embedding_env_passthrough(&json!({ "provider": "disabled" })).is_empty());
        assert!(embedding_env_passthrough(&json!({ "provider": "hrr" })).is_empty());
        assert!(embedding_env_passthrough(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn passthrough_names_the_configured_openai_key_variable() {
        assert_eq!(
            embedding_env_passthrough(&json!({ "provider": "openai" })),
            ["OPENAI_API_KEY"]
        );
        assert_eq!(
            embedding_env_passthrough(&json!({ "provider": "openai", "api_key_env": "EMBED_KEY" })),
            ["EMBED_KEY"]
        );
    }
}
