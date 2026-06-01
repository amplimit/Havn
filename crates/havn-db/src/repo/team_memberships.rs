//! Team memberships repository (spec §5.1).
//!
//! `(user_id, team_id)` is the primary key — one role per user per team.
//! Adding the same user a second time is a conflict, not a silent
//! upgrade; callers wanting to change a user's role within a team should
//! call [`set_role`].
//!
//! All FKs are `ON DELETE CASCADE` for `users` / `teams` so dropping
//! either end cleans up here. The `role_id` FK is `ON DELETE RESTRICT`
//! (spec §5.1) — operators must move members to another role before
//! deleting the role they're using.

use chrono::{DateTime, Utc};
use havn_core::{RoleId, TeamId, UserId};
use sqlx::SqlitePool;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone)]
pub struct Membership {
    pub user_id: UserId,
    pub team_id: TeamId,
    pub role_id: RoleId,
    pub joined_at: DateTime<Utc>,
}

/// Joined-by-the-DB convenience for the dashboard "members of team T"
/// view: the membership row + its role's name + the user's display name.
/// One DB round-trip beats N+1 lookups.
#[derive(Debug, Clone)]
pub struct MembershipDetail {
    pub user_id: UserId,
    pub user_display_name: String,
    pub role_id: RoleId,
    pub role_name: String,
    pub joined_at: DateTime<Utc>,
}

/// And its mirror for the "teams I'm in" view (per-user dashboard).
#[derive(Debug, Clone)]
pub struct UserTeamSummary {
    pub team_id: TeamId,
    pub team_name: String,
    pub role_id: RoleId,
    pub role_name: String,
}

pub async fn add(
    pool: &SqlitePool,
    user_id: UserId,
    team_id: TeamId,
    role_id: RoleId,
) -> Result<Membership> {
    sqlx::query("INSERT INTO team_memberships (user_id, team_id, role_id) VALUES (?1, ?2, ?3)")
        .bind(user_id.to_string())
        .bind(team_id.to_string())
        .bind(role_id.to_string())
        .execute(pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                DbError::Conflict("team_memberships.(user_id,team_id)")
            }
            _ => DbError::from(e),
        })?;
    find(pool, user_id, team_id).await?.ok_or(DbError::NotFound)
}

pub async fn find(
    pool: &SqlitePool,
    user_id: UserId,
    team_id: TeamId,
) -> Result<Option<Membership>> {
    let row: Option<MembershipRow> = sqlx::query_as::<_, MembershipRow>(
        "SELECT user_id, team_id, role_id, joined_at FROM team_memberships \
         WHERE user_id = ?1 AND team_id = ?2",
    )
    .bind(user_id.to_string())
    .bind(team_id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(Membership::try_from).transpose()
}

/// Update the role on an existing membership. Idempotent if the role
/// is already set to `role_id`.
pub async fn set_role(
    pool: &SqlitePool,
    user_id: UserId,
    team_id: TeamId,
    role_id: RoleId,
) -> Result<Membership> {
    let res = sqlx::query(
        "UPDATE team_memberships SET role_id = ?1 \
         WHERE user_id = ?2 AND team_id = ?3",
    )
    .bind(role_id.to_string())
    .bind(user_id.to_string())
    .bind(team_id.to_string())
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    find(pool, user_id, team_id).await?.ok_or(DbError::NotFound)
}

pub async fn remove(pool: &SqlitePool, user_id: UserId, team_id: TeamId) -> Result<()> {
    let res = sqlx::query("DELETE FROM team_memberships WHERE user_id = ?1 AND team_id = ?2")
        .bind(user_id.to_string())
        .bind(team_id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Members of `team_id`, joined to user.display_name + role.name.
/// Newest joiners last — the dashboard prefers chronological for
/// "who joined recently?".
pub async fn list_for_team(pool: &SqlitePool, team_id: TeamId) -> Result<Vec<MembershipDetail>> {
    let rows: Vec<MembershipDetailRow> = sqlx::query_as::<_, MembershipDetailRow>(
        "SELECT m.user_id, u.display_name AS user_display_name, \
                m.role_id, r.name AS role_name, \
                m.joined_at \
         FROM team_memberships m \
         JOIN users u ON u.id = m.user_id \
         JOIN roles r ON r.id = m.role_id \
         WHERE m.team_id = ?1 \
         ORDER BY m.joined_at",
    )
    .bind(team_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(MembershipDetail::try_from).collect()
}

/// Teams `user_id` belongs to + the role they hold in each. The
/// dashboard sidebar's "Teams" section iterates this.
pub async fn list_for_user(pool: &SqlitePool, user_id: UserId) -> Result<Vec<UserTeamSummary>> {
    let rows: Vec<UserTeamSummaryRow> = sqlx::query_as::<_, UserTeamSummaryRow>(
        "SELECT m.team_id, t.name AS team_name, \
                m.role_id, r.name AS role_name \
         FROM team_memberships m \
         JOIN teams t ON t.id = m.team_id \
         JOIN roles r ON r.id = m.role_id \
         WHERE m.user_id = ?1 \
         ORDER BY t.name",
    )
    .bind(user_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(UserTeamSummary::try_from).collect()
}

/// True iff `user` is a member of `team` AND that role's name is `"admin"`.
/// Cheap two-column join — one of the most-called RBAC checks. Returning
/// a bool lets call sites stay terse.
pub async fn is_admin(pool: &SqlitePool, user_id: UserId, team_id: TeamId) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_memberships m \
         JOIN roles r ON r.id = m.role_id \
         WHERE m.user_id = ?1 AND m.team_id = ?2 AND r.name = 'admin'",
    )
    .bind(user_id.to_string())
    .bind(team_id.to_string())
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// True iff `user` is a member of `team` regardless of role.
pub async fn is_member(pool: &SqlitePool, user_id: UserId, team_id: TeamId) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_memberships \
         WHERE user_id = ?1 AND team_id = ?2",
    )
    .bind(user_id.to_string())
    .bind(team_id.to_string())
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

#[derive(Debug, sqlx::FromRow)]
struct MembershipRow {
    user_id: String,
    team_id: String,
    role_id: String,
    joined_at: DateTime<Utc>,
}

impl TryFrom<MembershipRow> for Membership {
    type Error = DbError;
    fn try_from(r: MembershipRow) -> Result<Self> {
        Ok(Self {
            user_id: UserId::from_uuid(parse_db_uuid(&r.user_id, "team_memberships.user_id")?),
            team_id: TeamId::from_uuid(parse_db_uuid(&r.team_id, "team_memberships.team_id")?),
            role_id: RoleId::from_uuid(parse_db_uuid(&r.role_id, "team_memberships.role_id")?),
            joined_at: r.joined_at,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct MembershipDetailRow {
    user_id: String,
    user_display_name: String,
    role_id: String,
    role_name: String,
    joined_at: DateTime<Utc>,
}

impl TryFrom<MembershipDetailRow> for MembershipDetail {
    type Error = DbError;
    fn try_from(r: MembershipDetailRow) -> Result<Self> {
        Ok(Self {
            user_id: UserId::from_uuid(parse_db_uuid(&r.user_id, "team_memberships.user_id")?),
            user_display_name: r.user_display_name,
            role_id: RoleId::from_uuid(parse_db_uuid(&r.role_id, "team_memberships.role_id")?),
            role_name: r.role_name,
            joined_at: r.joined_at,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct UserTeamSummaryRow {
    team_id: String,
    team_name: String,
    role_id: String,
    role_name: String,
}

impl TryFrom<UserTeamSummaryRow> for UserTeamSummary {
    type Error = DbError;
    fn try_from(r: UserTeamSummaryRow) -> Result<Self> {
        Ok(Self {
            team_id: TeamId::from_uuid(parse_db_uuid(&r.team_id, "team_memberships.team_id")?),
            team_name: r.team_name,
            role_id: RoleId::from_uuid(parse_db_uuid(&r.role_id, "team_memberships.role_id")?),
            role_name: r.role_name,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::roles::{NewRole, create as create_role, find_by_team_and_name};
    use crate::repo::teams::{NewTeam, create as create_team};
    use crate::repo::users::{NewUser, create as create_user};
    use havn_core::Policy;

    async fn fixture() -> (SqlitePool, UserId, TeamId, RoleId, RoleId) {
        let pool = connect_in_memory().await.expect("db");
        let user = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user");
        let team = create_team(&pool, NewTeam { name: "t" })
            .await
            .expect("team");
        let policy = Policy::default();
        let admin_role = create_role(
            &pool,
            NewRole {
                team_id: Some(team.id),
                name: "admin",
                policy: &policy,
            },
        )
        .await
        .expect("admin role");
        let member_role = create_role(
            &pool,
            NewRole {
                team_id: Some(team.id),
                name: "member",
                policy: &policy,
            },
        )
        .await
        .expect("member role");
        (pool, user.id, team.id, admin_role.id, member_role.id)
    }

    #[tokio::test]
    async fn add_then_find_round_trip() {
        let (pool, user, team, admin, _) = fixture().await;
        add(&pool, user, team, admin).await.expect("add");
        let found = find(&pool, user, team).await.expect("find").expect("some");
        assert_eq!(found.role_id, admin);
    }

    #[tokio::test]
    async fn duplicate_add_conflicts() {
        let (pool, user, team, admin, _) = fixture().await;
        add(&pool, user, team, admin).await.expect("add");
        let again = add(&pool, user, team, admin).await;
        assert!(matches!(again, Err(DbError::Conflict(_))));
    }

    #[tokio::test]
    async fn set_role_changes_assignment() {
        let (pool, user, team, admin, member) = fixture().await;
        add(&pool, user, team, admin).await.expect("add");
        set_role(&pool, user, team, member).await.expect("change");
        let found = find(&pool, user, team).await.expect("find").expect("some");
        assert_eq!(found.role_id, member);
    }

    #[tokio::test]
    async fn is_admin_reflects_role() {
        let (pool, user, team, admin, member) = fixture().await;
        add(&pool, user, team, admin).await.expect("add");
        assert!(is_admin(&pool, user, team).await.expect("query"));

        set_role(&pool, user, team, member).await.expect("demote");
        assert!(!is_admin(&pool, user, team).await.expect("query"));
        assert!(is_member(&pool, user, team).await.expect("query"));
    }

    #[tokio::test]
    async fn list_for_team_returns_user_details() {
        let (pool, user, team, admin, _) = fixture().await;
        add(&pool, user, team, admin).await.expect("add");
        let rows = list_for_team(&pool, team).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_id, user);
        assert_eq!(rows[0].user_display_name, "u");
        assert_eq!(rows[0].role_name, "admin");
    }

    #[tokio::test]
    async fn list_for_user_uses_seeded_or_team_role() {
        let (pool, user, team, admin, _) = fixture().await;
        add(&pool, user, team, admin).await.expect("add");
        let rows = list_for_user(&pool, user).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].team_id, team);
        assert_eq!(rows[0].role_name, "admin");
    }

    #[tokio::test]
    async fn system_seed_lookup_works() {
        let pool = connect_in_memory().await.expect("db");
        let admin = find_by_team_and_name(&pool, None, "admin")
            .await
            .expect("query")
            .expect("seed");
        assert_eq!(admin.name, "admin");
    }
}
