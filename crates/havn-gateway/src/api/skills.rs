//! `/agents/:id/skills`, `/agents/:id/curator/reports`, and the
//! pin/unpin endpoints (spec §9.3, §9.5).
//!
//! Reads open agent.db RO so the dashboard works even when the
//! runtime is offline. Pin / unpin route through the agent socket as
//! [`havn_proto::SkillPinRequest`] frames so `skills_index` stays
//! single-writer (spec §5.2). The dashboard surfaces "agent must be
//! running" if the runtime is offline (HTTP 409).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_core::AgentId;
use havn_db::agent::skills_index::{self, CuratableSkill, Source};
use havn_proto::{AdminOutcome, GatewayToAgent, SkillPinRequest};
use serde::Serialize;
use std::str::FromStr as _;
use std::time::Duration;
use uuid::Uuid;

use crate::AppState;
use crate::admin_rpc::{AdminRpcCallError, RpcError};
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

const DEFAULT_LIMIT: u32 = 200;

#[derive(Debug, Serialize)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    pub source: String,
    pub pinned: bool,
    pub use_count: i64,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl From<CuratableSkill> for SkillView {
    fn from(s: CuratableSkill) -> Self {
        Self {
            name: s.name,
            description: s.description,
            source: match s.source {
                Source::Bundled => "bundled",
                Source::Workspace => "workspace",
            }
            .into(),
            pinned: s.pinned,
            use_count: s.use_count,
            last_used_at: s.last_used_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub agent_id: String,
    pub active: Vec<SkillView>,
    pub archived: Vec<SkillView>,
    pub uninitialised: bool,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<ListResponse>, ApiError> {
    let agent_id =
        AgentId::from_str(&id).map_err(|_| ApiError::BadRequest("invalid agent id".into()))?;
    let agent = havn_db::repo::agents::find_by_id(&state.db, agent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if agent.owner_id != user.id {
        return Err(ApiError::NotFound);
    }
    let agent_db_path = state
        .workspace_root
        .join(agent_id.to_string())
        .join("workspace")
        .join("agent.db");
    if !tokio::fs::try_exists(&agent_db_path).await.unwrap_or(false) {
        return Ok(Json(ListResponse {
            agent_id: id,
            active: Vec::new(),
            archived: Vec::new(),
            uninitialised: true,
        }));
    }
    let pool = match havn_db::agent::connect_read_only(&agent_db_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(agent_id = %id, error = %e, "skills list: opening agent.db RO failed");
            return Err(ApiError::Internal("opening agent.db".into()));
        }
    };
    let active = skills_index::list_all_active(&pool, DEFAULT_LIMIT).await?;
    let archived = skills_index::list_archived(&pool, DEFAULT_LIMIT).await?;
    Ok(Json(ListResponse {
        agent_id: id,
        active: active.into_iter().map(SkillView::from).collect(),
        archived: archived.into_iter().map(SkillView::from).collect(),
        uninitialised: false,
    }))
}

#[derive(Debug, Serialize)]
pub struct CuratorReportFile {
    /// Filename (e.g. `20260503T143007Z.md`); the dashboard renders
    /// the body inline so we don't expose absolute paths to the API.
    pub name: String,
    pub size_bytes: u64,
    pub modified_at: Option<DateTime<Utc>>,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct CuratorReportsResponse {
    pub agent_id: String,
    pub reports: Vec<CuratorReportFile>,
    pub uninitialised: bool,
}

pub async fn curator_reports(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<CuratorReportsResponse>, ApiError> {
    let agent_id =
        AgentId::from_str(&id).map_err(|_| ApiError::BadRequest("invalid agent id".into()))?;
    let agent = havn_db::repo::agents::find_by_id(&state.db, agent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if agent.owner_id != user.id {
        return Err(ApiError::NotFound);
    }
    let dir = state
        .workspace_root
        .join(agent_id.to_string())
        .join("workspace")
        .join(".curator");
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(Json(CuratorReportsResponse {
            agent_id: id,
            reports: Vec::new(),
            uninitialised: true,
        }));
    }
    let mut reports = Vec::new();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(_) => {
            return Ok(Json(CuratorReportsResponse {
                agent_id: id,
                reports,
                uninitialised: false,
            }));
        }
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let body = tokio::fs::read_to_string(&path)
            .await
            .unwrap_or_else(|_| "(failed to read report)".into());
        reports.push(CuratorReportFile {
            name: name.to_string(),
            size_bytes: metadata.len(),
            modified_at: metadata.modified().ok().and_then(|t| {
                let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
                DateTime::<Utc>::from_timestamp(dur.as_secs() as i64, dur.subsec_nanos())
            }),
            body,
        });
    }
    // Newest first.
    reports.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(Json(CuratorReportsResponse {
        agent_id: id,
        reports,
        uninitialised: false,
    }))
}

/// `POST /agents/:id/skills/:name/pin` and `.../unpin` — flip the
/// `pinned` flag on a workspace skill via the agent socket. Pinned
/// skills are immune to the curator (spec §9.5). Same status-code
/// shape as `memory::forget` (spec §5.2 single-writer).
pub async fn pin(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    set_pinned(state, user, &id, &name, true).await
}

pub async fn unpin(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    set_pinned(state, user, &id, &name, false).await
}

async fn set_pinned(
    state: AppState,
    user: AuthedUser,
    id: &str,
    name: &str,
    pinned: bool,
) -> Result<StatusCode, ApiError> {
    let agent_id =
        AgentId::from_str(id).map_err(|_| ApiError::BadRequest("invalid agent id".into()))?;
    let agent = havn_db::repo::agents::find_by_id(&state.db, agent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if agent.owner_id != user.id {
        return Err(ApiError::NotFound);
    }
    if !state.registry.is_connected(agent_id).await {
        return Err(ApiError::Conflict(
            "agent is not running — start it to apply skill edits (spec §5.2 single-writer)".into(),
        ));
    }
    // Defence in depth — keep prompt-injection-supplied skill names
    // away from any future filesystem op the runtime might add to the
    // pin path. Same shape as `skill_manage::validate_name`.
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(
            "skill name must be kebab-case (lowercase, digits, '-', '_' only, ≤ 64 chars)".into(),
        ));
    }

    let request_id = Uuid::now_v7().to_string();
    let req = SkillPinRequest {
        request_id: request_id.clone(),
        name: name.into(),
        pinned,
    };
    let registry = state.registry.clone();
    let agent_for_send = agent_id;
    let result = state
        .admin_rpc
        .call(
            &request_id,
            || async move {
                registry
                    .send(agent_for_send, GatewayToAgent::SkillPinRequest(req))
                    .await
                    .map_err(|e| format!("send: {e}"))
            },
            Duration::from_secs(10),
        )
        .await;

    match result {
        Ok(AdminOutcome::Ok { .. }) => {
            audit::record_agent_action(
                &state.db,
                user.id,
                agent_id,
                if pinned {
                    "skill.pinned"
                } else {
                    "skill.unpinned"
                },
                serde_json::json!({"name": name}),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(AdminOutcome::NotFound) => Err(ApiError::NotFound),
        Ok(AdminOutcome::Error { message }) => Err(ApiError::Internal(message)),
        Err(AdminRpcCallError::Send(e)) => Err(ApiError::Internal(format!("rpc send: {e}"))),
        Err(AdminRpcCallError::Rpc(RpcError::Timeout(_))) => Err(ApiError::Internal(
            "agent runtime did not reply to skill.pin within 10s".into(),
        )),
        Err(AdminRpcCallError::Rpc(RpcError::ChannelClosed)) => Err(ApiError::Conflict(
            "agent disconnected before reply arrived; retry once the agent reconnects".into(),
        )),
    }
}
