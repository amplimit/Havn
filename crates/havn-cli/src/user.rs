//! `havn user {add,list,delete}` — operator-side user provisioning
//! (spec §1.7).
//!
//! The CLI talks **directly** to the gateway SQLite, not via HTTP.
//! Reasoning: the operator who provisions users is on the same host as
//! the gateway, has filesystem access, and runs before any reverse
//! proxy is even configured. Going through HTTP would mean the CLI
//! itself needs an X-User-ID, which there's no clean way to bootstrap.
//! Direct DB access avoids that chicken-and-egg.
//!
//! The gateway must NOT be running concurrently with destructive `user
//! delete` commands — SQLite WAL allows concurrent readers but
//! deletions touching FK-cascading tables (agents, credentials,
//! audit_log) hold a writer lock. Reads (`list`) are always safe.

use std::path::PathBuf;
use std::str::FromStr as _;

use anyhow::Context as _;
use havn_core::UserId;
use havn_db::repo::users;

/// Resolve the gateway DB path from `$HAVN_DB`, the config file at
/// `$HAVN_CONFIG`, or the default location. Mirrors the gateway's own
/// resolution so the CLI hits the same file.
fn resolve_db_path() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("HAVN_DB") {
        return Ok(PathBuf::from(p));
    }
    // The gateway config is TOML; we don't depend on the gateway's
    // config crate (would create a cycle: cli → gateway → cli). Read
    // the file by hand and pull just `db_path`.
    let config_path = std::env::var_os("HAVN_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config/havn/config.toml")
        });
    if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        if let Ok(value) = raw.parse::<toml::Value>()
            && let Some(db) = value.get("db_path").and_then(|v| v.as_str())
        {
            return Ok(PathBuf::from(db));
        }
    }
    // Last-resort default — matches gateway/src/config.rs.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    Ok(home.join(".local/share/havn/havn.db"))
}

/// Public alias so sibling subcommand modules (`team`, `role`) can
/// share the same DB-resolution logic without duplicating the config
/// fallback chain.
pub async fn open_db_pub() -> anyhow::Result<sqlx::SqlitePool> {
    open_db().await
}

async fn open_db() -> anyhow::Result<sqlx::SqlitePool> {
    let path = resolve_db_path()?;
    if !path.exists() {
        anyhow::bail!(
            "havn DB not found at {}. Run `havn setup` first, or set HAVN_DB to the gateway DB path.",
            path.display()
        );
    }
    havn_db::connect(&path)
        .await
        .with_context(|| format!("opening havn DB at {}", path.display()))
}

pub async fn add(id: &str, display_name: &str) -> anyhow::Result<()> {
    if display_name.trim().is_empty() {
        anyhow::bail!("--display-name must be non-empty");
    }
    let user_id = UserId::from_str(id)
        .map_err(|_| anyhow::anyhow!("user id must be a UUID v7 (got {id:?})"))?;
    let pool = open_db().await?;
    let user = users::create_with_id(&pool, user_id, display_name)
        .await
        .with_context(|| format!("inserting user {user_id}"))?;
    println!("added user {} ({})", user.id, user.display_name);
    Ok(())
}

pub async fn list() -> anyhow::Result<()> {
    let pool = open_db().await?;
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, display_name, created_at FROM users ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .context("listing users")?;
    if rows.is_empty() {
        println!("(no users)");
        return Ok(());
    }
    for (id, display_name, created_at) in rows {
        println!("{id}  {display_name:24}  {created_at}");
    }
    Ok(())
}

pub async fn delete(id: &str) -> anyhow::Result<()> {
    let user_id = UserId::from_str(id)
        .map_err(|_| anyhow::anyhow!("user id must be a UUID v7 (got {id:?})"))?;
    let pool = open_db().await?;
    // Hard-delete. FK cascades on agents.owner_id wipe the user's
    // agents, on credentials they take their credentials, on
    // credential_usages the usage rows. audit_log SETs NULL on the
    // user_id (caller of long-gone records). team_memberships go too.
    //
    // This is destructive — the operator presumably intends it. The
    // caller printed-warning is the only confirmation.
    let res = sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(user_id.to_string())
        .execute(&pool)
        .await
        .context("deleting user")?;
    if res.rows_affected() == 0 {
        anyhow::bail!("no user with id {user_id}");
    }
    println!("deleted user {user_id} (and all owned agents / credentials)");
    Ok(())
}
