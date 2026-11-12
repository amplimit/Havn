//! `/agents/:id/memory` — admin view of the agent's typed memory
//! (spec §9.4).
//!
//! Lets the user see, in the dashboard, exactly what the agent has
//! remembered about them, grouped by kind, with the supersedes audit
//! trail. The agent's runtime reads its own agent.db and returns the
//! rows (it is started if it is not running); the gateway never opens
//! the file, which the agent can write.
//!
//! `DELETE /agents/:id/memory/:key` routes the request through the
//! agent socket as a [`havn_proto::MemoryForgetRequest`] frame —
//! agent.db remains single-writer (spec §5.2). Requires the agent to
//! be running; offline deletes return 409.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_db::agent::memory;
use havn_proto::{AdminOutcome, GatewayToAgent, MemoryForgetRequest};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

use crate::AppState;
use crate::admin_rpc::{AdminRpcCallError, RpcError};
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

const DEFAULT_LIMIT: u32 = 200;
const MAX_LIMIT: u32 = 1000;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Optional kind filter. When omitted, all active rows are returned.
    #[serde(default)]
    pub kind: Option<String>,
    /// Cap on rows returned. Default 200, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct MemoryEntryView {
    pub key: String,
    pub value: String,
    pub kind: String,
    pub source: String,
    pub ttl_days: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub recall_count: i64,
    pub last_recalled_at: Option<DateTime<Utc>>,
    pub archived_at: Option<DateTime<Utc>>,
    pub supersedes_id: Option<String>,
}

impl From<memory::Entry> for MemoryEntryView {
    fn from(e: memory::Entry) -> Self {
        Self {
            key: e.key,
            value: e.value,
            kind: e.kind.as_str().into(),
            source: e.source.as_str().into(),
            ttl_days: e.ttl_days,
            created_at: e.created_at,
            updated_at: e.updated_at,
            recall_count: e.recall_count,
            last_recalled_at: e.last_recalled_at,
            archived_at: e.archived_at,
            supersedes_id: e.supersedes_id,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub agent_id: String,
    pub entries: Vec<MemoryEntryView>,
    /// Kept for API compatibility; always false now that the running agent
    /// answers (a stopped agent gets 409 instead).
    pub uninitialised: bool,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let entries: Vec<memory::Entry> = crate::api::agents::agent_data(
        &state,
        user.id,
        agent_id,
        havn_proto::DataQuery::Memory {
            memory_kind: q.kind.clone(),
            limit,
        },
        false,
    )
    .await?;

    let views = entries.into_iter().map(MemoryEntryView::from).collect();
    Ok(Json(ListResponse {
        agent_id: agent_id.to_string(),
        entries: views,
        uninitialised: false,
    }))
}

/// `DELETE /agents/:id/memory/:key` — soft-delete a memory row via
/// the agent socket (spec §5.2 single-writer invariant). The runtime's
/// `memory::forget` archives the row and suffixes the key with
/// `@forgotten:<ts>` for audit.
///
/// Status codes:
/// - 204: forgotten.
/// - 404: agent doesn't exist or caller doesn't own it (consistent
///   with other endpoints — never leak existence to non-owners).
/// - 409: agent is not running. Dashboard surfaces "start the agent
///   to delete this memory".
/// - 504: agent runtime did not reply within the RPC timeout.
pub async fn forget(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, key)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;
    if !state.registry.is_connected(agent_id).await {
        return Err(ApiError::Conflict(
            "agent is not running — start it to apply memory edits (spec §5.2 single-writer)"
                .into(),
        ));
    }

    let request_id = Uuid::now_v7().to_string();
    let req = MemoryForgetRequest {
        request_id: request_id.clone(),
        key: key.clone(),
    };
    let registry = state.registry.clone();
    let agent_for_send = agent_id;
    let result = state
        .admin_rpc
        .call(
            &request_id,
            agent_id,
            || async move {
                registry
                    .send(agent_for_send, GatewayToAgent::MemoryForgetRequest(req))
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
                "memory.forgot",
                serde_json::json!({"key": key}),
            )
            .await;
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(AdminOutcome::NotFound) => {
            // Idempotent — surfaces 404 so the dashboard can refresh.
            Err(ApiError::NotFound)
        }
        Ok(AdminOutcome::Error { message }) => Err(ApiError::Internal(message)),
        Err(AdminRpcCallError::Send(e)) => Err(ApiError::Internal(format!("rpc send: {e}"))),
        Err(AdminRpcCallError::Rpc(RpcError::Timeout(_))) => Err(ApiError::Internal(
            "agent runtime did not reply to memory.forget within 10s".into(),
        )),
        Err(AdminRpcCallError::Rpc(RpcError::ChannelClosed)) => Err(ApiError::Conflict(
            "agent disconnected before reply arrived; retry once the agent reconnects".into(),
        )),
    }
}
