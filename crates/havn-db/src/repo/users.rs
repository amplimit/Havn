//! Users repository — the root of every ownership chain.
//!
//! Spec §1.6 / §1.7 v0.6: havn does no authentication. The User row
//! is just an authorisation key — the upstream reverse proxy
//! authenticates the human and forwards the resolved subject as the
//! `X-User-ID` header; the gateway looks it up here. No password
//! hash, no email-verification state, no recovery token. Email and
//! auth_provider columns were dropped in migration 0003.

use chrono::{DateTime, Utc};
use havn_core::UserId;
use sqlx::SqlitePool;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone)]
pub struct User {
    pub id: UserId,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewUser<'a> {
    pub display_name: &'a str,
}

pub async fn create(pool: &SqlitePool, new: NewUser<'_>) -> Result<User> {
    let id = UserId::new();
    sqlx::query("INSERT INTO users (id, display_name) VALUES (?1, ?2)")
        .bind(id.to_string())
        .bind(new.display_name)
        .execute(pool)
        .await?;

    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

/// Insert a user with a caller-supplied ID. Used by `havn user add <id>`
/// in the multi-tenant deployment shape — the operator has already
/// chosen the X-User-ID value (typically the upstream SSO subject), and
/// we record it verbatim so future header lookups match.
pub async fn create_with_id(pool: &SqlitePool, id: UserId, display_name: &str) -> Result<User> {
    sqlx::query("INSERT INTO users (id, display_name) VALUES (?1, ?2)")
        .bind(id.to_string())
        .bind(display_name)
        .execute(pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => DbError::Conflict("users.id"),
            _ => DbError::from(e),
        })?;
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn find_by_id(pool: &SqlitePool, id: UserId) -> Result<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as::<_, UserRow>(
        "SELECT id, display_name, created_at FROM users WHERE id = ?1",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;

    row.map(User::try_from).transpose()
}

/// Single-user-mode bootstrap. Returns the only user in the database,
/// creating a default `havn` user if none exist. Used by the gateway
/// on first startup before the reverse proxy is wired up — the
/// loopback-only single-user path of spec §1.7.
pub async fn ensure_default(pool: &SqlitePool) -> Result<User> {
    if let Some(user) = find_first(pool).await? {
        return Ok(user);
    }
    create(
        pool,
        NewUser {
            display_name: "havn",
        },
    )
    .await
}

async fn find_first(pool: &SqlitePool) -> Result<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as::<_, UserRow>(
        "SELECT id, display_name, created_at FROM users \
         ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    row.map(User::try_from).transpose()
}

#[derive(Debug, sqlx::FromRow)]
struct UserRow {
    id: String,
    display_name: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<UserRow> for User {
    type Error = DbError;
    fn try_from(r: UserRow) -> Result<Self> {
        Ok(Self {
            id: UserId::from_uuid(parse_db_uuid(&r.id, "users.id")?),
            display_name: r.display_name,
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
    async fn create_and_find_round_trip() {
        let pool = connect_in_memory().await.expect("connect");
        let created = create(
            &pool,
            NewUser {
                display_name: "Ada",
            },
        )
        .await
        .expect("create");
        let by_id = find_by_id(&pool, created.id)
            .await
            .expect("find")
            .expect("some");
        assert_eq!(created.id, by_id.id);
        assert_eq!(by_id.display_name, "Ada");
    }

    #[tokio::test]
    async fn ensure_default_creates_then_returns_existing() {
        let pool = connect_in_memory().await.expect("connect");
        let first = ensure_default(&pool).await.expect("ensure 1");
        let second = ensure_default(&pool).await.expect("ensure 2");
        assert_eq!(first.id, second.id);
        assert_eq!(first.display_name, "havn");
    }

    #[tokio::test]
    async fn create_with_id_round_trip() {
        let pool = connect_in_memory().await.expect("connect");
        let id = UserId::new();
        let u = create_with_id(&pool, id, "Alice")
            .await
            .expect("create_with_id");
        assert_eq!(u.id, id);
        assert_eq!(u.display_name, "Alice");
    }

    #[tokio::test]
    async fn duplicate_id_returns_conflict() {
        let pool = connect_in_memory().await.expect("connect");
        let id = UserId::new();
        create_with_id(&pool, id, "first").await.expect("first");
        let dup = create_with_id(&pool, id, "second").await;
        assert!(
            matches!(dup, Err(DbError::Conflict("users.id"))),
            "got {dup:?}"
        );
    }
}
