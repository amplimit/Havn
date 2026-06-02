//! `/agents/:id/memory` — admin view of the agent's typed memory
//! (spec §9.4, §5.2 nuance — see `havn_db::agent::connect_read_only`).
//!
//! Lets the user see, in the dashboard, exactly what the agent has
//! remembered about them, grouped by kind, with the supersedes audit
//! trail. Reads go straight to agent.db (RO connection) so the
//! dashboard works even when the runtime is offline.
//!
//! `DELETE /agents/:id/memory/:key` routes the request through the
//! agent socket as a [`havn_proto::MemoryForgetRequest`] frame —
//! agent.db remains single-writer (spec §5.2). Requires the agent to
//! be running; offline deletes return 409.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use havn_db::agent::memory::{self, Kind};
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
    /// True when the dashboard should render "agent isn't running OR
    /// has no memory yet" rather than "0 of N". The agent.db file
    /// hasn't been opened by the agent runtime even once.
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

    let agent_db_path = state
        .workspace_root
        .join(agent_id.to_string())
        .join("workspace")
        .join("agent.db");
    if !tokio::fs::try_exists(&agent_db_path).await.unwrap_or(false) {
        return Ok(Json(ListResponse {
            agent_id: agent_id.to_string(),
            entries: Vec::new(),
            uninitialised: true,
        }));
    }

    let pool = match havn_db::agent::connect_read_only(&agent_db_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                agent_id = %id,
                error = %e,
                "memory list: opening agent.db RO failed"
            );
            return Err(ApiError::Internal("opening agent.db".into()));
        }
    };

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let kinds: Vec<Kind> = q
        .kind
        .as_deref()
        .and_then(Kind::parse)
        .map(|k| vec![k])
        .unwrap_or_default();

    // We use list_active for the unfiltered case; for kind filter we
    // run a tiny query inline since memory's public surface only
    // exposes FTS search by kind. The dashboard's "filter by kind"
    // case is small enough that this duplication isn't worth a new
    // repo function for now — refactor when a third caller appears.
    let entries = if kinds.is_empty() {
        memory::list_active(&pool, limit).await?
    } else {
        list_active_by_kind(&pool, &kinds[0], limit).await?
    };

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

/// Active rows of a single kind, newest first. Inline because the
/// memory repo's public API only filters by kind on the FTS path.
async fn list_active_by_kind(
    pool: &sqlx::SqlitePool,
    kind: &Kind,
    limit: u32,
) -> Result<Vec<memory::Entry>, havn_db::DbError> {
    use chrono::Duration as ChronoDuration;
    // Borrow the recent_events query shape for the kind+limit case;
    // it returns the same Entry struct.
    if matches!(kind, Kind::Event) {
        // Use a wide window so the dashboard sees the full event tail.
        return memory::recent_events(pool, ChronoDuration::days(365), limit).await;
    }
    // For non-event kinds, all active rows of that kind, newest first.
    use sqlx::Row as _;
    let rows = sqlx::query(
        "SELECT id, key, value, kind, source, ttl_days, archived_at, created_at, updated_at, \
                recall_count, last_recalled_at, supersedes_id \
         FROM memory \
         WHERE archived_at IS NULL AND kind = ?1 \
         ORDER BY updated_at DESC LIMIT ?2",
    )
    .bind(kind.as_str())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    // Hand-deserialise into Entry; we can't reuse memory::Entry's
    // FromRow because it's private to the memory module.
    let entries = rows
        .into_iter()
        .filter_map(|r| {
            Some(memory::Entry {
                id: r.try_get::<String, _>("id").ok()?,
                key: r.try_get::<String, _>("key").ok()?,
                value: r.try_get::<String, _>("value").ok()?,
                kind: Kind::parse(&r.try_get::<String, _>("kind").ok()?)
                    .unwrap_or(Kind::Preference),
                source: havn_db::agent::memory::Source::parse(
                    &r.try_get::<String, _>("source").ok()?,
                )
                .unwrap_or(havn_db::agent::memory::Source::AgentInferred),
                ttl_days: r.try_get::<Option<i64>, _>("ttl_days").ok()?,
                archived_at: r.try_get::<Option<DateTime<Utc>>, _>("archived_at").ok()?,
                created_at: r.try_get::<DateTime<Utc>, _>("created_at").ok()?,
                updated_at: r.try_get::<DateTime<Utc>, _>("updated_at").ok()?,
                recall_count: r.try_get::<i64, _>("recall_count").ok()?,
                last_recalled_at: r
                    .try_get::<Option<DateTime<Utc>>, _>("last_recalled_at")
                    .ok()?,
                supersedes_id: r.try_get::<Option<String>, _>("supersedes_id").ok()?,
            })
        })
        .collect();
    Ok(entries)
}
