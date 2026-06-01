//! Roles repository (spec §5.1, §6.4).
//!
//! A role is a name + a [`havn_core::Policy`] JSON blob, optionally
//! scoped to a team. `team_id IS NULL` marks the system-wide built-ins
//! (`admin` + `member`, seeded by migration 0004).
//!
//! Policy storage choice: TEXT column with serde-json round-trip rather
//! than a normalised schema. The Policy struct is wide and changes shape
//! across versions; storing it as JSON lets us evolve fields without a
//! migration per revision. The downside — no SQL-level filtering — is
//! fine because every read-path here returns the parsed Policy and the
//! gateway filters in Rust.

use chrono::{DateTime, Utc};
use havn_core::{Policy, RoleId, TeamId};
use sqlx::SqlitePool;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone)]
pub struct Role {
    pub id: RoleId,
    /// `None` for system-wide roles (the migration 0004 seeds).
    pub team_id: Option<TeamId>,
    pub name: String,
    pub policy: Policy,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewRole<'a> {
    pub team_id: Option<TeamId>,
    pub name: &'a str,
    pub policy: &'a Policy,
}

pub async fn create(pool: &SqlitePool, new: NewRole<'_>) -> Result<Role> {
    let id = RoleId::new();
    let json = serde_json::to_string(new.policy).map_err(|e| DbError::InvalidValue {
        column: "roles.policy",
        message: e.to_string(),
    })?;
    sqlx::query("INSERT INTO roles (id, team_id, name, policy) VALUES (?1, ?2, ?3, ?4)")
        .bind(id.to_string())
        .bind(new.team_id.map(|t| t.to_string()))
        .bind(new.name)
        .bind(json)
        .execute(pool)
        .await?;
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn find_by_id(pool: &SqlitePool, id: RoleId) -> Result<Option<Role>> {
    let row: Option<RoleRow> = sqlx::query_as::<_, RoleRow>(
        "SELECT id, team_id, name, policy, created_at FROM roles WHERE id = ?1",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(Role::try_from).transpose()
}

/// Resolve a role by team + name. The two main lookups: per-team
/// `(team, "admin")`, and the system-wide `(NULL, "admin")` /
/// `(NULL, "member")` fallbacks.
pub async fn find_by_team_and_name(
    pool: &SqlitePool,
    team_id: Option<TeamId>,
    name: &str,
) -> Result<Option<Role>> {
    let row: Option<RoleRow> = match team_id {
        Some(tid) => {
            sqlx::query_as::<_, RoleRow>(
                "SELECT id, team_id, name, policy, created_at \
             FROM roles WHERE team_id = ?1 AND name = ?2",
            )
            .bind(tid.to_string())
            .bind(name)
            .fetch_optional(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, RoleRow>(
                "SELECT id, team_id, name, policy, created_at \
             FROM roles WHERE team_id IS NULL AND name = ?1",
            )
            .bind(name)
            .fetch_optional(pool)
            .await?
        }
    };
    row.map(Role::try_from).transpose()
}

/// All roles for one team (the team admins' "role manager" view).
/// Ordered by name for stable rendering.
pub async fn list_for_team(pool: &SqlitePool, team_id: TeamId) -> Result<Vec<Role>> {
    let rows: Vec<RoleRow> = sqlx::query_as::<_, RoleRow>(
        "SELECT id, team_id, name, policy, created_at \
         FROM roles WHERE team_id = ?1 ORDER BY name",
    )
    .bind(team_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(Role::try_from).collect()
}

/// Replace the policy JSON for an existing role. Returns the updated
/// row; `NotFound` when the id doesn't match. Used by both the REST
/// endpoint and the `havn role set-policy` CLI.
///
/// Refuses to operate on system-wide roles (`team_id IS NULL`) — those
/// are seed defaults; mutate them and every team that hasn't carved its
/// own policy starts behaving differently. The CLI checks the id-shape
/// first, but the SQL guard is the real defence.
pub async fn set_policy(pool: &SqlitePool, id: RoleId, policy: &Policy) -> Result<Role> {
    let json = serde_json::to_string(policy).map_err(|e| DbError::InvalidValue {
        column: "roles.policy",
        message: e.to_string(),
    })?;
    let res = sqlx::query("UPDATE roles SET policy = ?1 WHERE id = ?2 AND team_id IS NOT NULL")
        .bind(json)
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        // Either the id doesn't exist or it points at a system role;
        // either way, the caller should hear "not found" — leaking
        // "this is a sealed system role" doesn't help operators.
        return Err(DbError::NotFound);
    }
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

/// Hard-delete a (team-scoped) role. Refuses if any membership still
/// references it (FK is `ON DELETE RESTRICT` so the DB enforces this
/// — caller catches the surfaced sqlx error and surfaces a friendlier
/// message). System-wide roles also can't be deleted.
pub async fn delete(pool: &SqlitePool, id: RoleId) -> Result<()> {
    let res = sqlx::query("DELETE FROM roles WHERE id = ?1 AND team_id IS NOT NULL")
        .bind(id.to_string())
        .execute(pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.message().contains("FOREIGN KEY") => {
                // Members still hold this role — caller surfaces.
                DbError::Conflict("roles.in_use_by_membership")
            }
            _ => DbError::from(e),
        })?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct RoleRow {
    id: String,
    team_id: Option<String>,
    name: String,
    policy: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<RoleRow> for Role {
    type Error = DbError;
    fn try_from(r: RoleRow) -> Result<Self> {
        let policy: Policy =
            serde_json::from_str(&r.policy).map_err(|e| DbError::InvalidValue {
                column: "roles.policy",
                message: e.to_string(),
            })?;
        Ok(Self {
            id: RoleId::from_uuid(parse_db_uuid(&r.id, "roles.id")?),
            team_id: r
                .team_id
                .as_deref()
                .map(|s| parse_db_uuid(s, "roles.team_id").map(TeamId::from_uuid))
                .transpose()?,
            name: r.name,
            policy,
            created_at: r.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::teams::{NewTeam, create as create_team};

    #[tokio::test]
    async fn system_seeds_present_after_migration() {
        let pool = connect_in_memory().await.expect("db");
        let admin = find_by_team_and_name(&pool, None, "admin")
            .await
            .expect("query")
            .expect("admin seed");
        assert!(admin.team_id.is_none());
        assert!(admin.policy.permissions.can_use_shell);

        let member = find_by_team_and_name(&pool, None, "member")
            .await
            .expect("query")
            .expect("member seed");
        assert!(!member.policy.permissions.can_use_shell);
    }

    #[tokio::test]
    async fn create_team_role_round_trip() {
        let pool = connect_in_memory().await.expect("db");
        let team = create_team(&pool, NewTeam { name: "t" })
            .await
            .expect("team");
        let mut policy = Policy::default();
        policy.permissions.can_spawn_subagents = true;
        let r = create(
            &pool,
            NewRole {
                team_id: Some(team.id),
                name: "lead",
                policy: &policy,
            },
        )
        .await
        .expect("create");
        let found = find_by_id(&pool, r.id).await.expect("find").expect("some");
        assert_eq!(found.name, "lead");
        assert_eq!(found.team_id, Some(team.id));
        assert!(found.policy.permissions.can_spawn_subagents);
    }

    #[tokio::test]
    async fn set_policy_refuses_system_role() {
        let pool = connect_in_memory().await.expect("db");
        let admin = find_by_team_and_name(&pool, None, "admin")
            .await
            .expect("query")
            .expect("seed");
        let mut policy = admin.policy.clone();
        policy.permissions.can_use_shell = false;
        let result = set_policy(&pool, admin.id, &policy).await;
        assert!(matches!(result, Err(DbError::NotFound)));
    }

    #[tokio::test]
    async fn list_for_team_filters_correctly() {
        let pool = connect_in_memory().await.expect("db");
        let t1 = create_team(&pool, NewTeam { name: "t1" })
            .await
            .expect("t1");
        let t2 = create_team(&pool, NewTeam { name: "t2" })
            .await
            .expect("t2");
        let policy = Policy::default();
        for n in ["a", "b"] {
            create(
                &pool,
                NewRole {
                    team_id: Some(t1.id),
                    name: n,
                    policy: &policy,
                },
            )
            .await
            .expect("create");
        }
        create(
            &pool,
            NewRole {
                team_id: Some(t2.id),
                name: "c",
                policy: &policy,
            },
        )
        .await
        .expect("create");

        let t1_roles = list_for_team(&pool, t1.id).await.expect("list");
        assert_eq!(t1_roles.len(), 2);
        assert!(t1_roles.iter().all(|r| r.team_id == Some(t1.id)));
    }
}
