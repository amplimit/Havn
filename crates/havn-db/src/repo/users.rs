//! Users repository — the root of every ownership chain.
//!
//! Spec §1.6 / §1.7 v0.6: havn does no authentication. The User row
//! is just an authorisation key — the upstream reverse proxy
//! authenticates the human and forwards the resolved subject as the
//! identity header (`X-User-ID` by default); the gateway looks it up
//! here, by internal id or by `external_id` (migration 0008). No password
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
    /// Subject the upstream proxy emits for this user (username or
    /// email), matched exactly and case-sensitively.
    pub external_id: Option<String>,
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

/// Insert a user with a caller-supplied ID and optional external
/// identity. Used by `havn user add` in the multi-tenant deployment
/// shape: the operator records the value the upstream proxy will emit
/// (the internal id itself, or `external_id`) so header lookups match.
pub async fn create_with_id(
    pool: &SqlitePool,
    id: UserId,
    display_name: &str,
    external_id: Option<&str>,
) -> Result<User> {
    check_external_id(external_id)?;
    sqlx::query("INSERT INTO users (id, display_name, external_id) VALUES (?1, ?2, ?3)")
        .bind(id.to_string())
        .bind(display_name)
        .bind(external_id)
        .execute(pool)
        .await
        .map_err(unique_violation)?;
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

/// Attach (or replace) the external identity of an existing user.
pub async fn set_external_id(pool: &SqlitePool, id: UserId, external_id: &str) -> Result<User> {
    check_external_id(Some(external_id))?;
    let res = sqlx::query("UPDATE users SET external_id = ?2 WHERE id = ?1")
        .bind(id.to_string())
        .bind(external_id)
        .execute(pool)
        .await
        .map_err(unique_violation)?;
    if res.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

// The gateway trims header values, and header values cannot carry control
// characters, so such an identity could never be matched.
fn check_external_id(external_id: Option<&str>) -> Result<()> {
    match external_id {
        Some(v) if v.is_empty() || v.trim() != v || v.chars().any(char::is_control) => {
            Err(DbError::InvalidValue {
                column: "users.external_id",
                message: format!(
                    "{v:?} must be non-empty, without surrounding whitespace or control characters"
                ),
            })
        }
        _ => Ok(()),
    }
}

fn unique_violation(e: sqlx::Error) -> DbError {
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            if db.message().contains("external_id") {
                DbError::Conflict("users.external_id")
            } else {
                DbError::Conflict("users.id")
            }
        }
        _ => DbError::from(e),
    }
}

pub async fn find_by_id(pool: &SqlitePool, id: UserId) -> Result<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as::<_, UserRow>(
        "SELECT id, display_name, external_id, created_at FROM users WHERE id = ?1",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;

    row.map(User::try_from).transpose()
}

pub async fn find_by_external_id(pool: &SqlitePool, external_id: &str) -> Result<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as::<_, UserRow>(
        "SELECT id, display_name, external_id, created_at FROM users WHERE external_id = ?1",
    )
    .bind(external_id)
    .fetch_optional(pool)
    .await?;

    row.map(User::try_from).transpose()
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<User>> {
    let rows: Vec<UserRow> = sqlx::query_as::<_, UserRow>(
        "SELECT id, display_name, external_id, created_at FROM users ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(User::try_from).collect()
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
        "SELECT id, display_name, external_id, created_at FROM users \
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
    external_id: Option<String>,
    created_at: DateTime<Utc>,
}

impl TryFrom<UserRow> for User {
    type Error = DbError;
    fn try_from(r: UserRow) -> Result<Self> {
        Ok(Self {
            id: UserId::from_uuid(parse_db_uuid(&r.id, "users.id")?),
            display_name: r.display_name,
            external_id: r.external_id,
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
        let u = create_with_id(&pool, id, "Alice", None)
            .await
            .expect("create_with_id");
        assert_eq!(u.id, id);
        assert_eq!(u.display_name, "Alice");
    }

    #[tokio::test]
    async fn duplicate_id_returns_conflict() {
        let pool = connect_in_memory().await.expect("connect");
        let id = UserId::new();
        create_with_id(&pool, id, "first", None)
            .await
            .expect("first");
        let dup = create_with_id(&pool, id, "second", None).await;
        assert!(
            matches!(dup, Err(DbError::Conflict("users.id"))),
            "got {dup:?}"
        );
    }

    #[tokio::test]
    async fn external_id_round_trip_is_exact() {
        let pool = connect_in_memory().await.expect("connect");
        let id = UserId::new();
        create_with_id(&pool, id, "Alice", Some("alice@example.com"))
            .await
            .expect("create");
        let found = find_by_external_id(&pool, "alice@example.com")
            .await
            .expect("find")
            .expect("some");
        assert_eq!(found.id, id);
        assert_eq!(found.external_id.as_deref(), Some("alice@example.com"));
        assert!(
            find_by_external_id(&pool, "Alice@example.com")
                .await
                .expect("find")
                .is_none(),
            "matching is case-sensitive"
        );
    }

    #[tokio::test]
    async fn duplicate_external_id_returns_conflict() {
        let pool = connect_in_memory().await.expect("connect");
        create_with_id(&pool, UserId::new(), "a", Some("alice"))
            .await
            .expect("first");
        let dup = create_with_id(&pool, UserId::new(), "b", Some("alice")).await;
        assert!(
            matches!(dup, Err(DbError::Conflict("users.external_id"))),
            "got {dup:?}"
        );
        let other = create_with_id(&pool, UserId::new(), "c", None)
            .await
            .expect("no external id");
        let taken = set_external_id(&pool, other.id, "alice").await;
        assert!(
            matches!(taken, Err(DbError::Conflict("users.external_id"))),
            "got {taken:?}"
        );
        create_with_id(&pool, UserId::new(), "d", None)
            .await
            .expect("several users without an external id");
    }

    #[tokio::test]
    async fn set_external_id_links_existing_user() {
        let pool = connect_in_memory().await.expect("connect");
        let user = ensure_default(&pool).await.expect("bootstrap");
        set_external_id(&pool, user.id, "ops@example.com")
            .await
            .expect("set");
        let found = find_by_external_id(&pool, "ops@example.com")
            .await
            .expect("find")
            .expect("some");
        assert_eq!(found.id, user.id);
        let missing = set_external_id(&pool, UserId::new(), "x").await;
        assert!(matches!(missing, Err(DbError::NotFound)), "got {missing:?}");
    }

    #[tokio::test]
    async fn unusable_external_ids_are_refused() {
        let pool = connect_in_memory().await.expect("connect");
        for bad in ["", " alice", "alice ", "al\tice"] {
            let res = create_with_id(&pool, UserId::new(), "x", Some(bad)).await;
            assert!(
                matches!(res, Err(DbError::InvalidValue { .. })),
                "{bad:?}: {res:?}"
            );
        }
    }

    #[tokio::test]
    async fn external_id_migration_keeps_existing_users() {
        use sqlx::migrate::Migrator;
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::borrow::Cow;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .in_memory(true)
                    .foreign_keys(true),
            )
            .await
            .expect("connect");
        let before = Migrator {
            migrations: Cow::Owned(
                crate::MIGRATOR
                    .iter()
                    .filter(|m| m.version < 8)
                    .cloned()
                    .collect(),
            ),
            ..Migrator::DEFAULT
        };
        before.run(&pool).await.expect("migrate up to 0007");
        let id = UserId::new();
        sqlx::query("INSERT INTO users (id, display_name) VALUES (?1, 'old')")
            .bind(id.to_string())
            .execute(&pool)
            .await
            .expect("legacy insert");

        crate::MIGRATOR.run(&pool).await.expect("migrate to latest");
        let user = find_by_id(&pool, id).await.expect("find").expect("some");
        assert_eq!(user.display_name, "old");
        assert_eq!(user.external_id, None);
        set_external_id(&pool, id, "old@example.com")
            .await
            .expect("set on migrated row");
    }
}
