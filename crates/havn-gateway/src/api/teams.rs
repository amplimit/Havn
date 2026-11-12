//! `/teams` and `/teams/{id}` endpoints (spec §8.3, §10.3).
//!
//! Teams group users for shared agents, shared credentials, and a
//! single audit log. In multi-user mode the operator creates teams with
//! `havn team add`; in single-user mode the API may create them and the
//! creator becomes the team's `admin`. Renames and deletion are
//! admin-only.
//!
//! Sub-resources live in sibling modules:
//! - `/teams/{id}/members` → [`super::members`]
//! - `/teams/{id}/roles`   → [`super::roles`]
//! - `/teams/{id}/agents`  → [`list_agents`] below
//! - `/teams/{id}/audit-log`   → [`super::audit`]
//! - `/teams/{id}/usage`       → [`usage`] below
//! - `/teams/{id}/credentials` → [`super::team_credentials`]
//!
//! All admin-mutating endpoints write a `team.*` audit entry (best-effort).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use havn_core::{Policy, TeamId};
use havn_db::repo::credential_usage::TeamUserUsage;
use havn_db::repo::roles::{NewRole, create as create_role};
use havn_db::repo::team_memberships;
use havn_db::repo::teams::{self as repo, NewTeam};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::perm;

#[derive(Debug, Serialize)]
pub struct TeamView {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// Convenience flag — true when the calling user holds the team's
    /// `admin` role. Lets the dashboard render mgmt buttons without
    /// a second request per row.
    pub is_admin: bool,
    /// True when the caller's role in the team allows another agent of
    /// theirs there (`max_agents`), for the new-agent form.
    pub can_create_agents: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchTeamRequest {
    pub name: Option<String>,
}

/// `GET /teams` — teams the calling user belongs to. Single-user
/// operators see an empty list until they create one.
pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<Vec<TeamView>>, ApiError> {
    let memberships = team_memberships::list_for_user(&state.db, user.id).await?;
    let mut views = Vec::with_capacity(memberships.len());
    for m in memberships {
        let team = repo::find_by_id(&state.db, m.team_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        views.push(TeamView {
            id: team.id.to_string(),
            name: team.name,
            created_at: team.created_at,
            is_admin: m.role_name == "admin",
            can_create_agents: can_create_agents(&state, user.id, m.team_id).await?,
        });
    }
    Ok(Json(views))
}

/// `POST /teams` — single-user mode only: create a team and add the
/// creator as admin.
/// Atomic enough: if the role create or membership add fails after
/// the team is inserted, we leave a half-created team behind. The CLI
/// `havn team delete` cleans up; in practice a failure here is rare
/// (tests exercise the full path) and the dashboard surfaces the
/// 5xx so the operator notices.
pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<CreateTeamRequest>,
) -> Result<(StatusCode, Json<TeamView>), ApiError> {
    // The creator becomes the team's admin and admins set role policies,
    // so letting any user create a team would let any user grant
    // themselves any policy. In multi-user mode teams are provisioned by
    // the operator with `havn team add`.
    if state.trusted_proxies.is_some() {
        return Err(ApiError::Forbidden(
            "teams are created by the operator with `havn team add --name <name> --admin <user-id>`"
                .into(),
        ));
    }
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must be non-empty".into()));
    }
    let team = repo::create(
        &state.db,
        NewTeam {
            name: req.name.trim(),
        },
    )
    .await?;
    info!(team_id = %team.id, name = %team.name, owner = %user.id, "team created");

    // Seed the team's own admin + member roles so admins can edit
    // policies without creating roles by hand.
    let admin_role = create_role(
        &state.db,
        NewRole {
            team_id: Some(team.id),
            name: "admin",
            policy: &admin_default_policy(),
        },
    )
    .await?;
    create_role(
        &state.db,
        NewRole {
            team_id: Some(team.id),
            name: "member",
            policy: &member_default_policy(),
        },
    )
    .await?;

    // Auto-add creator as admin.
    team_memberships::add(&state.db, user.id, team.id, admin_role.id).await?;

    audit::record(
        &state.db,
        user.id,
        Some(team.id),
        None,
        "team.created",
        serde_json::json!({"name": team.name}),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(TeamView {
            id: team.id.to_string(),
            name: team.name,
            created_at: team.created_at,
            is_admin: true,
            can_create_agents: can_create_agents(&state, user.id, team.id).await?,
        }),
    ))
}

/// `GET /teams/{id}` — full view (members must be in the team).
pub async fn get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<TeamView>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_member(&state.db, user.id, id).await?;
    let team = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let is_admin = perm::is_admin(&state.db, user.id, id).await?;
    Ok(Json(TeamView {
        id: team.id.to_string(),
        name: team.name,
        created_at: team.created_at,
        is_admin,
        can_create_agents: can_create_agents(&state, user.id, id).await?,
    }))
}

/// `PATCH /teams/{id}` — admin-only rename.
pub async fn patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<PatchTeamRequest>,
) -> Result<Json<TeamView>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_admin(&state.db, user.id, id).await?;
    if let Some(name) = req.name {
        if name.trim().is_empty() {
            return Err(ApiError::BadRequest("name must be non-empty".into()));
        }
        let renamed = repo::rename(&state.db, id, name.trim()).await?;
        audit::record(
            &state.db,
            user.id,
            Some(id),
            None,
            "team.renamed",
            serde_json::json!({"name": renamed.name}),
        )
        .await;
        let is_admin = perm::is_admin(&state.db, user.id, id).await?;
        return Ok(Json(TeamView {
            id: renamed.id.to_string(),
            name: renamed.name,
            created_at: renamed.created_at,
            is_admin,
            can_create_agents: can_create_agents(&state, user.id, id).await?,
        }));
    }
    // No-op patch — return current state.
    let team = repo::find_by_id(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let is_admin = perm::is_admin(&state.db, user.id, id).await?;
    Ok(Json(TeamView {
        id: team.id.to_string(),
        name: team.name,
        created_at: team.created_at,
        is_admin,
        can_create_agents: can_create_agents(&state, user.id, id).await?,
    }))
}

/// `DELETE /teams/{id}` — admin-only. Cascades through roles and
/// memberships. The team's running agents are stopped first; the FK rule
/// then sets their team_id to NULL, so they are kept (operators preserve
/// work) as their owners' personal agents, governed by the owner's own
/// policy from the next start on.
///
/// Audit shape: the deletion is recorded as a USER action (team_id
/// `None`), with the team's name + id snapshotted into details. The
/// team-scoped audit endpoint can't surface this entry (the team is
/// gone), but `/me/audit-log` shows it for the deleting admin with
/// full forensic context. Spec §11 treats audit as a soft signal,
/// so this trade-off is fine — the operator who needs to forensic
/// "who deleted team X" greps user audits, not team ones.
pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    perm::require_admin(&state.db, user.id, id).await?;

    // Snapshot the team name BEFORE the delete cascades.
    let snapshot_name = repo::find_by_id(&state.db, id)
        .await?
        .map_or_else(|| "(unknown)".into(), |t| t.name);

    let stopped =
        crate::api::agents::stop_team_agents(&state, id, None, user.id, "team deleted").await?;
    repo::delete(&state.db, id).await?;
    audit::record(
        &state.db,
        user.id,
        None,
        None,
        "team.deleted",
        serde_json::json!({
            "team_id": id.to_string(),
            "team_name": snapshot_name,
            "agents_stopped": stopped,
        }),
    )
    .await;
    info!(team_id = %id, by = %user.id, "team deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---- /teams/{id}/agents ---------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TeamAgentView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub owner_id: String,
    pub owner_display_name: String,
    pub created_at: DateTime<Utc>,
}

/// `GET /teams/{id}/agents` — every agent rows with `team_id = id`.
/// Members can view; admins use this to bulk-stop runaway agents
/// (the bulk action is dashboard-side via per-agent stop calls;
/// the listing endpoint is members-only because seeing what's
/// running is non-sensitive).
pub async fn list_agents(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<TeamAgentView>>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_member(&state.db, user.id, id).await?;

    // Inline join — havn-db's agent repo doesn't have a "by team_id"
    // helper yet (every prior consumer keyed on owner). Hand-rolling
    // the SQL keeps the change local; promote to a repo function when
    // a third caller appears.
    use sqlx::Row as _;
    let rows = sqlx::query(
        "SELECT a.id, a.name, a.status, a.owner_id, u.display_name AS owner_display_name, \
                a.created_at \
         FROM agents a \
         JOIN users u ON u.id = a.owner_id \
         WHERE a.team_id = ?1 \
         ORDER BY a.created_at DESC",
    )
    .bind(id.to_string())
    .fetch_all(&state.db)
    .await
    .map_err(|e| ApiError::Internal(format!("query: {e}")))?;

    let views: Vec<TeamAgentView> = rows
        .into_iter()
        .filter_map(|r| {
            Some(TeamAgentView {
                id: r.try_get::<String, _>("id").ok()?,
                name: r.try_get::<String, _>("name").ok()?,
                status: r.try_get::<String, _>("status").ok()?,
                owner_id: r.try_get::<String, _>("owner_id").ok()?,
                owner_display_name: r.try_get::<String, _>("owner_display_name").ok()?,
                created_at: r.try_get::<DateTime<Utc>, _>("created_at").ok()?,
            })
        })
        .collect();
    Ok(Json(views))
}

// ---- /teams/{id}/usage ----------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// Lookback window in days. Defaults to 30. Caps at 365 to keep
    /// the rollup cheap.
    pub days: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TeamUsageEntry {
    pub user_id: String,
    pub display_name: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub call_count: i64,
}

#[derive(Debug, Serialize)]
pub struct TeamUsageResponse {
    pub team_id: String,
    pub since: DateTime<Utc>,
    pub entries: Vec<TeamUsageEntry>,
}

/// `GET /teams/{id}/usage?days=30` — token totals per user of the team's
/// agents and credentials (see `list_team_usage`). Spec §10.3 admin view:
/// admin-only, since it shows every member's activity.
pub async fn usage(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<TeamUsageResponse>, ApiError> {
    let id = parse_id(&id)?;
    perm::require_admin(&state.db, user.id, id).await?;

    let days = q.days.unwrap_or(30).min(365);
    let since = Utc::now() - ChronoDuration::days(days.into());
    let since_rfc = since.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let rows = havn_db::repo::credential_usage::list_team_usage(&state.db, id, &since_rfc).await?;
    Ok(Json(TeamUsageResponse {
        team_id: id.to_string(),
        since,
        entries: rows.into_iter().map(into_usage_entry).collect(),
    }))
}

fn into_usage_entry(u: TeamUserUsage) -> TeamUsageEntry {
    TeamUsageEntry {
        user_id: u.user_id.to_string(),
        display_name: u.display_name,
        tokens_in: u.tokens_in,
        tokens_out: u.tokens_out,
        call_count: u.call_count,
    }
}

// ---- helpers --------------------------------------------------------------

async fn can_create_agents(
    state: &AppState,
    user_id: havn_core::UserId,
    team_id: TeamId,
) -> Result<bool, ApiError> {
    let role = crate::policy_resolver::for_team_member(&state.db, user_id, team_id).await;
    let owned = havn_db::repo::agents::count_for_owner_in_team(&state.db, user_id, team_id).await?;
    Ok(owned < role.max_agents)
}

fn parse_id(s: &str) -> Result<TeamId, ApiError> {
    TeamId::from_str(s).map_err(|_| ApiError::BadRequest("invalid team id".into()))
}

/// Default policy for a freshly-created team's `admin` role. Mirrors
/// the system seed but lives independently in code so dashboard
/// admins can edit it without touching system roles.
fn admin_default_policy() -> Policy {
    let mut p = Policy {
        max_agents: 50,
        ..Policy::default()
    };
    p.permissions.can_use_shell = true;
    p.permissions.can_spawn_subagents = true;
    p.admin_visibility.can_view_audit_log = true;
    p
}

fn member_default_policy() -> Policy {
    let mut p = Policy {
        max_agents: 5,
        ..Policy::default()
    };
    p.permissions.can_use_shell = false;
    p.permissions.can_spawn_subagents = false;
    p.admin_visibility.can_view_audit_log = false;
    p
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::auth::tests::state_with;
    use havn_core::AgentId;
    use havn_db::repo::agents::AgentStatus;

    fn request() -> Json<CreateTeamRequest> {
        Json(CreateTeamRequest {
            name: "self-made".into(),
        })
    }

    #[tokio::test]
    async fn create_is_rejected_in_multi_user_mode() {
        let (state, user_id) = state_with(true).await;
        let user = AuthedUser {
            id: user_id,
            display_name: "member".into(),
        };
        let err = create(State(state.clone()), user, request())
            .await
            .expect_err("trust_header mode must refuse");
        assert!(matches!(err, ApiError::Forbidden(_)), "{err:?}");
        let teams = repo::list_all(&state.db).await.expect("list teams");
        assert!(teams.is_empty(), "no team may be created: {teams:?}");
    }

    #[tokio::test]
    async fn create_is_allowed_in_single_user_mode() {
        let (state, user_id) = state_with(false).await;
        let user = AuthedUser {
            id: user_id,
            display_name: "operator".into(),
        };
        let (status, Json(team)) = create(State(state.clone()), user, request())
            .await
            .expect("single-user mode allows team creation");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(team.name, "self-made");
    }

    struct TeamFixture {
        state: AppState,
        team: TeamId,
        admin: AuthedUser,
        member: AuthedUser,
        viewer: AuthedUser,
        outsider: AuthedUser,
    }

    async fn team_fixture() -> TeamFixture {
        use havn_db::repo::users::{NewUser, create as create_user};
        let (state, admin_id) = state_with(true).await;
        let team = repo::create(&state.db, NewTeam { name: "t" })
            .await
            .expect("team");
        let role = |name: &'static str, max_agents: u32| {
            let db = state.db.clone();
            async move {
                let policy = Policy {
                    max_agents,
                    ..admin_default_policy()
                };
                create_role(
                    &db,
                    NewRole {
                        team_id: Some(team.id),
                        name,
                        policy: &policy,
                    },
                )
                .await
                .expect("role")
                .id
            }
        };
        let admin_role = role("admin", 50).await;
        let member_role = role("member", 1).await;
        let viewer_role = role("viewer", 0).await;
        let user = |name: &'static str| {
            let db = state.db.clone();
            async move {
                let u = create_user(&db, NewUser { display_name: name })
                    .await
                    .expect("user");
                AuthedUser {
                    id: u.id,
                    display_name: name.into(),
                }
            }
        };
        let admin = AuthedUser {
            id: admin_id,
            display_name: "admin".into(),
        };
        let member = user("member").await;
        let viewer = user("viewer").await;
        let outsider = user("outsider").await;
        for (u, r) in [
            (&admin, admin_role),
            (&member, member_role),
            (&viewer, viewer_role),
        ] {
            team_memberships::add(&state.db, u.id, team.id, r)
                .await
                .expect("membership");
        }
        TeamFixture {
            state,
            team: team.id,
            admin,
            member,
            viewer,
            outsider,
        }
    }

    fn agent_request(
        name: &str,
        team: Option<TeamId>,
    ) -> Json<crate::api::agents::CreateAgentRequest> {
        Json(crate::api::agents::CreateAgentRequest {
            name: name.into(),
            config: None,
            team_id: team.map(|t| t.to_string()),
        })
    }

    async fn team_agent_names(f: &TeamFixture, user: &AuthedUser) -> Result<Vec<String>, ApiError> {
        let Json(agents) = list_agents(
            State(f.state.clone()),
            user.clone(),
            Path(f.team.to_string()),
        )
        .await?;
        Ok(agents.into_iter().map(|a| a.name).collect())
    }

    #[tokio::test]
    async fn agents_are_created_in_a_team_within_the_role_quota() {
        use crate::api::agents::create as create_agent;
        let f = team_fixture().await;
        let (status, Json(agent)) = create_agent(
            State(f.state.clone()),
            f.member.clone(),
            agent_request("shared", Some(f.team)),
        )
        .await
        .expect("member may create a team agent");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(agent.team_id, Some(f.team.to_string()));

        let over_quota = create_agent(
            State(f.state.clone()),
            f.member.clone(),
            agent_request("second", Some(f.team)),
        )
        .await;
        assert!(
            matches!(over_quota, Err(ApiError::Forbidden(_))),
            "{over_quota:?}"
        );
        let (_, Json(personal)) = create_agent(
            State(f.state.clone()),
            f.admin.clone(),
            agent_request("personal", None),
        )
        .await
        .expect("personal agent");
        assert_eq!(personal.team_id, None);

        for user in [&f.viewer, &f.outsider] {
            let denied = create_agent(
                State(f.state.clone()),
                user.clone(),
                agent_request("nope", Some(f.team)),
            )
            .await;
            assert!(matches!(denied, Err(ApiError::Forbidden(_))), "{denied:?}");
        }
        let invalid = create_agent(
            State(f.state.clone()),
            f.admin.clone(),
            Json(crate::api::agents::CreateAgentRequest {
                name: "bad".into(),
                config: None,
                team_id: Some("not-a-uuid".into()),
            }),
        )
        .await;
        assert!(
            matches!(invalid, Err(ApiError::BadRequest(_))),
            "{invalid:?}"
        );
        let bad_hooks = create_agent(
            State(f.state.clone()),
            f.admin.clone(),
            Json(crate::api::agents::CreateAgentRequest {
                name: "bad-hooks".into(),
                config: Some(serde_json::json!({
                    "policy": { "hooks": { "pre_tool_use": [{ "tool": "shell", "deny": ["[unclosed"] }] } }
                })),
                team_id: Some(f.team.to_string()),
            }),
        )
        .await;
        assert!(
            matches!(bad_hooks, Err(ApiError::BadRequest(_))),
            "a team agent's policy override is validated too: {bad_hooks:?}"
        );

        assert_eq!(
            team_agent_names(&f, &f.viewer).await.expect("members list"),
            ["shared"]
        );
        let outsider = team_agent_names(&f, &f.outsider).await;
        assert!(
            matches!(outsider, Err(ApiError::Forbidden(_))),
            "{outsider:?}"
        );
    }

    #[tokio::test]
    async fn team_usage_counts_only_team_resources_and_is_admin_only() {
        use havn_core::UserId;
        use havn_db::repo::agents::{NewAgent, create as create_agent_row};
        use havn_db::repo::credential_usage::{Caps, Reservation, reserve, settle};
        use havn_db::repo::credentials::{CredentialScope, NewCredential, create as create_cred};
        let f = team_fixture().await;
        let db = &f.state.db;
        let cred = |scope: CredentialScope, scope_id: String| async move {
            create_cred(
                db,
                NewCredential {
                    scope,
                    scope_id: &scope_id,
                    provider: "llm:anthropic",
                    name: None,
                    api_key_ciphertext: b"x",
                    priority: 0,
                    limits: serde_json::json!({}),
                },
            )
            .await
            .expect("credential")
            .id
        };
        let team_cred = cred(CredentialScope::Team, f.team.to_string()).await;
        let own_cred = cred(CredentialScope::User, f.member.id.to_string()).await;
        let agent = |team_id: Option<TeamId>, name: &'static str| async move {
            create_agent_row(
                db,
                NewAgent {
                    owner_id: f.member.id,
                    team_id,
                    name,
                    config: serde_json::json!({}),
                },
            )
            .await
            .expect("agent")
            .id
        };
        let team_agent = agent(Some(f.team), "team-agent").await;
        let own_agent = agent(None, "own-agent").await;
        // Recorded the way the LLM proxy does: reserve, then settle.
        let spend = |user: UserId, credential, agent_id, tokens: i64| async move {
            let reserved = reserve(
                db,
                Reservation {
                    credential_id: credential,
                    user_id: user,
                    agent_id,
                    provider: "anthropic",
                    model: "m",
                    tokens_in: tokens,
                    max_tokens_out: 1,
                    min_tokens_out: 1,
                    caps: Caps::default(),
                    day_start: "1970-01-01T00:00:00.000Z",
                    minute_start: "1970-01-01T00:00:00.000Z",
                },
            )
            .await
            .expect("reserve")
            .expect("no caps");
            settle(db, &reserved.id, "m", tokens, 0)
                .await
                .expect("settle");
        };
        // Counted: team credential, and team agent on a personal credential.
        spend(f.member.id, team_cred, Some(own_agent), 10).await;
        spend(f.member.id, own_cred, Some(team_agent), 100).await;
        // Not counted: a member's own credential on their own agent.
        spend(f.member.id, own_cred, Some(own_agent), 1000).await;
        spend(f.member.id, own_cred, None, 10_000).await;
        // A call by this team's agent on another team's credential (the
        // resolver no longer allows it; older rows may exist) counts for
        // the agent's team only.
        let other = repo::create(db, NewTeam { name: "other" })
            .await
            .expect("team");
        let other_admin = create_role(
            db,
            NewRole {
                team_id: Some(other.id),
                name: "admin",
                policy: &admin_default_policy(),
            },
        )
        .await
        .expect("role");
        team_memberships::add(db, f.admin.id, other.id, other_admin.id)
            .await
            .expect("membership");
        let other_cred = cred(CredentialScope::Team, other.id.to_string()).await;
        spend(f.member.id, other_cred, Some(team_agent), 5).await;

        let read = |user: AuthedUser| {
            usage(
                State(f.state.clone()),
                user,
                Path(f.team.to_string()),
                Query(UsageQuery { days: None }),
            )
        };
        let Json(resp) = read(f.admin.clone()).await.expect("admin reads usage");
        let member = resp
            .entries
            .iter()
            .find(|e| e.user_id == f.member.id.to_string())
            .expect("member row");
        assert_eq!(member.tokens_in, 115);
        assert_eq!(member.call_count, 3);
        let admin = resp
            .entries
            .iter()
            .find(|e| e.user_id == f.admin.id.to_string())
            .expect("members without usage are listed");
        assert_eq!(admin.tokens_in, 0);
        assert!(
            !resp
                .entries
                .iter()
                .any(|e| e.user_id == f.outsider.id.to_string()),
            "{:?}",
            resp.entries
        );

        for user in [f.member.clone(), f.outsider.clone()] {
            let denied = read(user).await;
            assert!(matches!(denied, Err(ApiError::Forbidden(_))), "{denied:?}");
        }

        let Json(other_usage) = usage(
            State(f.state.clone()),
            f.admin.clone(),
            Path(other.id.to_string()),
            Query(UsageQuery { days: None }),
        )
        .await
        .expect("admin of the other team");
        assert!(
            other_usage.entries.iter().all(|e| e.tokens_in == 0),
            "another team's agent is not counted again: {:?}",
            other_usage.entries
        );
    }

    async fn running_team_agent(f: &TeamFixture, owner: &AuthedUser) -> AgentId {
        use crate::api::agents::create as create_agent;
        let (_, Json(agent)) = create_agent(
            State(f.state.clone()),
            owner.clone(),
            agent_request("worker", Some(f.team)),
        )
        .await
        .expect("team agent");
        let id = AgentId::from_str(&agent.id).expect("id");
        havn_db::repo::agents::set_status(&f.state.db, id, AgentStatus::Running)
            .await
            .expect("status");
        id
    }

    async fn status_of(f: &TeamFixture, id: AgentId) -> AgentStatus {
        havn_db::repo::agents::find_by_id(&f.state.db, id)
            .await
            .expect("lookup")
            .expect("agent kept")
            .status
    }

    #[tokio::test]
    async fn removing_a_member_stops_their_team_agents() {
        let f = team_fixture().await;
        let id = running_team_agent(&f, &f.member).await;

        crate::api::members::remove(
            State(f.state.clone()),
            f.admin.clone(),
            Path((f.team.to_string(), f.member.id.to_string())),
        )
        .await
        .expect("remove member");
        assert_eq!(status_of(&f, id).await, AgentStatus::Stopped);

        let start = crate::api::agents::ensure_running(&f.state, f.member.id, id).await;
        assert!(matches!(start, Err(ApiError::Forbidden(_))), "{start:?}");

        let cron = crate::api::cron::create(
            State(f.state.clone()),
            f.member.clone(),
            Path(id.to_string()),
            Json(crate::api::cron::CreateCronRequest {
                name: "nightly".into(),
                prompt: "p".into(),
                schedule: crate::api::cron::ScheduleInput::Interval {
                    interval_seconds: 3600,
                },
                next_run_at: None,
                wake_check: None,
                deliver: "local".into(),
                enabled_toolsets: Vec::new(),
            }),
        )
        .await;
        assert!(matches!(cron, Err(ApiError::Forbidden(_))), "{cron:?}");
    }

    #[tokio::test]
    async fn team_admins_may_stop_team_agents_and_others_may_not() {
        let f = team_fixture().await;
        let id = running_team_agent(&f, &f.member).await;
        let stop = |user: &AuthedUser| {
            crate::api::agents::stop(State(f.state.clone()), user.clone(), Path(id.to_string()))
        };

        for user in [&f.viewer, &f.outsider] {
            let denied = stop(user).await;
            assert!(matches!(denied, Err(ApiError::NotFound)), "{denied:?}");
        }
        assert_eq!(status_of(&f, id).await, AgentStatus::Running);

        let Json(stopped) = stop(&f.admin).await.expect("team admin stops it");
        assert_eq!(stopped.status, "stopped");
        assert_eq!(status_of(&f, id).await, AgentStatus::Stopped);
    }

    #[tokio::test]
    async fn deleting_a_team_stops_and_keeps_its_agents() {
        let f = team_fixture().await;
        let id = running_team_agent(&f, &f.member).await;
        delete(
            State(f.state.clone()),
            f.admin.clone(),
            Path(f.team.to_string()),
        )
        .await
        .expect("delete team");
        let agent = havn_db::repo::agents::find_by_id(&f.state.db, id)
            .await
            .expect("lookup")
            .expect("agent kept");
        assert_eq!(agent.status, AgentStatus::Stopped);
        assert_eq!(agent.team_id, None);
    }

    #[tokio::test]
    async fn team_view_reports_whether_another_agent_fits() {
        let f = team_fixture().await;
        let view = |user: &AuthedUser| {
            get(
                State(f.state.clone()),
                user.clone(),
                Path(f.team.to_string()),
            )
        };
        let Json(before) = view(&f.member).await.expect("view");
        assert!(before.can_create_agents);
        running_team_agent(&f, &f.member).await;
        let Json(after) = view(&f.member).await.expect("view");
        assert!(!after.can_create_agents, "member role allows one agent");
        let Json(viewer) = view(&f.viewer).await.expect("view");
        assert!(!viewer.can_create_agents);
    }
}
