//! Per-agent policy resolution (spec §6).
//!
//! Per-agent override chain:
//!
//! ```text
//! 1. Agent.config["policy"]   ← per-agent override stored as JSON
//! 2. Policy::default()        ← system default (havn-core)
//! ```
//!
//! `for_user` (below) walks team memberships → roles to derive the
//! user-level policy used for `max_agents`, `can_schedule_cron`, etc.
//! Future: merge the agent override with the resolved team-role policy
//! per spec §6.3 (per-agent override → team role → system default).
//! Today the agent-side chain stops at the override; team policy
//! affects the *user* surface only.
//!
//! Resolution runs once per agent connection — during the gateway-side
//! `Welcome` handshake — and the resulting snapshot is shipped to the
//! runtime in the `Welcome` frame. Mid-session policy edits do NOT affect
//! the running session (matching spec §9.4 frozen-prompt invariant for the
//! related case); the next session picks up the new value.

use havn_core::{Policy, UserId};
use havn_db::repo::agents::Agent;
use sqlx::SqlitePool;
use tracing::warn;

/// Resolve the effective policy for `agent`.
///
/// Never fails: a malformed `config.policy` value falls back to
/// `Policy::default()` with a warn log so misconfiguration tightens
/// privileges rather than expanding them. (Returning an error here would
/// either reject the agent connection — bad for ux — or force every
/// caller to re-implement the same fallback.)
/// Resolve the effective policy for a user (independent of any one
/// agent — used to enforce `max_agents`, `can_bind_channels`,
/// `can_schedule_cron`, etc. *before* an agent is created or a
/// channel/cron is added).
///
/// Phase 2 (now): returns `Policy::default()` with `max_agents` lifted
/// to a sane single-user cap (50). The Policy struct's own default of 1
/// is the conservative spec default for an *unauthorized* user; in
/// single-user mode the gateway's bootstrap user gets a more permissive
/// baseline so the install→first-chat SLO doesn't trip on the second
/// agent.
///
/// Walks `team_memberships → roles → policy` for `user_id`. When the
/// user belongs to one or more teams, returns the most-permissive of
/// their team-role policies (max of `max_agents`, OR of capability
/// flags). When the user belongs to no team, returns the single-user
/// default — `Policy::default()` widened with `HAVN_MAX_AGENTS_PER_USER`.
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

    // Walk teams + roles. Skip silently on DB errors — the safe fall-back
    // is the conservative single-user default rather than blocking the
    // user from creating an agent because of a transient lookup failure.
    let memberships = match havn_db::repo::team_memberships::list_for_user(db, user_id).await {
        Ok(m) => m,
        Err(e) => {
            warn!(%user_id, error = %e, "team membership lookup failed; using single-user default");
            return base;
        }
    };
    if memberships.is_empty() {
        return base;
    }
    for m in memberships {
        if let Ok(Some(role)) = havn_db::repo::roles::find_by_id(db, m.role_id).await {
            base.max_agents = base.max_agents.max(role.policy.max_agents);
            base.permissions.can_install_skills |= role.policy.permissions.can_install_skills;
            base.permissions.can_access_network |= role.policy.permissions.can_access_network;
            base.permissions.can_use_shell |= role.policy.permissions.can_use_shell;
            base.permissions.can_spawn_subagents |= role.policy.permissions.can_spawn_subagents;
            base.permissions.can_schedule_cron |= role.policy.permissions.can_schedule_cron;
        }
    }
    base
}

/// Per-agent override only — pure / sync. Returns `None` when the
/// agent has no `config.policy` field, or `Some(default)` when the
/// override JSON is malformed (we tighten on misconfig). Public for
/// tests; production code should use [`for_session`] which folds
/// this into the full chain.
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

/// Full per-session resolution chain (spec §6.3): per-agent override
/// wins, falling through to the owner's user/team-merged policy, then
/// the system default.
///
/// This is what *both* the spawn-time path (cgroup limits) and the
/// Welcome handshake (runtime tool registry) consult, so the agent
/// process gets one consistent policy snapshot. Earlier drafts had
/// the spawner use `Policy::default()` while the runtime got the
/// override-only resolver — meaning a member with a team role
/// granting `memory_mb: 1024` saw the override in their tool
/// registry but their cgroup was still capped at the system default
/// 512 MiB. Drift fix.
pub async fn for_session(db: &SqlitePool, agent: &Agent) -> Policy {
    if let Some(p) = override_for(agent) {
        return p;
    }
    for_user(db, agent.owner_id).await
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
}
