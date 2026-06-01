//! Per-agent skill metadata repo (spec §9.5).
//!
//! The runtime's `skills_index` table holds the FTS-mirrored body of
//! every active skill plus the curator-relevant signals (`source`,
//! `pinned`, `last_used_at`, `use_count`, `archived_at`). Authoritative
//! file-on-disk lives at `workspace/skills/<name>/SKILL.md`; this row
//! is the index that retrieval and the curator pass query.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::Result;
use crate::agent::hybrid_common::{EmbeddedCandidate, FtsHit, bytes_to_f32};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Bundled,
    Workspace,
}

impl Source {
    fn parse(s: &str) -> Self {
        match s {
            "bundled" => Self::Bundled,
            _ => Self::Workspace,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CuratableSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: Source,
    pub pinned: bool,
    pub last_used_at: Option<DateTime<Utc>>,
    pub use_count: i64,
}

/// Skills the curator is allowed to consider:
/// - `source = workspace` (bundled skills are immutable inputs from the
///   binary; the curator must never touch them)
/// - `pinned = 0` (operator / agent has marked it sacred)
/// - `archived_at IS NULL` (not already archived)
///
/// Ordered by `last_used_at NULLS FIRST` so the rule-based aging phase
/// hits the cold candidates first. Bounded by `limit` so a runaway
/// curator doesn't try to reason about thousands of skills in one LLM
/// call (spec §9.5 implies a per-pass cap).
pub async fn list_curatable(pool: &SqlitePool, limit: u32) -> Result<Vec<CuratableSkill>> {
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT id, name, description, body, source, pinned, last_used_at, use_count \
         FROM skills_index \
         WHERE source = 'workspace' AND pinned = 0 AND archived_at IS NULL \
         ORDER BY (last_used_at IS NOT NULL), last_used_at, name \
         LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(CuratableSkill::from).collect())
}

/// Soft-archive a skill by name. Mirrors memory's `forget` shape:
/// sets `archived_at = now`. Returns `true` when a row was flipped.
/// The on-disk SKILL.md is moved separately by the curator (it has the
/// workspace path; this repo just owns the DB column).
pub async fn archive(pool: &SqlitePool, name: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE skills_index \
         SET archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE name = ?1 AND archived_at IS NULL",
    )
    .bind(name)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Every active row, regardless of source / pinned state. For the
/// dashboard `/skills` page — operators want to see what's installed,
/// including bundled skills they can't curate. Newest first.
pub async fn list_all_active(pool: &SqlitePool, limit: u32) -> Result<Vec<CuratableSkill>> {
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT id, name, description, body, source, pinned, last_used_at, use_count \
         FROM skills_index \
         WHERE archived_at IS NULL \
         ORDER BY (source = 'bundled') DESC, name ASC \
         LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(CuratableSkill::from).collect())
}

/// Archived rows for the audit trail. `like_pattern` lets the caller
/// filter by suffix (`@archived:%`, `@forgotten:%`) — but the dashboard
/// just calls with `%` to get all. Newest archived first.
pub async fn list_archived(pool: &SqlitePool, limit: u32) -> Result<Vec<CuratableSkill>> {
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT id, name, description, body, source, pinned, last_used_at, use_count \
         FROM skills_index \
         WHERE archived_at IS NOT NULL \
         ORDER BY archived_at DESC \
         LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(CuratableSkill::from).collect())
}

// ---------------------------------------------------------------- //
// Hybrid retrieval — embedding storage + FTS5 + recall (spec §13). //
// ---------------------------------------------------------------- //

/// Persist an embedding for the active skill row keyed by `name`.
/// Skills are uniquely keyed by name + survive `skill_manage` updates
/// in place (the index_into upsert preserves the row id), so by-name
/// pinning doesn't have memory's race-fix concern from §9.4. Returns
/// `Ok(true)` when a row was found and updated.
///
/// Vectors stored as native-byte-order f32 BLOB via
/// `bytemuck::cast_slice`. Same on-disk shape as the memory table —
/// runtime can reuse one decoder.
pub async fn set_embedding(pool: &SqlitePool, name: &str, vector: &[f32]) -> Result<bool> {
    let bytes: &[u8] = bytemuck::cast_slice(vector);
    let dim = i64::try_from(vector.len()).unwrap_or(0);
    let res = sqlx::query(
        "UPDATE skills_index \
         SET embedding = ?1, embedding_dim = ?2 \
         WHERE name = ?3 AND archived_at IS NULL",
    )
    .bind(bytes)
    .bind(dim)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Active candidate rows that carry a vector at `expected_dim`. Used
/// by the runtime hybrid scorer's vector leg. Wrong-dim and missing-
/// vector rows are filtered out SQL-side; they remain findable via
/// FTS so retrieval degrades rather than breaks when an operator
/// switches embedding providers.
pub async fn embedded_candidates(
    pool: &SqlitePool,
    expected_dim: usize,
) -> Result<Vec<EmbeddedCandidate>> {
    let expected_i64 = i64::try_from(expected_dim).unwrap_or(i64::MAX);
    let rows: Vec<(String, Vec<u8>, i64)> = sqlx::query_as::<_, (String, Vec<u8>, i64)>(
        "SELECT id, embedding, embedding_dim FROM skills_index \
         WHERE archived_at IS NULL \
           AND embedding IS NOT NULL \
           AND embedding_dim = ?1",
    )
    .bind(expected_i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, bytes, _dim)| EmbeddedCandidate {
            id,
            embedding: bytes_to_f32(&bytes),
        })
        .collect())
}

/// FTS5 hits for a skills query, restricted to active rows. Returns
/// up to `limit` hits ordered best-first. Caller is responsible for
/// pre-escaping the query (`havn_db::agent::conversations::escape_fts_query`).
pub async fn fts_hits(pool: &SqlitePool, query: &str, limit: u32) -> Result<Vec<FtsHit>> {
    let rows: Vec<(String, f64)> = sqlx::query_as::<_, (String, f64)>(
        "SELECT s.id, rank \
         FROM skills_fts f JOIN skills_index s ON s.rowid = f.rowid \
         WHERE skills_fts MATCH ?1 AND s.archived_at IS NULL \
         ORDER BY rank LIMIT ?2",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, rank)| FtsHit { id, rank })
        .collect())
}

/// Fetch one active skill row by id. Used by the hybrid scorer
/// after MMR has picked winners.
pub async fn fetch_by_id(pool: &SqlitePool, id: &str) -> Result<Option<CuratableSkill>> {
    let row: Option<Row> = sqlx::query_as::<_, Row>(
        "SELECT id, name, description, body, source, pinned, last_used_at, use_count \
         FROM skills_index \
         WHERE id = ?1 AND archived_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(CuratableSkill::from))
}

/// Bump `use_count` + `last_used_at` for the given (already-truncated
/// to top-K) ids. Mirrors memory's `bump_recall_for` shape so the
/// runtime's `HybridSource` impl can call it the same way.
pub async fn bump_use_for(pool: &SqlitePool, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = (0..ids.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE skills_index \
         SET use_count = use_count + 1, \
             last_used_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.execute(pool).await?;
    Ok(())
}

/// Mark a skill as pinned (or unpinned). Pinned skills are excluded
/// from `list_curatable` so the curator never touches them.
pub async fn set_pinned(pool: &SqlitePool, name: &str, pinned: bool) -> Result<bool> {
    let res = sqlx::query("UPDATE skills_index SET pinned = ?1 WHERE name = ?2")
        .bind(i64::from(pinned))
        .bind(name)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    id: String,
    name: String,
    description: String,
    body: String,
    source: String,
    pinned: i64,
    last_used_at: Option<DateTime<Utc>>,
    use_count: i64,
}

impl From<Row> for CuratableSkill {
    fn from(r: Row) -> Self {
        Self {
            id: r.id,
            name: r.name,
            description: r.description,
            body: r.body,
            source: Source::parse(&r.source),
            pinned: r.pinned != 0,
            last_used_at: r.last_used_at,
            use_count: r.use_count,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::agent::connect_in_memory;

    async fn insert(
        pool: &SqlitePool,
        name: &str,
        source: &str,
        pinned: i64,
        last_used_offset_days: Option<i64>,
        archived: bool,
    ) {
        sqlx::query(
            "INSERT INTO skills_index (id, name, description, body, source, pinned, last_used_at, archived_at) \
             VALUES (?1, ?2, 'd', 'b', ?3, ?4, \
                CASE WHEN ?5 IS NULL THEN NULL \
                     ELSE strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?6) END, \
                CASE WHEN ?7 = 1 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ELSE NULL END)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(name)
        .bind(source)
        .bind(pinned)
        .bind(last_used_offset_days)
        .bind(last_used_offset_days.map_or(String::new(), |d| format!("{d} days")))
        .bind(i64::from(archived))
        .execute(pool)
        .await
        .expect("insert");
    }

    #[tokio::test]
    async fn list_curatable_excludes_bundled_pinned_archived() {
        let pool = connect_in_memory().await.expect("connect");
        insert(&pool, "bundled-one", "bundled", 0, Some(0), false).await;
        insert(&pool, "pinned-one", "workspace", 1, Some(0), false).await;
        insert(&pool, "archived-one", "workspace", 0, Some(0), true).await;
        insert(&pool, "active-cold", "workspace", 0, Some(-100), false).await;
        insert(&pool, "active-hot", "workspace", 0, Some(0), false).await;
        insert(&pool, "active-never", "workspace", 0, None, false).await;

        let curatable = list_curatable(&pool, 50).await.expect("list");
        let names: Vec<&str> = curatable.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"bundled-one"));
        assert!(!names.contains(&"pinned-one"));
        assert!(!names.contains(&"archived-one"));
        assert!(names.contains(&"active-cold"));
        assert!(names.contains(&"active-hot"));
        assert!(names.contains(&"active-never"));

        // Order: never-used first (NULL last_used), then oldest used, then newest.
        assert_eq!(names[0], "active-never");
        assert_eq!(names[1], "active-cold");
        assert_eq!(names[2], "active-hot");
    }

    #[tokio::test]
    async fn archive_then_excluded() {
        let pool = connect_in_memory().await.expect("connect");
        insert(&pool, "k", "workspace", 0, Some(-200), false).await;
        assert!(archive(&pool, "k").await.expect("archive"));
        assert!(!archive(&pool, "k").await.expect("archive idempotent"));
        let curatable = list_curatable(&pool, 50).await.expect("list");
        assert!(curatable.iter().all(|s| s.name != "k"));
    }

    #[tokio::test]
    async fn pinning_excludes_from_curator() {
        let pool = connect_in_memory().await.expect("connect");
        insert(&pool, "k", "workspace", 0, Some(-200), false).await;
        assert!(set_pinned(&pool, "k", true).await.expect("pin"));
        let curatable = list_curatable(&pool, 50).await.expect("list");
        assert!(curatable.iter().all(|s| s.name != "k"));
        assert!(set_pinned(&pool, "k", false).await.expect("unpin"));
        let curatable = list_curatable(&pool, 50).await.expect("list");
        assert!(curatable.iter().any(|s| s.name == "k"));
    }
}
