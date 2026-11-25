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
//! be running; offline deletes return 409. `PUT` saves an entry the
//! user added or edited the same way, as a data request, and `PUT
//! .../visibility` makes one private or public (issue #309).

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
    pub id: String,
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
    pub visibility: &'static str,
    pub suggested_public: bool,
}

impl From<memory::Entry> for MemoryEntryView {
    fn from(e: memory::Entry) -> Self {
        Self {
            id: e.id,
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
            visibility: e.visibility.as_str(),
            suggested_public: e.suggested_public,
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

#[derive(Debug, Deserialize)]
pub struct SaveRequest {
    pub value: String,
    pub kind: String,
    #[serde(default)]
    pub expiry: havn_proto::MemoryExpiry,
}

/// `PUT /agents/:id/memory/:key` — save `value` under `key` as something
/// the user said, added or edited in the dashboard. The running agent
/// writes it (409 when it is stopped); an entry with another value or
/// kind is kept, archived, as the one the new one supersedes. Answers
/// with the saved entry.
pub async fn save(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, key)): Path<(String, String)>,
    Json(body): Json<SaveRequest>,
) -> Result<Json<MemoryEntryView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;
    havn_proto::check_memory(&key, &body.value, body.expiry).map_err(ApiError::BadRequest)?;
    if memory::Kind::parse(&body.kind).is_none() {
        return Err(ApiError::BadRequest(format!(
            "kind is identity, preference, project or event, not {:?}",
            body.kind
        )));
    }
    let kind = body.kind.clone();
    let entry: memory::Entry = crate::api::agents::agent_data(
        &state,
        user.id,
        agent_id,
        havn_proto::DataQuery::Remember {
            key,
            value: body.value,
            memory_kind: body.kind,
            expiry: body.expiry,
        },
        false,
    )
    .await?;
    audit::record_agent_action(
        &state.db,
        user.id,
        agent_id,
        "memory.saved",
        serde_json::json!({"key": entry.key, "kind": kind}),
    )
    .await;
    Ok(Json(MemoryEntryView::from(entry)))
}

#[derive(Debug, Deserialize)]
pub struct UndoRequest {
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
}

/// `POST /agents/:id/memory/:key/undo` — undo a change a chat showed:
/// entry `after` is taken out and entry `before` brought back, as they
/// were (row ids from the change). 204, or 409 when the key changed since
/// (a later save, or the change already undone). The running agent does
/// it, like the other writes.
pub async fn undo(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, key)): Path<(String, String)>,
    Json(body): Json<UndoRequest>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;
    if body.before.is_none() && body.after.is_none() {
        return Err(ApiError::BadRequest("nothing to undo".into()));
    }
    #[derive(Deserialize)]
    struct Undone {
        undone: bool,
    }
    let reply: Undone = crate::api::agents::agent_data(
        &state,
        user.id,
        agent_id,
        havn_proto::DataQuery::UndoMemory {
            key: key.clone(),
            before: body.before.clone(),
            after: body.after.clone(),
        },
        false,
    )
    .await?;
    if !reply.undone {
        return Err(ApiError::Conflict(
            "this memory changed since; see the memory page".into(),
        ));
    }
    audit::record_agent_action(
        &state.db,
        user.id,
        agent_id,
        "memory.undone",
        serde_json::json!({"key": key, "before": body.before, "after": body.after}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct VisibilityRequest {
    pub visibility: String,
    /// The entry the switch was shown for (a row id); when given, nothing
    /// changes once the key holds another.
    #[serde(default)]
    pub entry: Option<String>,
}

/// `PUT /agents/:id/memory/:key/visibility` — make the entry under `key`
/// private or public (issue #309): public facts are put in the prompt of
/// every chat, other people's and groups' included. Only the agent's owner
/// may (`resolve_agent`; anyone else gets 404): this is the only way a
/// fact the agent saved becomes public, and it settles the agent's
/// suggestion either way. Answers with the entry;
/// 409 when the key changed since (or the agent is stopped). Audited as
/// `memory.visibility`, with the entry and who changed it.
pub async fn set_visibility(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, key)): Path<(String, String)>,
    Json(body): Json<VisibilityRequest>,
) -> Result<Json<MemoryEntryView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let agent_id = agent.id;
    let Some(visibility) = memory::Visibility::parse(&body.visibility) else {
        return Err(ApiError::BadRequest(format!(
            "visibility is private or public, not {:?}",
            body.visibility
        )));
    };
    #[derive(Deserialize)]
    struct Set {
        entry: Option<memory::Entry>,
        #[serde(default)]
        was: Option<String>,
    }
    let reply: Set = crate::api::agents::agent_data(
        &state,
        user.id,
        agent_id,
        havn_proto::DataQuery::SetMemoryVisibility {
            key: key.clone(),
            entry: body.entry.clone(),
            visibility: visibility.as_str().into(),
        },
        false,
    )
    .await?;
    let Some(entry) = reply.entry else {
        return Err(ApiError::Conflict(
            "this memory changed since; see the memory page".into(),
        ));
    };
    audit::record_agent_action(
        &state.db,
        user.id,
        agent_id,
        "memory.visibility",
        serde_json::json!({
            "key": entry.key,
            "entry": entry.id,
            "visibility": visibility.as_str(),
            "was": reply.was,
        }),
    )
    .await;
    Ok(Json(MemoryEntryView::from(entry)))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_db::repo::audit::{self, ListFilter};
    use havn_spawner::AgentHandle;

    /// An agent of `owner`'s, connected, whose runtime answers each data
    /// request with `answer` until the test ends.
    async fn answering_agent(
        state: &AppState,
        owner: havn_core::UserId,
        answer: serde_json::Value,
    ) -> havn_core::AgentId {
        let agent = havn_db::repo::agents::create(
            &state.db,
            havn_db::repo::agents::NewAgent {
                owner_id: owner,
                team_id: None,
                name: "remembers",
                config: serde_json::json!({}),
            },
        )
        .await
        .unwrap()
        .id;
        state.registry.register(AgentHandle::new(agent, 1)).await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        state.registry.attach(agent, tx).await.expect("attach");
        let rpc = state.admin_rpc.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if let GatewayToAgent::DataRequest(req) = frame {
                    rpc.resolve(
                        agent,
                        &req.request_id,
                        AdminOutcome::Ok {
                            details: answer.clone(),
                        },
                    )
                    .await;
                }
            }
        });
        agent
    }

    fn entry(visibility: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "row-1",
            "key": "user.name",
            "value": "Mira",
            "kind": "Identity",
            "source": "UserTold",
            "ttl_days": null,
            "archived_at": null,
            "created_at": "2026-11-24T08:00:00Z",
            "updated_at": "2026-11-24T08:00:00Z",
            "recall_count": 0,
            "last_recalled_at": null,
            "supersedes_id": null,
            "visibility": visibility,
        })
    }

    fn request(visibility: &str) -> Json<VisibilityRequest> {
        Json(VisibilityRequest {
            visibility: visibility.into(),
            entry: Some("row-1".into()),
        })
    }

    #[tokio::test]
    async fn only_the_owner_switches_a_fact_and_the_switch_is_audited() {
        let (state, owner) = crate::auth::tests::state_with(false).await;
        let agent = answering_agent(
            &state,
            owner,
            serde_json::json!({"entry": entry("public"), "was": "private"}),
        )
        .await;
        let someone = havn_db::repo::users::create(
            &state.db,
            havn_db::repo::users::NewUser {
                display_name: "someone",
            },
        )
        .await
        .unwrap()
        .id;
        let path = || Path((agent.to_string(), "user.name".to_string()));

        let refused = set_visibility(
            State(state.clone()),
            AuthedUser {
                id: someone,
                display_name: "someone".into(),
            },
            path(),
            request("public"),
        )
        .await;
        assert!(matches!(refused, Err(ApiError::NotFound)), "{refused:?}");
        let odd = set_visibility(
            State(state.clone()),
            AuthedUser {
                id: owner,
                display_name: "owner".into(),
            },
            path(),
            request("everyone"),
        )
        .await;
        assert!(matches!(odd, Err(ApiError::BadRequest(_))), "{odd:?}");

        let Json(view) = set_visibility(
            State(state.clone()),
            AuthedUser {
                id: owner,
                display_name: "owner".into(),
            },
            path(),
            request("public"),
        )
        .await
        .expect("switched");
        assert_eq!(view.visibility, "public");
        assert_eq!(view.id, "row-1");

        let rows = audit::list(
            &state.db,
            ListFilter {
                action_prefix: Some("memory.visibility".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("audit");
        assert_eq!(rows.len(), 1, "the refused ones are not recorded");
        assert_eq!(rows[0].user_id, owner);
        assert_eq!(rows[0].agent_id, Some(agent));
        assert_eq!(rows[0].details["entry"], "row-1");
        assert_eq!(rows[0].details["key"], "user.name");
        assert_eq!(rows[0].details["visibility"], "public");
        assert_eq!(rows[0].details["was"], "private");
    }

    #[tokio::test]
    async fn a_switch_for_an_entry_replaced_since_is_a_conflict() {
        let (state, owner) = crate::auth::tests::state_with(false).await;
        let agent = answering_agent(&state, owner, serde_json::json!({"entry": null})).await;
        let changed = set_visibility(
            State(state.clone()),
            AuthedUser {
                id: owner,
                display_name: "owner".into(),
            },
            Path((agent.to_string(), "user.name".to_string())),
            request("private"),
        )
        .await;
        assert!(matches!(changed, Err(ApiError::Conflict(_))), "{changed:?}");
        let rows = audit::list(
            &state.db,
            ListFilter {
                action_prefix: Some("memory.visibility".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("audit");
        assert!(rows.is_empty());
    }
}
