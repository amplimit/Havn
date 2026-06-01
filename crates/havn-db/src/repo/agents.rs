//! Agents repository.

use chrono::{DateTime, Utc};
use havn_core::{AgentId, TeamId, UserId};
use sqlx::SqlitePool;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Created,
    Running,
    Paused,
    Stopped,
    Error,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "created" => Self::Created,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "stopped" => Self::Stopped,
            "error" => Self::Error,
            other => {
                return Err(DbError::InvalidValue {
                    column: "agents.status",
                    message: format!("unknown status {other:?}"),
                });
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub owner_id: UserId,
    pub team_id: Option<TeamId>,
    pub name: String,
    pub status: AgentStatus,
    pub host: Option<String>,
    pub pid: Option<i64>,
    pub config: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewAgent<'a> {
    pub owner_id: UserId,
    pub team_id: Option<TeamId>,
    pub name: &'a str,
    pub config: serde_json::Value,
}

pub async fn create(pool: &SqlitePool, new: NewAgent<'_>) -> Result<Agent> {
    let id = AgentId::new();
    let config = serde_json::to_string(&new.config).map_err(|e| DbError::InvalidValue {
        column: "agents.config",
        message: e.to_string(),
    })?;
    sqlx::query(
        "INSERT INTO agents (id, owner_id, team_id, name, config) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id.to_string())
    .bind(new.owner_id.to_string())
    .bind(new.team_id.map(|t| t.to_string()))
    .bind(new.name)
    .bind(config)
    .execute(pool)
    .await
    .map_err(map_unique("agents.(owner_id,name)"))?;

    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn find_by_id(pool: &SqlitePool, id: AgentId) -> Result<Option<Agent>> {
    let row: Option<AgentRow> = sqlx::query_as::<_, AgentRow>(
        "SELECT id, owner_id, team_id, name, status, host, pid, config, created_at, updated_at \
         FROM agents WHERE id = ?1",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;

    row.map(Agent::try_from).transpose()
}

/// Number of agents owned by `owner_id`. Used by the policy gate that
/// enforces `Policy::max_agents` (spec §6.3) without paying the cost of
/// loading every row.
pub async fn count_for_owner(pool: &SqlitePool, owner_id: UserId) -> Result<u32> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE owner_id = ?1")
        .bind(owner_id.to_string())
        .fetch_one(pool)
        .await?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

pub async fn list_for_owner(pool: &SqlitePool, owner_id: UserId) -> Result<Vec<Agent>> {
    let rows: Vec<AgentRow> = sqlx::query_as::<_, AgentRow>(
        "SELECT id, owner_id, team_id, name, status, host, pid, config, created_at, updated_at \
         FROM agents WHERE owner_id = ?1 ORDER BY created_at",
    )
    .bind(owner_id.to_string())
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(Agent::try_from).collect()
}

pub async fn set_status(pool: &SqlitePool, id: AgentId, status: AgentStatus) -> Result<()> {
    let result = sqlx::query(
        "UPDATE agents SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE id = ?2",
    )
    .bind(status.as_str())
    .bind(id.to_string())
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Patch the mutable fields of an agent: `name` and/or `config`.
/// Either / both can be `None` for a no-op on that field. `config`
/// is replaced wholesale (caller is responsible for merging) — it's
/// a single JSON blob and atomic replace is the safest semantic.
/// Updates `updated_at`. Returns the post-update row.
pub async fn patch(
    pool: &SqlitePool,
    id: AgentId,
    name: Option<&str>,
    config: Option<&serde_json::Value>,
) -> Result<Agent> {
    let config_str =
        config
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| DbError::InvalidValue {
                column: "agents.config",
                message: e.to_string(),
            })?;
    let res = sqlx::query(
        "UPDATE agents \
         SET name       = COALESCE(?1, name), \
             config     = COALESCE(?2, config), \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE id = ?3",
    )
    .bind(name)
    .bind(config_str)
    .bind(id.to_string())
    .execute(pool)
    .await
    .map_err(map_unique("agents.(owner_id,name)"))?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn delete(pool: &SqlitePool, id: AgentId) -> Result<()> {
    let result = sqlx::query("DELETE FROM agents WHERE id = ?1")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct AgentRow {
    id: String,
    owner_id: String,
    team_id: Option<String>,
    name: String,
    status: String,
    host: Option<String>,
    pid: Option<i64>,
    config: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<AgentRow> for Agent {
    type Error = DbError;
    fn try_from(r: AgentRow) -> Result<Self> {
        let config = serde_json::from_str(&r.config).map_err(|e| DbError::InvalidValue {
            column: "agents.config",
            message: e.to_string(),
        })?;
        Ok(Self {
            id: AgentId::from_uuid(parse_db_uuid(&r.id, "agents.id")?),
            owner_id: UserId::from_uuid(parse_db_uuid(&r.owner_id, "agents.owner_id")?),
            team_id: r
                .team_id
                .as_deref()
                .map(|s| parse_db_uuid(s, "agents.team_id").map(TeamId::from_uuid))
                .transpose()?,
            name: r.name,
            status: AgentStatus::parse(&r.status)?,
            host: r.host,
            pid: r.pid,
            config,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

fn map_unique(column: &'static str) -> impl FnOnce(sqlx::Error) -> DbError {
    move |e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::Conflict(column),
        _ => DbError::from(e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::users::{NewUser, create as create_user};

    async fn make_user(pool: &SqlitePool, _disambiguator: &str) -> UserId {
        // `_disambiguator` was the email in the old shape; kept as a
        // parameter for call-site compatibility but no longer used —
        // every test create_user gets a fresh UUID.
        create_user(
            pool,
            NewUser {
                display_name: "test",
            },
        )
        .await
        .expect("create user")
        .id
    }

    #[tokio::test]
    async fn create_and_find_round_trip() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = make_user(&pool, "u1@e").await;
        let agent = create(
            &pool,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({"model": "claude-sonnet-4-6"}),
            },
        )
        .await
        .expect("create agent");
        assert_eq!(agent.status, AgentStatus::Created);

        let found = find_by_id(&pool, agent.id)
            .await
            .expect("find")
            .expect("some");
        assert_eq!(found.id, agent.id);
        assert_eq!(found.config["model"], "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn list_for_owner_only_returns_owned() {
        let pool = connect_in_memory().await.expect("connect");
        let alice = make_user(&pool, "alice@e").await;
        let bob = make_user(&pool, "bob@e").await;
        for n in ["a", "b", "c"] {
            create(
                &pool,
                NewAgent {
                    owner_id: alice,
                    team_id: None,
                    name: n,
                    config: serde_json::json!({}),
                },
            )
            .await
            .expect("create");
        }
        create(
            &pool,
            NewAgent {
                owner_id: bob,
                team_id: None,
                name: "z",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("create");

        let alice_agents = list_for_owner(&pool, alice).await.expect("list");
        assert_eq!(alice_agents.len(), 3);
        assert!(alice_agents.iter().all(|a| a.owner_id == alice));
    }

    #[tokio::test]
    async fn set_status_updates() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = make_user(&pool, "u@e").await;
        let agent = create(
            &pool,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("create");

        set_status(&pool, agent.id, AgentStatus::Running)
            .await
            .expect("set status");
        let after = find_by_id(&pool, agent.id)
            .await
            .expect("find")
            .expect("some");
        assert_eq!(after.status, AgentStatus::Running);
    }

    #[tokio::test]
    async fn duplicate_name_for_same_owner_conflicts() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = make_user(&pool, "u@e").await;
        create(
            &pool,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("first");
        let dup = create(
            &pool,
            NewAgent {
                owner_id: owner,
                team_id: None,
                name: "alpha",
                config: serde_json::json!({}),
            },
        )
        .await;
        assert!(matches!(dup, Err(DbError::Conflict(_))));
    }
}
