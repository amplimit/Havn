//! `/agents` REST endpoints (spec §8.3).
//!
//! Lifecycle:
//! - `POST /agents` — create DB row + workspace dir. Status `created`.
//! - `POST /agents/{id}/start` — spawn runtime process via the spawner,
//!   register the handle. Status `running` once the agent connects.
//! - `POST /agents/{id}/stop` — request graceful shutdown via socket frame,
//!   then SIGTERM/SIGKILL via spawner.
//! - `DELETE /agents/{id}` — stop if running, remove workspace, delete DB row.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_core::AgentId;
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
use crate::policy_resolver;
use crate::workspace;

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub name: String,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
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
    let id = parse_id(&id)?;
    let agent = ensure_owner(&state, &user, id).await?;
    let connected = state.registry.is_connected(id).await;
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

    // Spec §6.3 enforcement: max_agents is checked here, before INSERT.
    // policy_resolver::for_user is the single source of truth for "what
    // is this user allowed to do" (single-user returns a permissive
    // policy; team-aware role walk is Phase 3).
    let user_policy = policy_resolver::for_user(&state.db, user.id).await;
    let current_count = repo::count_for_owner(&state.db, user.id).await?;
    if current_count >= user_policy.max_agents {
        return Err(ApiError::Forbidden(format!(
            "agent quota exhausted: {current_count}/{} agents already created. Adjust HAVN_MAX_AGENTS_PER_USER or remove an existing agent.",
            user_policy.max_agents
        )));
    }

    let agent = repo::create(
        &state.db,
        NewAgent {
            owner_id: user.id,
            team_id: None,
            name: req.name.trim(),
            config: req.config.unwrap_or_else(|| serde_json::json!({})),
        },
    )
    .await?;

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
        serde_json::json!({"name": agent.name}),
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
    let id = parse_id(&id)?;
    let agent = ensure_owner(&state, &user, id).await?;

    if let Some(n) = &req.name {
        if n.trim().is_empty() {
            return Err(ApiError::BadRequest("name must be non-empty".into()));
        }
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

pub async fn start(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<AgentView>, ApiError> {
    let id = parse_id(&id)?;
    let _ = ensure_owner(&state, &user, id).await?;

    // Idempotent — explicit POST /start is now mostly redundant since
    // the WS handler auto-starts on chat connect (spec §4.3 lifecycle
    // is preserved as data-model state, but most users never see it).
    // Operators who want to pre-warm an agent can still POST here.
    ensure_running(&state, user.id, id).await?;

    let agent = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let connected = state.registry.is_connected(id).await;
    Ok(Json(AgentView::from_agent(agent, connected)))
}

/// Make `agent_id` runnable and connected. Idempotent and concurrency-
/// safe via per-agent start lock — multiple WS connects to the same
/// offline agent serialize through the lock; only the first spawns,
/// the rest wait for the existing handshake to complete.
///
/// Steps:
/// 1. Fast path — if already connected, return.
/// 2. Acquire per-agent lock so we don't race a sibling start.
/// 3. Re-check connected (someone else may have raced ahead while we
///    were waiting on the lock).
/// 4. Ensure workspace exists, resolve policy via for_session, spawn
///    runtime, register handle, set DB status, write audit.
/// 5. Poll `is_connected` for up to [`HANDSHAKE_TIMEOUT`] so the
///    caller (typically the WS handler) can proceed knowing the
///    agent socket is live.
///
/// Returns Ok when the agent finished its Hello/Welcome handshake.
/// Returns ApiError::Internal on spawn failure or handshake timeout
/// (caller surfaces a friendlier message at the boundary).
pub async fn ensure_running(
    state: &AppState,
    by_user: havn_core::UserId,
    id: AgentId,
) -> Result<(), ApiError> {
    use std::time::Duration;

    if state.registry.is_connected(id).await {
        return Ok(());
    }

    // Per-agent serialization. The map-of-mutexes pattern keeps each
    // agent's start independent — slow agent A doesn't block agent B
    // from starting. Lock is held across spawn + initial registration
    // (a few hundred ms in practice).
    let agent_lock = {
        let mut map = state.start_locks.lock().await;
        map.entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = agent_lock.lock().await;

    // Re-check after acquiring — a sibling start may have completed
    // while we waited on the lock.
    if state.registry.is_connected(id).await {
        return Ok(());
    }

    // If a handle is registered but the socket hasn't completed
    // handshake yet (a previous attempt is mid-flight), DON'T spawn
    // again — just fall through to the polling loop and wait.
    if !state.registry.is_registered(id).await {
        let workspace = workspace_for(state, id);
        workspace::ensure(&workspace)
            .await
            .map_err(|e| ApiError::Internal(format!("workspace: {e}")))?;

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
        };
        let agent_row = repo::find_by_id(&state.db, id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let policy = policy_resolver::for_session(&state.db, &agent_row).await;

        let handle = state.spawner.spawn(&cfg, &policy).await.map_err(|e| {
            tracing::error!(agent_id = %id, error = %e, "spawner.spawn failed");
            ApiError::Internal(format!("spawn: {e}"))
        })?;
        info!(agent_id = %id, pid = handle.pid, "agent spawned (lazy)");

        state.registry.register(handle).await;
        repo::set_status(&state.db, id, AgentStatus::Running).await?;
        audit::record_agent_action(
            &state.db,
            by_user,
            id,
            "agent.started",
            serde_json::json!({}),
        )
        .await;
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
        tokio::time::sleep(POLL).await;
    }
    Err(ApiError::Internal(format!(
        "agent {id} did not complete its handshake within {}s",
        HANDSHAKE_TIMEOUT.as_secs()
    )))
}

pub async fn stop(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<AgentView>, ApiError> {
    let id = parse_id(&id)?;
    let _ = ensure_owner(&state, &user, id).await?;

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

    repo::set_status(&state.db, id, AgentStatus::Stopped).await?;
    audit::record_agent_action(
        &state.db,
        user.id,
        id,
        "agent.stopped",
        serde_json::json!({}),
    )
    .await;

    let agent = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    info!(agent_id = %id, "agent stopped");
    let connected = state.registry.is_connected(id).await;
    Ok(Json(AgentView::from_agent(agent, connected)))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    let id = parse_id(&id)?;
    let _ = ensure_owner(&state, &user, id).await?;

    // Stop first if running.
    if let Some(handle) = state.registry.remove(id).await {
        if state.registry.is_connected(id).await {
            let _ = state.registry.send(id, GatewayToAgent::Shutdown).await;
        }
        if let Err(e) = state.spawner.stop(&handle).await {
            warn!(agent_id = %id, error = %e, "spawner.stop during delete failed");
        }
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

fn parse_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_str(s).map_err(|_| ApiError::BadRequest("invalid agent id".into()))
}

pub(crate) async fn ensure_owner(
    state: &AppState,
    user: &AuthedUser,
    id: AgentId,
) -> Result<Agent, ApiError> {
    let agent = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if agent.owner_id != user.id {
        // Don't leak existence to non-owners.
        return Err(ApiError::NotFound);
    }
    Ok(agent)
}

pub(crate) fn workspace_for(state: &AppState, id: AgentId) -> std::path::PathBuf {
    state.workspace_root.join(id.to_string()).join("workspace")
}
