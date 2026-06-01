//! RBAC primitives for team-scoped endpoints (spec §6, §10.3).
//!
//! Two questions every team-aware handler asks:
//! 1. Is this user a member of this team at all?
//! 2. Is this user an admin of this team — i.e. allowed to mutate
//!    membership / roles / team credentials?
//!
//! Both are one indexed SELECT against `team_memberships` joined to
//! `roles`. The helpers cache nothing; the gateway's per-request
//! lifetime is short enough that the freshness wins outweigh the
//! lookup cost. (Promote a single user to admin via the role manager
//! and the next request sees it without restart.)
//!
//! Note: "admin of team" is distinct from `policy.admin_visibility`.
//! The latter is the *runtime* policy a member sees; this helper just
//! gates the management-API routes that mutate team state. A team-admin
//! user always has the wide system-level admin policy regardless of
//! what their per-team role policy says — the team-admin role is the
//! one with `name = 'admin'` in `roles`, by convention seeded by
//! [`crate::policy_resolver::for_user`].

use havn_core::{TeamId, UserId};
use havn_db::repo::team_memberships;
use sqlx::SqlitePool;

use crate::api::ApiError;

/// Caller is a member of `team_id`. `Forbidden` otherwise — we do
/// **not** return 404 here even for teams that exist (information
/// leak): a non-member trying to read a team's roster should know it
/// exists if they can guess the id. The dashboard never shows team ids
/// to non-members anyway, so this comes up rarely; when it does, the
/// 403 is a more honest answer than a 404.
pub async fn require_member(
    db: &SqlitePool,
    user_id: UserId,
    team_id: TeamId,
) -> Result<(), ApiError> {
    if team_memberships::is_member(db, user_id, team_id).await? {
        Ok(())
    } else {
        Err(ApiError::Forbidden(format!(
            "user {user_id} is not a member of team {team_id}"
        )))
    }
}

/// Caller is a member of `team_id` AND holds the `admin` role.
/// Mutating endpoints (members, roles, team credentials, deleting the
/// team) gate on this. Audit log read also gates here unless the
/// member's role policy explicitly grants `can_view_audit_log`.
pub async fn require_admin(
    db: &SqlitePool,
    user_id: UserId,
    team_id: TeamId,
) -> Result<(), ApiError> {
    if team_memberships::is_admin(db, user_id, team_id).await? {
        Ok(())
    } else {
        Err(ApiError::Forbidden(format!(
            "user {user_id} is not an admin of team {team_id}"
        )))
    }
}

/// Boolean variant for view assembly — handlers that want to *render*
/// admin-only fields conditionally (e.g. a "delete" button) check
/// this instead of returning errors.
pub async fn is_admin(db: &SqlitePool, user_id: UserId, team_id: TeamId) -> Result<bool, ApiError> {
    Ok(team_memberships::is_admin(db, user_id, team_id).await?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use havn_db::connect_in_memory;
    use havn_db::repo::roles::{NewRole, create as create_role};
    use havn_db::repo::teams::{NewTeam, create as create_team};
    use havn_db::repo::users::{NewUser, create as create_user};

    async fn fixture() -> (SqlitePool, UserId, UserId, TeamId) {
        let pool = connect_in_memory().await.expect("db");
        let admin = create_user(&pool, NewUser { display_name: "a" })
            .await
            .expect("user");
        let outsider = create_user(&pool, NewUser { display_name: "o" })
            .await
            .expect("user");
        let team = create_team(&pool, NewTeam { name: "t" })
            .await
            .expect("team");
        let admin_role = create_role(
            &pool,
            NewRole {
                team_id: Some(team.id),
                name: "admin",
                policy: &Policy::default(),
            },
        )
        .await
        .expect("role");
        team_memberships::add(&pool, admin.id, team.id, admin_role.id)
            .await
            .expect("add");
        (pool, admin.id, outsider.id, team.id)
    }

    #[tokio::test]
    async fn admin_passes_both_checks() {
        let (pool, admin, _outsider, team) = fixture().await;
        require_member(&pool, admin, team).await.expect("member");
        require_admin(&pool, admin, team).await.expect("admin");
        assert!(is_admin(&pool, admin, team).await.expect("query"));
    }

    #[tokio::test]
    async fn outsider_fails_member_check() {
        let (pool, _admin, outsider, team) = fixture().await;
        let r = require_member(&pool, outsider, team).await;
        assert!(matches!(r, Err(ApiError::Forbidden(_))));
        assert!(!is_admin(&pool, outsider, team).await.expect("query"));
    }

    #[tokio::test]
    async fn member_only_fails_admin_check() {
        let (pool, _admin, outsider, team) = fixture().await;
        // Add outsider as a non-admin member.
        let member_role = create_role(
            &pool,
            NewRole {
                team_id: Some(team),
                name: "member",
                policy: &Policy::default(),
            },
        )
        .await
        .expect("role");
        team_memberships::add(&pool, outsider, team, member_role.id)
            .await
            .expect("add");

        require_member(&pool, outsider, team)
            .await
            .expect("member ok");
        let r = require_admin(&pool, outsider, team).await;
        assert!(matches!(r, Err(ApiError::Forbidden(_))));
    }
}
