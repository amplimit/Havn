//! havn data access — `SQLite` pool + embedded migrations + repositories.
//!
//! The gateway holds a single [`sqlx::SqlitePool`] over the database file at
//! `gateway.db_path`. Each agent has its own per-workspace `SQLite` (different
//! crate / different file). Per [project scope](../project_havn_scope.md),
//! havn does not depend on any non-`SQLite` datastore.

use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

pub mod agent;
pub mod error;
pub mod repo;

pub use error::{DbError, Result};

/// Gateway-DB migrations under `crates/havn-db/migrations/`. Run automatically by
/// [`connect`] / [`connect_in_memory`].
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Per-agent-DB migrations under `crates/havn-db/migrations_agent/`. Run automatically by
/// [`agent::connect`] / [`agent::connect_in_memory`].
pub static AGENT_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations_agent");

/// Open or create a `SQLite` database at `path`, run all pending migrations,
/// and return a connected pool.
///
/// PRAGMAs applied at connect time:
/// - `journal_mode = WAL` — concurrent readers + one writer; standard for server use.
/// - `foreign_keys = ON` — enforced (off by default in `SQLite`!).
/// - `synchronous = NORMAL` — safe with WAL on a single host.
/// - `busy_timeout = 5s` — wait briefly when contended instead of failing immediately.
pub async fn connect<P: AsRef<Path>>(path: P) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Normal);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    MIGRATOR.run(&pool).await.map_err(DbError::Migrate)?;
    Ok(pool)
}

/// Open an ephemeral in-memory `SQLite` for tests.
///
/// Uses a single connection so every query observes the same database state.
#[doc(hidden)]
pub async fn connect_in_memory() -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .in_memory(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;

    MIGRATOR.run(&pool).await.map_err(DbError::Migrate)?;
    Ok(pool)
}
