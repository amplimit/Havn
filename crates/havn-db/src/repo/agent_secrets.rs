//! Secrets for an agent's tools (issue #233): set by the agent's owner,
//! or asked for by the agent and not set yet. Values are ciphertext the
//! gateway makes with its `KeyRing`, as credentials' are; this layer never
//! sees plaintext.

use chrono::{DateTime, Utc};
use havn_core::AgentId;
use sqlx::SqlitePool;

use crate::{DbError, Result};

/// Secrets (set or asked for) an agent may have.
pub const MAX_PER_AGENT: i64 = 100;
/// Secrets an agent may have asked for and not been given.
pub const MAX_REQUESTED: i64 = 20;

#[derive(Clone)]
pub struct AgentSecret {
    pub name: String,
    /// `None` while it is only asked for.
    pub value_ciphertext: Option<Vec<u8>>,
    pub request: Option<Request>,
    pub updated_at: DateTime<Utc>,
}

impl std::fmt::Debug for AgentSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSecret")
            .field("name", &self.name)
            .field("set", &self.value_ciphertext.is_some())
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// Why the agent asked for a secret it lacks, and in answer to whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub reason: String,
    /// The shell variable it asked to get it as.
    pub env: Option<String>,
    pub at: DateTime<Utc>,
    /// Who it was answering: a name, or a sender's id on a chat app;
    /// `None` for a scheduled task or a heartbeat.
    pub asked_by: Option<String>,
    /// Where: `web chat`, a chat app's name, `group on <app>`, `webhook`.
    pub asked_in: Option<String>,
    /// The owner, in their own chat.
    pub by_owner: bool,
    /// Times it asked again since; the first reason and variable stay.
    pub again: i64,
}

/// A request as [`request`] records it.
#[derive(Debug, Clone, Copy)]
pub struct NewRequest<'a> {
    pub reason: &'a str,
    pub env: Option<&'a str>,
    pub asked_by: Option<&'a str>,
    pub asked_in: Option<&'a str>,
    pub by_owner: bool,
}

/// What a secret was before a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Was {
    Absent,
    Requested,
    Set,
}

impl Was {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Requested => "requested",
            Self::Set => "set",
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    name: String,
    value: Option<Vec<u8>>,
    reason: Option<String>,
    env: Option<String>,
    requested_at: Option<String>,
    asked_by: Option<String>,
    asked_in: Option<String>,
    asked_by_owner: i64,
    asked_again: i64,
    updated_at: String,
}

const COLUMNS: &str = "name, value, reason, env, requested_at, asked_by, asked_in, \
                       asked_by_owner, asked_again, updated_at";

fn time(value: &str, column: &'static str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| DbError::InvalidValue {
            column,
            message: e.to_string(),
        })
}

fn from_row(row: Row) -> Result<AgentSecret> {
    let request = match (row.value.is_none(), row.reason, row.requested_at) {
        (true, Some(reason), Some(at)) => Some(Request {
            reason,
            env: row.env,
            at: time(&at, "agent_secrets.requested_at")?,
            asked_by: row.asked_by,
            asked_in: row.asked_in,
            by_owner: row.asked_by_owner != 0,
            again: row.asked_again,
        }),
        _ => None,
    };
    Ok(AgentSecret {
        name: row.name,
        value_ciphertext: row.value,
        request,
        updated_at: time(&row.updated_at, "agent_secrets.updated_at")?,
    })
}

/// The agent's secrets, set and asked for, by name.
pub async fn list(pool: &SqlitePool, agent_id: AgentId) -> Result<Vec<AgentSecret>> {
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM agent_secrets WHERE agent_id = ?1 ORDER BY name"
    ))
    .bind(agent_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(from_row).collect()
}

/// One secret of the agent, set or asked for.
pub async fn find(pool: &SqlitePool, agent_id: AgentId, name: &str) -> Result<Option<AgentSecret>> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM agent_secrets WHERE agent_id = ?1 AND name = ?2"
    ))
    .bind(agent_id.to_string())
    .bind(name)
    .fetch_optional(pool)
    .await?;
    row.map(from_row).transpose()
}

async fn was(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    agent_id: AgentId,
    name: &str,
) -> Result<Was> {
    let row: Option<(Option<Vec<u8>>,)> =
        sqlx::query_as("SELECT value FROM agent_secrets WHERE agent_id = ?1 AND name = ?2")
            .bind(agent_id.to_string())
            .bind(name)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(match row {
        None => Was::Absent,
        Some((None,)) => Was::Requested,
        Some((Some(_),)) => Was::Set,
    })
}

async fn count(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    agent_id: AgentId,
    requested_only: bool,
) -> Result<i64> {
    let sql = if requested_only {
        "SELECT COUNT(*) FROM agent_secrets WHERE agent_id = ?1 AND value IS NULL"
    } else {
        "SELECT COUNT(*) FROM agent_secrets WHERE agent_id = ?1"
    };
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(agent_id.to_string())
        .fetch_one(&mut **tx)
        .await?;
    Ok(n)
}

/// Set the secret `name` of the agent to `value_ciphertext`, ending a
/// request for it. Returns what it was; a conflict when it is new and the
/// agent has [`MAX_PER_AGENT`] already.
pub async fn set(
    pool: &SqlitePool,
    agent_id: AgentId,
    name: &str,
    value_ciphertext: &[u8],
) -> Result<Was> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let was = was(&mut tx, agent_id, name).await?;
    if was == Was::Absent && count(&mut tx, agent_id, false).await? >= MAX_PER_AGENT {
        return Err(DbError::Conflict("an agent may have at most 100 secrets"));
    }
    sqlx::query(
        "INSERT INTO agent_secrets (agent_id, name, value) VALUES (?1, ?2, ?3) \
         ON CONFLICT (agent_id, name) DO UPDATE SET value = excluded.value, reason = NULL, \
         env = NULL, requested_at = NULL, asked_by = NULL, asked_in = NULL, \
         asked_by_owner = 0, asked_again = 0, \
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(agent_id.to_string())
    .bind(name)
    .bind(value_ciphertext)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(was)
}

/// Record that the agent asked for the secret `name`, which it lacks.
/// Returns what it was: a secret already set is left as it is; asking
/// again for one still waiting only counts, keeping the first reason,
/// variable and who asked, which the owner may be reading. A conflict
/// when the agent has asked for [`MAX_REQUESTED`] secrets not set, or has
/// [`MAX_PER_AGENT`].
pub async fn request(
    pool: &SqlitePool,
    agent_id: AgentId,
    name: &str,
    new: NewRequest<'_>,
) -> Result<Was> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let was = was(&mut tx, agent_id, name).await?;
    match was {
        Was::Set => return Ok(was),
        Was::Requested => {
            sqlx::query(
                "UPDATE agent_secrets SET asked_again = asked_again + 1 \
                 WHERE agent_id = ?1 AND name = ?2 AND value IS NULL",
            )
            .bind(agent_id.to_string())
            .bind(name)
            .execute(&mut *tx)
            .await?;
        }
        Was::Absent => {
            if count(&mut tx, agent_id, true).await? >= MAX_REQUESTED {
                return Err(DbError::Conflict(
                    "20 secrets asked for are waiting for the owner already",
                ));
            }
            if count(&mut tx, agent_id, false).await? >= MAX_PER_AGENT {
                return Err(DbError::Conflict("an agent may have at most 100 secrets"));
            }
            sqlx::query(
                "INSERT INTO agent_secrets (agent_id, name, reason, env, requested_at, \
                 asked_by, asked_in, asked_by_owner) \
                 VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?5, ?6, ?7)",
            )
            .bind(agent_id.to_string())
            .bind(name)
            .bind(new.reason)
            .bind(new.env)
            .bind(new.asked_by)
            .bind(new.asked_in)
            .bind(i64::from(new.by_owner))
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(was)
}

/// Remove the secret `name` of the agent, or the request for it. Returns
/// what it was.
pub async fn remove(pool: &SqlitePool, agent_id: AgentId, name: &str) -> Result<Was> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let was = was(&mut tx, agent_id, name).await?;
    if was != Was::Absent {
        sqlx::query("DELETE FROM agent_secrets WHERE agent_id = ?1 AND name = ?2")
            .bind(agent_id.to_string())
            .bind(name)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(was)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::agents::{NewAgent, create as create_agent};
    use crate::repo::users::{NewUser, create as create_user};

    async fn agent(pool: &SqlitePool) -> AgentId {
        let owner = create_user(pool, NewUser { display_name: "o" })
            .await
            .expect("user");
        create_agent(
            pool,
            NewAgent {
                owner_id: owner.id,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent")
        .id
    }

    fn asked<'a>(reason: &'a str, env: Option<&'a str>) -> NewRequest<'a> {
        NewRequest {
            reason,
            env,
            asked_by: Some("tg:42"),
            asked_in: Some("telegram"),
            by_owner: false,
        }
    }

    #[tokio::test]
    async fn a_request_is_ended_by_setting_and_never_replaces_a_value() {
        let pool = connect_in_memory().await.expect("db");
        let id = agent(&pool).await;
        let was = request(
            &pool,
            id,
            "github",
            asked("to open the PR", Some("GH_TOKEN")),
        )
        .await
        .expect("request");
        assert_eq!(was, Was::Absent);
        let first = find(&pool, id, "github").await.expect("find").expect("row");
        assert!(first.value_ciphertext.is_none());
        let request_row = first.request.expect("a request");
        assert_eq!(request_row.reason, "to open the PR");
        assert_eq!(request_row.env.as_deref(), Some("GH_TOKEN"));
        assert_eq!(request_row.asked_by.as_deref(), Some("tg:42"));
        assert_eq!(request_row.asked_in.as_deref(), Some("telegram"));
        assert!(!request_row.by_owner);

        // Asked again: counted, the first reason and variable kept.
        assert_eq!(
            request(&pool, id, "github", asked("something else", Some("EVIL")))
                .await
                .expect("again"),
            Was::Requested
        );
        let again = find(&pool, id, "github")
            .await
            .expect("find")
            .expect("row")
            .request
            .expect("a request");
        assert_eq!(again.reason, "to open the PR");
        assert_eq!(again.env.as_deref(), Some("GH_TOKEN"));
        assert_eq!(again.again, 1);

        assert_eq!(
            set(&pool, id, "github", b"cipher").await.expect("set"),
            Was::Requested
        );
        let set_row = find(&pool, id, "github").await.expect("find").expect("row");
        assert_eq!(set_row.value_ciphertext.as_deref(), Some(&b"cipher"[..]));
        assert!(set_row.request.is_none());

        assert_eq!(
            request(&pool, id, "github", asked("again", None))
                .await
                .expect("request"),
            Was::Set
        );
        let kept = find(&pool, id, "github").await.expect("find").expect("row");
        assert_eq!(kept.value_ciphertext.as_deref(), Some(&b"cipher"[..]));
        assert!(kept.request.is_none());

        assert_eq!(
            set(&pool, id, "github", b"two").await.expect("set"),
            Was::Set
        );
        assert_eq!(remove(&pool, id, "github").await.expect("remove"), Was::Set);
        assert_eq!(
            remove(&pool, id, "github").await.expect("remove"),
            Was::Absent
        );
        assert!(list(&pool, id).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn requests_waiting_are_capped() {
        let pool = connect_in_memory().await.expect("db");
        let id = agent(&pool).await;
        for n in 0..MAX_REQUESTED {
            request(&pool, id, &format!("s{n}"), asked("why", None))
                .await
                .expect("request");
        }
        assert!(matches!(
            request(&pool, id, "one-more", asked("why", None)).await,
            Err(DbError::Conflict(_))
        ));
        // Asking again for one already asked for is not a new request.
        request(&pool, id, "s0", asked("why now", None))
            .await
            .expect("again");
        // Setting one makes room.
        set(&pool, id, "s1", b"c").await.expect("set");
        request(&pool, id, "one-more", asked("why", None))
            .await
            .expect("room");
    }

    #[tokio::test]
    async fn secrets_go_with_their_agent() {
        let pool = connect_in_memory().await.expect("db");
        let id = agent(&pool).await;
        set(&pool, id, "k", b"c").await.expect("set");
        crate::repo::agents::delete(&pool, id)
            .await
            .expect("delete");
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM agent_secrets")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(n, 0);
    }
}
