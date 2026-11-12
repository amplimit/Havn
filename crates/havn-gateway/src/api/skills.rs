//! `/agents/:id/skills`, `/agents/:id/curator/reports`, and the
//! pin/unpin endpoints (spec §9.3, §9.5).
//!
//! Skill rows are read by the agent's runtime from its own agent.db
//! (started if needed); the gateway never opens the file. Pin / unpin route through the agent socket as
//! [`havn_proto::SkillPinRequest`] frames so `skills_index` stays
//! single-writer (spec §5.2). The dashboard surfaces "agent must be
//! running" if the runtime is offline (HTTP 409).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_db::agent::skills_index::{CuratableSkill, Source};
use havn_proto::{AdminOutcome, GatewayToAgent, SkillPinRequest};
use serde::Serialize;
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
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;
    #[derive(serde::Deserialize)]
    struct Rows {
        active: Vec<CuratableSkill>,
        archived: Vec<CuratableSkill>,
    }
    let Rows { active, archived } = crate::api::agents::agent_data(
        &state,
        user.id,
        agent_id,
        havn_proto::DataQuery::Skills {
            limit: DEFAULT_LIMIT,
        },
        false,
    )
    .await?;
    Ok(Json(ListResponse {
        agent_id: agent_id.to_string(),
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
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;
    let workspace = state
        .workspace_root
        .join(agent_id.to_string())
        .join("workspace");
    // The workspace is agent-writable: open it and `.curator` without
    // following symlinks and read only regular files.
    let found = tokio::task::spawn_blocking(move || read_curator_reports(&workspace))
        .await
        .map_err(|e| ApiError::Internal(format!("curator reports task: {e}")))?;
    let Some(mut reports) = found else {
        return Ok(Json(CuratorReportsResponse {
            agent_id: agent_id.to_string(),
            reports: Vec::new(),
            uninitialised: true,
        }));
    };
    // Newest first.
    reports.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(Json(CuratorReportsResponse {
        agent_id: agent_id.to_string(),
        reports,
        uninitialised: false,
    }))
}

/// Caps on one curator report, on how many are returned and on their
/// total size.
const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const MAX_REPORTS: usize = 200;
const MAX_REPORTS_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// `None` when the workspace or its `.curator` directory does not exist.
fn read_curator_reports(workspace: &std::path::Path) -> Option<Vec<CuratorReportFile>> {
    let ws = crate::agent_files::WorkspaceDir::open(workspace).ok()?;
    let dir = ws.subdir(".curator").ok().flatten()?;
    let mut files = dir
        .regular_files(|name| name.ends_with(".md"), MAX_REPORTS)
        .unwrap_or_default();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut total = 0usize;
    let reports = files
        .into_iter()
        .map(|(name, metadata)| {
            let body = if total >= MAX_REPORTS_TOTAL_BYTES {
                "(omitted: report size limit reached)".into()
            } else {
                match dir.read_text(&name, MAX_REPORT_BYTES) {
                    Ok(Some(text)) => text,
                    _ => "(failed to read report)".into(),
                }
            };
            total += body.len();
            CuratorReportFile {
                size_bytes: metadata.len(),
                modified_at: metadata.modified().ok().and_then(|t| {
                    let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
                    DateTime::<Utc>::from_timestamp(
                        i64::try_from(dur.as_secs()).ok()?,
                        dur.subsec_nanos(),
                    )
                }),
                name,
                body,
            }
        })
        .collect();
    Some(reports)
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
    let agent = crate::api::agents::resolve_agent(id, &user, &state).await?;
    let agent_id = agent.id;
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
