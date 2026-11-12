//! Per-agent `SQLite` — one file per agent at `<workspace>/agent.db`.
//!
//! Schema is independent from the gateway database: conversations, curated
//! memory, and a skill index, each with FTS5 mirrors. Owned by the agent
//! runtime process; the gateway never opens these files directly (spec §5.2).

use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::{AGENT_MIGRATOR, DbError, Result};

pub mod conversations;
pub mod hybrid_common;
pub mod memory;
pub mod skills_index;

/// Open or create the per-agent `SQLite` at `path`, run all pending migrations,
/// and return a connected pool. Same pragmas as the gateway DB ([`crate::connect`]):
/// WAL journal, foreign keys ON, NORMAL sync, 5s busy timeout.
pub async fn connect<P: AsRef<Path>>(path: P) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Normal);

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await?;

    AGENT_MIGRATOR.run(&pool).await.map_err(DbError::Migrate)?;
    Ok(pool)
}

/// In-memory pool for tests. Single connection so all queries see the same data.
#[doc(hidden)]
pub async fn connect_in_memory() -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .in_memory(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    AGENT_MIGRATOR.run(&pool).await.map_err(DbError::Migrate)?;
    Ok(pool)
}
