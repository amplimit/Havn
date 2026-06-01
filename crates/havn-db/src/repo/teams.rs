//! Teams repository (spec §5.1).
//!
//! A `team` groups users for shared agents, shared credentials, and a
//! single audit log. Membership is mediated by [`super::team_memberships`];
//! per-team policy lives in [`super::roles`]. Both repos cascade DELETE
//! through here so dropping a team cleans up its memberships and roles
//! without orphaned rows.

use chrono::{DateTime, Utc};
use havn_core::TeamId;
use sqlx::SqlitePool;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone)]
pub struct Team {
    pub id: TeamId,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewTeam<'a> {
    pub name: &'a str,
}

pub async fn create(pool: &SqlitePool, new: NewTeam<'_>) -> Result<Team> {
    let id = TeamId::new();
    sqlx::query("INSERT INTO teams (id, name) VALUES (?1, ?2)")
        .bind(id.to_string())
        .bind(new.name)
        .execute(pool)
        .await?;
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn find_by_id(pool: &SqlitePool, id: TeamId) -> Result<Option<Team>> {
    let row: Option<TeamRow> =
        sqlx::query_as::<_, TeamRow>("SELECT id, name, created_at FROM teams WHERE id = ?1")
            .bind(id.to_string())
            .fetch_optional(pool)
            .await?;
    row.map(Team::try_from).transpose()
}

/// Every team in the system, oldest first. Used by the admin dashboard
/// landing page; small enough that pagination isn't worth it.
pub async fn list_all(pool: &SqlitePool) -> Result<Vec<Team>> {
    let rows: Vec<TeamRow> =
        sqlx::query_as::<_, TeamRow>("SELECT id, name, created_at FROM teams ORDER BY created_at")
            .fetch_all(pool)
            .await?;
    rows.into_iter().map(Team::try_from).collect()
}

/// Rename a team. Idempotent if the new name matches the current.
pub async fn rename(pool: &SqlitePool, id: TeamId, name: &str) -> Result<Team> {
    let res = sqlx::query("UPDATE teams SET name = ?1 WHERE id = ?2")
        .bind(name)
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

/// Hard-delete. Cascades through `roles`, `team_memberships`, and sets
/// `agents.team_id`/`audit_log.team_id`/`credentials.scope_id` to a
/// dangling reference (FK is loose for credentials by design — scope_id
/// is a generic TEXT). Caller should warn the user before invoking.
pub async fn delete(pool: &SqlitePool, id: TeamId) -> Result<()> {
    let res = sqlx::query("DELETE FROM teams WHERE id = ?1")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct TeamRow {
    id: String,
    name: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<TeamRow> for Team {
    type Error = DbError;
    fn try_from(r: TeamRow) -> Result<Self> {
        Ok(Self {
            id: TeamId::from_uuid(parse_db_uuid(&r.id, "teams.id")?),
            name: r.name,
            created_at: r.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;

    #[tokio::test]
    async fn create_then_round_trip() {
        let pool = connect_in_memory().await.expect("db");
        let t = create(
            &pool,
            NewTeam {
                name: "engineering",
            },
        )
        .await
        .expect("create");
        let by_id = find_by_id(&pool, t.id).await.expect("find").expect("some");
        assert_eq!(by_id.name, "engineering");
        assert_eq!(by_id.id, t.id);
    }

    #[tokio::test]
    async fn list_all_returns_oldest_first() {
        let pool = connect_in_memory().await.expect("db");
        for n in ["a", "b", "c"] {
            create(&pool, NewTeam { name: n }).await.expect("create");
        }
        let teams = list_all(&pool).await.expect("list");
        assert_eq!(teams.len(), 3);
        assert_eq!(teams[0].name, "a");
        assert_eq!(teams[2].name, "c");
    }

    #[tokio::test]
    async fn rename_updates_name() {
        let pool = connect_in_memory().await.expect("db");
        let t = create(&pool, NewTeam { name: "old" })
            .await
            .expect("create");
        let renamed = rename(&pool, t.id, "new").await.expect("rename");
        assert_eq!(renamed.name, "new");
    }

    #[tokio::test]
    async fn delete_then_find_returns_none() {
        let pool = connect_in_memory().await.expect("db");
        let t = create(&pool, NewTeam { name: "doomed" })
            .await
            .expect("create");
        delete(&pool, t.id).await.expect("delete");
        assert!(find_by_id(&pool, t.id).await.expect("find").is_none());
    }
}
