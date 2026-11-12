//! Per-agent policy resolution (spec §6).
//!
//! `for_user` derives a user's policy from their team roles (or the
//! single-user default when they belong to no team). `for_session` adds
//! the per-agent override (`Agent.config["policy"]`): applied as written
//! in single-user mode, clamped to the owner's policy in multi-user mode.
//!
//! The snapshot shipped to the runtime in the `Welcome` frame (and the
//! sandbox set up at spawn) is resolved once per agent connection, so
//! mid-session policy edits do not change the running runtime's tools or
//! sandbox. Gateway-side checks on each request (queries, LLM calls)
//! resolve the policy again and therefore see edits immediately.

use havn_core::{Policy, UserId};
use havn_db::repo::agents::Agent;
use sqlx::SqlitePool;
use tracing::warn;

/// Resolve the effective policy for a user (independent of any one
/// agent — used to enforce `max_agents`, `can_bind_channels`,
/// `can_schedule_cron`, etc. *before* an agent is created or a
/// channel/cron is added).
///
/// Without team memberships this is `Policy::default()` with `max_agents`
/// lifted to a single-user cap (50, or `HAVN_MAX_AGENTS_PER_USER`). The
/// Policy struct's own default of 1 is the conservative spec default for an
/// *unauthorized* user.
///
/// Walks `team_memberships → roles → policy` for `user_id`. When the
/// user belongs to one or more teams, returns the most-permissive
/// combination of their role policies themselves ([`Policy::widen_with`]),
/// so a role's restrictions hold (hook deny rules are the exception: every
/// role's rules are kept); `HAVN_MAX_AGENTS_PER_USER` caps
/// `max_agents`. When the user belongs to no team, returns the single-user
/// default. Lookup failures fall back to a restrictive policy.
///
/// The "max wins" merge is deliberate: a user who is admin of team A
/// and a viewer of team B should get the union of capabilities,
/// because they're the same human. Per-team scoping (e.g. "you can
/// only spawn shell tools on team A's agents") is a future refinement
/// and would live on the agent's policy override, not here.
pub async fn for_user(db: &SqlitePool, user_id: UserId) -> Policy {
    let mut base = Policy::default();
    let env_cap = std::env::var("HAVN_MAX_AGENTS_PER_USER")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&n| n > 0);
    base.max_agents = env_cap.unwrap_or(50);

    let memberships = match havn_db::repo::team_memberships::list_for_user(db, user_id).await {
        Ok(m) => m,
        Err(e) => {
            warn!(%user_id, error = %e, "team membership lookup failed; using the most restrictive policy");
            return restrictive_fallback();
        }
    };
    if memberships.is_empty() {
        return base;
    }
    let mut merged: Option<Policy> = None;
    for m in memberships {
        match havn_db::repo::roles::find_by_id(db, m.role_id).await {
            Ok(Some(role)) => {
                merged = Some(match merged {
                    Some(acc) => acc.widen_with(&role.policy),
                    None => role.policy,
                });
            }
            Ok(None) => {
                warn!(%user_id, role_id = %m.role_id, "membership references a missing role")
            }
            Err(e) => warn!(%user_id, role_id = %m.role_id, error = %e, "role lookup failed"),
        }
    }
    let Some(mut policy) = merged else {
        warn!(%user_id, "no role policy could be loaded; using the most restrictive policy");
        return restrictive_fallback();
    };
    if let Some(cap) = env_cap {
        policy.max_agents = policy.max_agents.min(cap);
    }
    policy
}

fn restrictive_fallback() -> Policy {
    let mut p = Policy::default();
    p.max_agents = 0;
    p.permissions.can_install_skills = false;
    p.permissions.can_access_network = false;
    p.permissions.can_use_shell = false;
    p.permissions.can_schedule_cron = false;
    p
}

/// Per-agent override only — pure / sync. Returns `None` when the
/// agent has no `config.policy` field or when it is malformed (logged;
/// the chain then falls through to the owner's policy). `PATCH
/// /agents/{id}` rejects a malformed override, so the latter only
/// happens for config written another way. Public for tests;
/// production code should use [`for_session`] which folds this into
/// the full chain.
pub fn override_for(agent: &Agent) -> Option<Policy> {
    let raw = agent.config.get("policy")?;
    match serde_json::from_value::<Policy>(raw.clone()) {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(
                agent_id = %agent.id,
                error = %e,
                "agent.config.policy is malformed; falling through to user/team chain"
            );
            None
        }
    }
}

/// Full per-session resolution chain (spec §6.3): the owner's policy,
/// with the per-agent override applied on top (clamped to the owner's
/// policy when `multi_user`). In both modes the owner's hook rules are
/// kept, so an override can add hooks but not remove them.
///
/// This is what *both* the spawn-time path (cgroup limits) and the
/// Welcome handshake (runtime tool registry) consult, so the agent
/// process gets one consistent policy snapshot. Earlier drafts had
/// the spawner use `Policy::default()` while the runtime got the
/// override-only resolver — meaning a member with a team role
/// granting `memory_mb: 1024` saw the override in their tool
/// registry but their cgroup was still capped at the system default
/// 512 MiB. Drift fix.
pub async fn for_session(db: &SqlitePool, agent: &Agent, multi_user: bool) -> Policy {
    let owner = for_user(db, agent.owner_id).await;
    match override_for(agent) {
        Some(p) if multi_user => p.clamp_to(&owner),
        Some(mut p) => {
            p.hooks = p.hooks.union(&owner.hooks);
            p
        }
        None => owner,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use chrono::Utc;
    use havn_core::AgentId;
    use havn_db::repo::agents::AgentStatus;
    use serde_json::json;

    fn agent_with(config: serde_json::Value) -> Agent {
        Agent {
            id: AgentId::new(),
            owner_id: havn_core::UserId::new(),
            team_id: None,
            name: "test".into(),
            status: AgentStatus::Created,
            host: None,
            pid: None,
            config,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn missing_policy_key_yields_no_override() {
        // No `config.policy` field → None. The full chain (`for_session`)
        // would then fall through to `for_user`, but the override-only
        // helper returns None here.
        let p = override_for(&agent_with(json!({})));
        assert!(p.is_none());
    }

    #[test]
    fn explicit_policy_overrides_default() {
        let p = override_for(&agent_with(json!({
            "policy": {
                "permissions": {
                    "can_use_shell": false,
                    "can_access_network": false,
                }
            }
        })))
        .expect("override present");
        assert!(!p.permissions.can_use_shell);
        assert!(!p.permissions.can_access_network);
        // Unmentioned fields keep their defaults.
        assert!(p.permissions.can_install_skills);
    }

    #[test]
    fn malformed_policy_falls_through_to_chain() {
        // can_use_shell is supposed to be a bool — a string here is malformed.
        // Override returns None so for_session falls through to for_user.
        let p = override_for(&agent_with(json!({
            "policy": { "permissions": { "can_use_shell": "yes" } }
        })));
        assert!(
            p.is_none(),
            "malformed override should fall through, not silently apply"
        );
    }

    #[test]
    fn context_toolsets_round_trip() {
        let p = override_for(&agent_with(json!({
            "policy": {
                "context_toolsets": {
                    "cron": { "disabled": ["shell", "web_fetch"] }
                }
            }
        })))
        .expect("override present");
        let cron = p.context_toolsets.0.get("cron").expect("cron entry");
        assert_eq!(
            cron.disabled,
            vec!["shell".to_string(), "web_fetch".to_string()]
        );
    }

    fn widening_override() -> Agent {
        agent_with(json!({
            "policy": {
                "permissions": { "can_spawn_subagents": true, "can_use_mcp": true },
                "resource_limits": { "memory_mb": 4096, "pids_max": 4096 }
            }
        }))
    }

    #[tokio::test]
    async fn multi_user_override_cannot_exceed_owner_policy() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let p = for_session(&db, &widening_override(), true).await;
        assert!(!p.permissions.can_spawn_subagents);
        assert!(!p.permissions.can_use_mcp);
        assert_eq!(p.resource_limits.memory_mb, 512);
        assert_eq!(p.resource_limits.pids_max, 64);
    }

    async fn user_in_roles(db: &SqlitePool, roles: &[&str]) -> havn_core::UserId {
        use havn_db::repo::{roles as role_repo, team_memberships, teams, users};
        let user = users::create(db, users::NewUser { display_name: "m" })
            .await
            .expect("user");
        for (i, name) in roles.iter().enumerate() {
            let team = teams::create(
                db,
                teams::NewTeam {
                    name: &format!("team-{i}"),
                },
            )
            .await
            .expect("team");
            let role = role_repo::find_by_team_and_name(db, None, name)
                .await
                .expect("lookup")
                .expect("seeded role");
            team_memberships::add(db, user.id, team.id, role.id)
                .await
                .expect("membership");
        }
        user.id
    }

    #[tokio::test]
    async fn member_role_restrictions_apply() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let uid = user_in_roles(&db, &["member"]).await;
        let p = for_user(&db, uid).await;
        assert!(!p.permissions.can_use_shell);
        assert!(!p.permissions.can_spawn_subagents);
        assert_eq!(p.max_agents, 5);
        assert_eq!(p.budget.max_tokens_per_day, 1_000_000);
        assert_eq!(
            p.context_toolsets.0["cron"].disabled,
            vec!["shell".to_string()]
        );
    }

    #[tokio::test]
    async fn member_override_cannot_enable_shell_or_raise_limits() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let mut agent = agent_with(json!({
            "policy": {
                "permissions": { "can_use_shell": true },
                "resource_limits": { "memory_mb": 4096 },
                "budget": { "max_tokens_per_day": 0 }
            }
        }));
        agent.owner_id = user_in_roles(&db, &["member"]).await;
        let p = for_session(&db, &agent, true).await;
        assert!(!p.permissions.can_use_shell);
        assert_eq!(p.resource_limits.memory_mb, 512);
        assert_eq!(p.budget.max_tokens_per_day, 1_000_000);
    }

    #[tokio::test]
    async fn multiple_roles_combine_to_the_most_permissive() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let uid = user_in_roles(&db, &["member", "admin"]).await;
        let p = for_user(&db, uid).await;
        assert!(p.permissions.can_use_shell);
        assert_eq!(p.resource_limits.memory_mb, 1024);
        assert_eq!(p.budget.max_tokens_per_day, 0);
        assert!(
            p.context_toolsets
                .0
                .get("cron")
                .is_none_or(|c| c.disabled.is_empty())
        );
    }

    #[tokio::test]
    async fn override_mcp_servers_need_a_role_grant() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let mut agent = agent_with(json!({
            "policy": {
                "permissions": { "can_use_mcp": true },
                "mcp_servers": { "fs": { "binary": "fs", "extra_paths_rw": ["/"] } }
            }
        }));
        agent.owner_id = user_in_roles(&db, &["admin"]).await;
        let p = for_session(&db, &agent, true).await;
        assert!(!p.permissions.can_use_mcp);
        assert!(p.mcp_servers.is_empty());
    }

    async fn user_in_role_denying_echo(db: &SqlitePool) -> havn_core::UserId {
        use havn_db::repo::{roles as role_repo, team_memberships, teams, users};
        let user = users::create(db, users::NewUser { display_name: "m" })
            .await
            .expect("user");
        let team = teams::create(db, teams::NewTeam { name: "hooks" })
            .await
            .expect("team");
        let mut policy = Policy::default();
        policy.hooks.pre_tool_use.push(havn_core::ToolHookRule {
            tool: "shell".into(),
            deny: vec!["^echo".into()],
        });
        let role = role_repo::create(
            db,
            role_repo::NewRole {
                team_id: Some(team.id),
                name: "guarded",
                policy: &policy,
            },
        )
        .await
        .expect("role");
        team_memberships::add(db, user.id, team.id, role.id)
            .await
            .expect("membership");
        user.id
    }

    fn denies_echo(p: &Policy) -> bool {
        p.hooks
            .pre_tool_use
            .iter()
            .any(|r| r.tool == "shell" && r.deny.iter().any(|d| d == "^echo"))
    }

    #[tokio::test]
    async fn override_cannot_remove_role_hooks() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let owner = user_in_role_denying_echo(&db).await;
        for config in [
            json!({ "policy": { "hooks": { "pre_tool_use": [] } } }),
            json!({ "policy": { "permissions": { "can_use_shell": "yes" } } }),
        ] {
            let mut agent = agent_with(config.clone());
            agent.owner_id = owner;
            for multi_user in [true, false] {
                let p = for_session(&db, &agent, multi_user).await;
                assert!(denies_echo(&p), "{config} multi_user={multi_user}");
            }
        }
    }

    #[tokio::test]
    async fn single_user_override_applies_as_written() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let p = for_session(&db, &widening_override(), false).await;
        assert!(p.permissions.can_spawn_subagents);
        assert!(p.permissions.can_use_mcp);
        assert_eq!(p.resource_limits.memory_mb, 4096);
    }
}
