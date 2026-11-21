//! Webhooks (issue #236): addresses outside systems call to have an agent
//! act on what they send.

use chrono::{DateTime, Utc};
use havn_core::AgentId;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

/// Webhooks an agent may have (named in the conflict [`create`] gives).
pub const MAX_PER_AGENT: i64 = 20;

/// Bytes of a webhook's secret, given out as hex.
const SECRET_BYTES: usize = 32;

#[derive(Clone)]
pub struct Webhook {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub name: String,
    pub secret: String,
    pub instructions: String,
    pub deliver: String,
    pub created_at: DateTime<Utc>,
    pub last_called_at: Option<DateTime<Utc>>,
    pub calls: i64,
}

impl std::fmt::Debug for Webhook {
    // The secret stays out of logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhook")
            .field("id", &self.id)
            .field("agent_id", &self.agent_id)
            .field("name", &self.name)
            .field("deliver", &self.deliver)
            .field("calls", &self.calls)
            .finish_non_exhaustive()
    }
}

type Row = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
);

const COLUMNS: &str =
    "id, agent_id, name, secret, instructions, deliver, created_at, last_called_at, calls";

fn time(value: &str, column: &'static str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| DbError::InvalidValue {
            column,
            message: e.to_string(),
        })
}

fn from_row(row: Row) -> Result<Webhook> {
    let (id, agent_id, name, secret, instructions, deliver, created_at, last_called_at, calls) =
        row;
    Ok(Webhook {
        id: parse_db_uuid(&id, "webhooks.id")?,
        agent_id: AgentId::from_uuid(parse_db_uuid(&agent_id, "webhooks.agent_id")?),
        name,
        secret,
        instructions,
        deliver,
        created_at: time(&created_at, "webhooks.created_at")?,
        last_called_at: last_called_at
            .as_deref()
            .map(|t| time(t, "webhooks.last_called_at"))
            .transpose()?,
        calls,
    })
}

/// A new secret from the OS RNG, as hex.
pub fn generate_secret() -> Result<String> {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| DbError::Random(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug)]
pub struct NewWebhook<'a> {
    pub agent_id: AgentId,
    pub name: &'a str,
    pub instructions: &'a str,
    pub deliver: &'a str,
}

/// Make a webhook for `new.agent_id`, with a new secret; a conflict when
/// the agent has [`MAX_PER_AGENT`] already.
pub async fn create(pool: &SqlitePool, new: NewWebhook<'_>) -> Result<Webhook> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM webhooks WHERE agent_id = ?1")
        .bind(new.agent_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_PER_AGENT {
        return Err(DbError::Conflict("an agent may have at most 20 webhooks"));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO webhooks (id, agent_id, name, secret, instructions, deliver) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(id.to_string())
    .bind(new.agent_id.to_string())
    .bind(new.name)
    .bind(generate_secret()?)
    .bind(new.instructions)
    .bind(new.deliver)
    .execute(&mut *tx)
    .await?;
    let row: Row = sqlx::query_as(&format!("SELECT {COLUMNS} FROM webhooks WHERE id = ?1"))
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    from_row(row)
}

pub async fn find(pool: &SqlitePool, id: Uuid) -> Result<Option<Webhook>> {
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM webhooks WHERE id = ?1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    row.map(from_row).transpose()
}

/// `agent_id`'s webhooks, oldest first.
pub async fn list_for_agent(pool: &SqlitePool, agent_id: AgentId) -> Result<Vec<Webhook>> {
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM webhooks WHERE agent_id = ?1 ORDER BY created_at, id"
    ))
    .bind(agent_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(from_row).collect()
}

/// Set what `id` does with a call and where its results go.
pub async fn update(pool: &SqlitePool, id: Uuid, instructions: &str, deliver: &str) -> Result<()> {
    sqlx::query("UPDATE webhooks SET instructions = ?2, deliver = ?3 WHERE id = ?1")
        .bind(id.to_string())
        .bind(instructions)
        .bind(deliver)
        .execute(pool)
        .await?;
    Ok(())
}

/// Note that `id` was called at `at`.
pub async fn called(pool: &SqlitePool, id: Uuid, at: DateTime<Utc>) -> Result<()> {
    sqlx::query("UPDATE webhooks SET last_called_at = ?2, calls = calls + 1 WHERE id = ?1")
        .bind(id.to_string())
        .bind(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete `id`; whether it was there.
pub async fn delete(pool: &SqlitePool, id: Uuid) -> Result<bool> {
    let res = sqlx::query("DELETE FROM webhooks WHERE id = ?1")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::repo::agents::{NewAgent, create as create_agent};
    use crate::repo::users::{NewUser, create as create_user};

    #[tokio::test]
    async fn webhooks_are_made_with_a_secret_listed_called_and_deleted() {
        let pool = crate::connect_in_memory().await.expect("db");
        let user = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user");
        let agent = create_agent(
            &pool,
            NewAgent {
                owner_id: user.id,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        let hook = create(
            &pool,
            NewWebhook {
                agent_id: agent.id,
                name: "alerts",
                instructions: "summarise",
                deliver: "web",
            },
        )
        .await
        .expect("hook");
        assert_eq!(hook.secret.len(), SECRET_BYTES * 2);
        assert!(hook.secret.bytes().all(|b| b.is_ascii_hexdigit()));
        let other = create(
            &pool,
            NewWebhook {
                agent_id: agent.id,
                name: "ci",
                instructions: "",
                deliver: "last",
            },
        )
        .await
        .expect("other");
        assert_ne!(hook.secret, other.secret);

        update(&pool, hook.id, "triage", "telegram")
            .await
            .expect("update");
        called(&pool, hook.id, Utc::now()).await.expect("called");
        let found = find(&pool, hook.id).await.expect("find").expect("there");
        assert_eq!(
            (
                found.instructions.as_str(),
                found.deliver.as_str(),
                found.calls
            ),
            ("triage", "telegram", 1)
        );
        assert!(found.last_called_at.is_some());
        let names: Vec<String> = list_for_agent(&pool, agent.id)
            .await
            .expect("list")
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(names, vec!["alerts", "ci"]);

        assert!(delete(&pool, hook.id).await.expect("delete"));
        assert!(!delete(&pool, hook.id).await.expect("again"));
        assert!(find(&pool, hook.id).await.expect("find").is_none());

        for n in 1..MAX_PER_AGENT {
            create(
                &pool,
                NewWebhook {
                    agent_id: agent.id,
                    name: &format!("h{n}"),
                    instructions: "",
                    deliver: "web",
                },
            )
            .await
            .expect("room");
        }
        let full = create(
            &pool,
            NewWebhook {
                agent_id: agent.id,
                name: "one too many",
                instructions: "",
                deliver: "web",
            },
        )
        .await;
        assert!(matches!(full, Err(DbError::Conflict(_))), "{full:?}");
    }
}
