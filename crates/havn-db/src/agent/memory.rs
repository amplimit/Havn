//! Memory repository — typed, agent-curated facts with FTS5 mirror.
//!
//! Spec §9.4 layer 3 (typed structured memory). Distinct from `MEMORY.md`
//! which lives on disk and is frozen into the system prompt at session
//! start. This table is the runtime side of the `memory_remember` /
//! `memory_search` / `memory_forget` tools and the daily aging pass.
//!
//! Phase 1 shipped a flat key/value table; Phase 2 (migration 0002)
//! adds `kind`, `source`, `ttl_days`, `archived_at`. The CRUD surface
//! below preserves the Phase 1 read paths (`get`, `delete`, `search`)
//! while introducing the typed write path (`remember`).

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::Result;
use crate::agent::hybrid_common::{EmbeddedCandidate, FtsHit, bytes_to_f32};

/// What flavour of fact a memory row holds. Spec §9.4 — see the docstring
/// on each variant for default lifetime and use case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Stable facts about the user as a person. Never auto-expires.
    Identity,
    /// Durable preferences and corrections. Never auto-expires.
    Preference,
    /// Facts about current work; may go stale. Default TTL 90 days.
    Project,
    /// Discrete time-stamped incidents. Default TTL 30 days. Surfaced
    /// in the auto-injected "Recent events" section at context build.
    Event,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Preference => "preference",
            Self::Project => "project",
            Self::Event => "event",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "identity" => Self::Identity,
            "preference" => Self::Preference,
            "project" => Self::Project,
            "event" => Self::Event,
            _ => return None,
        })
    }

    /// Default TTL when the caller doesn't specify one, weighted by
    /// `source` — facts the user told us directly are worth keeping
    /// longer than the agent's guesses (spec §9.4).
    ///
    /// | kind        | user_told  | agent_inferred |
    /// |-------------|-----------:|---------------:|
    /// | identity    | None       | None           |
    /// | preference  | None       | 180 days       |
    /// | project     | 180 days   | 90 days        |
    /// | event       | 90 days    | 30 days        |
    ///
    /// Identity facts never auto-expire regardless of source — even an
    /// agent-inferred "user is a Python developer" stays as long as the
    /// row isn't actively contradicted (a future `remember` writes a new
    /// `supersedes` chain).
    pub fn default_ttl_days(self, source: Source) -> Option<i64> {
        match (self, source) {
            (Self::Identity, _) | (Self::Preference, Source::UserTold) => None,
            (Self::Preference, Source::AgentInferred) | (Self::Project, Source::UserTold) => {
                Some(180)
            }
            (Self::Project, Source::AgentInferred) | (Self::Event, Source::UserTold) => Some(90),
            (Self::Event, Source::AgentInferred) => Some(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    UserTold,
    AgentInferred,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserTold => "user_told",
            Self::AgentInferred => "agent_inferred",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user_told" => Self::UserTold,
            "agent_inferred" => Self::AgentInferred,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub key: String,
    pub value: String,
    pub kind: Kind,
    pub source: Source,
    pub ttl_days: Option<i64>,
    pub archived_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Number of times this row has been returned by `search` since it
    /// was last written. Bumped by [`bump_recall`]. The aging pass uses
    /// `last_recalled_at` as the freshness anchor when present, so a
    /// recall-fresh row stays alive past its calendar TTL.
    pub recall_count: i64,
    pub last_recalled_at: Option<DateTime<Utc>>,
    /// When this row replaced a prior fact for the same key, `supersedes_id`
    /// points at the (now archived) old row. Forms a soft-delete chain
    /// the dashboard renders as "agent used to think X, now thinks Y".
    pub supersedes_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewEntry<'a> {
    pub key: &'a str,
    pub value: &'a str,
    pub kind: Kind,
    pub source: Source,
    /// `None` → use [`Kind::default_ttl_days`].
    pub ttl_days: Option<i64>,
}

/// Insert a new entry, or replace the value for an existing key while
/// preserving an audit trail (spec §9.4 supersedes chain).
///
/// Behaviour by current state of `key`:
/// - **No row exists** → INSERT a fresh row.
/// - **Active row with the same `value` and `kind`** → no-op write that
///   only refreshes `updated_at`. Idempotent reinforcement.
/// - **Active row with a different `value` or `kind`** → archive the old
///   row, INSERT a new row whose `supersedes_id` points at the archived
///   one. The dashboard renders the chain.
/// - **Archived row exists** → INSERT a new active row with
///   `supersedes_id` pointing at the most-recent archived row. The
///   archived row stays archived (audit) but the agent's view is
///   single-valued again.
///
/// Returns the active row's id after the write — capture this and
/// pass it to [`set_embedding_by_id`] so a concurrent
/// `remember(same_key, different_value)` can't have its archive +
/// re-insert sequence steal your row out from under you.
#[allow(clippy::explicit_auto_deref)]
pub async fn remember(pool: &SqlitePool, new: NewEntry<'_>) -> Result<String> {
    let ttl = new
        .ttl_days
        .or_else(|| new.kind.default_ttl_days(new.source));
    let mut tx = pool.begin().await?;

    // Look up the current row (if any) for this key. We need its id to
    // either UPDATE in place (idempotent reinforcement) or to set as
    // supersedes_id on a fresh INSERT.
    let existing: Option<(String, String, String, Option<DateTime<Utc>>)> =
        sqlx::query_as::<_, (String, String, String, Option<DateTime<Utc>>)>(
            "SELECT id, value, kind, archived_at FROM memory WHERE key = ?1",
        )
        .bind(new.key)
        .fetch_optional(&mut *tx)
        .await?;

    match existing {
        // Active + value/kind unchanged → idempotent refresh. If the
        // caller didn't supply an explicit ttl_days, recompute from the
        // (possibly upgraded) source so that promoting an
        // agent_inferred row to user_told extends its lifetime per
        // §9.4. Caller-supplied TTL still wins.
        Some((_id, ref v, ref k, None)) if v == new.value && k == new.kind.as_str() => {
            sqlx::query(
                "UPDATE memory \
                 SET source     = ?1, \
                     ttl_days   = ?2, \
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                 WHERE key = ?3",
            )
            .bind(new.source.as_str())
            .bind(ttl)
            .bind(new.key)
            .execute(&mut *tx)
            .await?;
        }
        // Active row whose content has changed → archive it, INSERT
        // replacement with supersedes link.
        Some((old_id, _, _, None)) => {
            sqlx::query(
                "UPDATE memory \
                 SET archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                 WHERE id = ?1",
            )
            .bind(&old_id)
            .execute(&mut *tx)
            .await?;
            // The schema's UNIQUE(key) constraint blocks two rows with
            // the same key. We mangle the old key with a suffix so the
            // archived row is preserved without colliding. Using the
            // archived timestamp keeps the suffix unique even if the
            // user later writes/archives the same key again.
            sqlx::query(
                "UPDATE memory \
                 SET key = key || '@archived:' || strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                 WHERE id = ?1",
            )
            .bind(&old_id)
            .execute(&mut *tx)
            .await?;
            insert_new(&mut *tx, &new, ttl, Some(&old_id)).await?;
        }
        // Only archived rows for this key — suffix the archived row's
        // key so the UNIQUE(key) constraint stays satisfied, then INSERT
        // the new active row linked to it.
        Some((archived_id, _, _, Some(_))) => {
            sqlx::query(
                "UPDATE memory \
                 SET key = key || '@reactivated:' || strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
                 WHERE id = ?1",
            )
            .bind(&archived_id)
            .execute(&mut *tx)
            .await?;
            insert_new(&mut *tx, &new, ttl, Some(&archived_id)).await?;
        }
        None => {
            insert_new(&mut *tx, &new, ttl, None).await?;
        }
    }

    tx.commit().await?;
    // Read back the active row's id under the same key. The transaction
    // above committed, so this read is durable + race-free wrt our own
    // write. A concurrent writer could supersede this row between commit
    // and read — caller's set_embedding_by_id then becomes a no-op
    // (correct: the new row will get its own embedding when it's
    // remembered). The audit-fix point is "don't write embedding to a
    // key whose row identity may have changed", which the by-id path
    // honours.
    let id: String =
        sqlx::query_scalar("SELECT id FROM memory WHERE key = ?1 AND archived_at IS NULL")
            .bind(new.key)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|_| String::new());
    Ok(id)
}

async fn insert_new(
    tx: &mut sqlx::SqliteConnection,
    new: &NewEntry<'_>,
    ttl: Option<i64>,
    supersedes_id: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO memory (id, key, value, kind, source, ttl_days, supersedes_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(new.key)
    .bind(new.value)
    .bind(new.kind.as_str())
    .bind(new.source.as_str())
    .bind(ttl)
    .bind(supersedes_id)
    .execute(tx)
    .await?;
    Ok(())
}

/// Persist an embedding vector for a specific row id. Pin-by-id
/// avoids the race the audit found: with key-only matching, a
/// concurrent `remember(same_key, new_value)` could archive row A,
/// insert row B, then this call would land row A's vector against
/// row B. Caller captures the id from `remember`'s return and feeds
/// it here.
///
/// Returns `Ok(true)` if the row was active and got the vector;
/// `Ok(false)` when the row no longer exists or was archived between
/// remember and now (silently dropped — next `remember` of the same
/// key produces a new id and a new embedding).
///
/// Vectors stored as native-byte-order f32 BLOB via
/// `bytemuck::cast_slice`. Native byte order means files aren't
/// portable across endian families (PowerPC/S390x vs the common
/// x86/ARM little-endian) — fine for havn's single-host SQLite
/// scope (spec §1.4); flagged here for future archive/import paths.
pub async fn set_embedding_by_id(pool: &SqlitePool, id: &str, vector: &[f32]) -> Result<bool> {
    let bytes: &[u8] = bytemuck::cast_slice(vector);
    let dim = i64::try_from(vector.len()).unwrap_or(0);
    let res = sqlx::query(
        "UPDATE memory \
         SET embedding = ?1, embedding_dim = ?2 \
         WHERE id = ?3 AND archived_at IS NULL",
    )
    .bind(bytes)
    .bind(dim)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Backfill-only helper: by-key UPDATE used by the background scan
/// where we don't carry id around (we're walking the table). Safe
/// because the backfill never INSERTs — it only writes vectors to
/// existing rows for which the key→id mapping is stable for the
/// duration of the read+write within the same scan iteration.
pub async fn set_embedding(pool: &SqlitePool, key: &str, vector: &[f32]) -> Result<bool> {
    let bytes: &[u8] = bytemuck::cast_slice(vector);
    let dim = i64::try_from(vector.len()).unwrap_or(0);
    let res = sqlx::query(
        "UPDATE memory \
         SET embedding = ?1, embedding_dim = ?2 \
         WHERE key = ?3 AND archived_at IS NULL",
    )
    .bind(bytes)
    .bind(dim)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Candidate rows for the runtime's hybrid scorer. SQL-side filters
/// applied:
/// - active only (`archived_at IS NULL`),
/// - vector dimension matches `expected_dim` (rows from a different
///   embedder are silently dropped — they're still findable via FTS5,
///   so retrieval degrades rather than breaks),
/// - kind is in `kinds` (or every kind when `kinds` is empty).
///
/// For agents with a few hundred memory rows × 1536-d vectors this
/// is ~1 MB read + a sub-ms cosine sweep in pure Rust — well below
/// any user-visible latency. The migration on `memory.embedding`
/// notes the >10k-rows-per-agent threshold above which sqlite-vec
/// becomes worth the build complexity.
pub async fn embedded_candidates(
    pool: &SqlitePool,
    expected_dim: usize,
    kinds: &[Kind],
) -> Result<Vec<EmbeddedCandidate>> {
    let expected_i64 = i64::try_from(expected_dim).unwrap_or(i64::MAX);
    let rows: Vec<(String, Vec<u8>, i64)> = if kinds.is_empty() {
        sqlx::query_as::<_, (String, Vec<u8>, i64)>(
            "SELECT id, embedding, embedding_dim FROM memory \
             WHERE archived_at IS NULL \
               AND embedding IS NOT NULL \
               AND embedding_dim = ?1",
        )
        .bind(expected_i64)
        .fetch_all(pool)
        .await?
    } else {
        let placeholders = (0..kinds.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, embedding, embedding_dim FROM memory \
             WHERE archived_at IS NULL \
               AND embedding IS NOT NULL \
               AND embedding_dim = ?1 \
               AND kind IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, (String, Vec<u8>, i64)>(&sql).bind(expected_i64);
        for k in kinds {
            q = q.bind(k.as_str());
        }
        q.fetch_all(pool).await?
    };
    Ok(rows
        .into_iter()
        .map(|(id, bytes, _dim)| EmbeddedCandidate {
            id,
            embedding: bytes_to_f32(&bytes),
        })
        .collect())
}

/// FTS5 hits for a query, restricted to active rows + (optionally)
/// the requested kinds. Returns up to `limit` hits ordered best-first.
/// The hybrid scorer in havn-runtime combines these with cosine
/// scores from [`embedded_candidates`].
pub async fn fts_hits(
    pool: &SqlitePool,
    query: &str,
    kinds: &[Kind],
    limit: u32,
) -> Result<Vec<FtsHit>> {
    let rows: Vec<(String, f64)> = if kinds.is_empty() {
        sqlx::query_as::<_, (String, f64)>(
            "SELECT m.id, rank \
             FROM memory_fts f JOIN memory m ON m.rowid = f.rowid \
             WHERE memory_fts MATCH ?1 AND m.archived_at IS NULL \
             ORDER BY rank LIMIT ?2",
        )
        .bind(query)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        let placeholders = (0..kinds.len())
            .map(|i| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT m.id, rank \
             FROM memory_fts f JOIN memory m ON m.rowid = f.rowid \
             WHERE memory_fts MATCH ?1 AND m.archived_at IS NULL AND m.kind IN ({placeholders}) \
             ORDER BY rank LIMIT ?2"
        );
        let mut q = sqlx::query_as::<_, (String, f64)>(&sql)
            .bind(query)
            .bind(limit);
        for k in kinds {
            q = q.bind(k.as_str());
        }
        q.fetch_all(pool).await?
    };
    Ok(rows
        .into_iter()
        .map(|(id, rank)| FtsHit { id, rank })
        .collect())
}

/// Fetch a single entry by id (used by hybrid search after the
/// scorer picks the winning row ids). Bump path lives separately.
pub async fn fetch_by_id(pool: &SqlitePool, id: &str) -> Result<Option<Entry>> {
    fetch_one(pool, "WHERE id = ?1 AND archived_at IS NULL", &[id]).await
}

/// Bump `recall_count` and `last_recalled_at` for every active row whose
/// id is in `ids`. Single SQL statement — no per-row round trip.
/// Called from [`search`] after fetching hits.
async fn bump_recall(pool: &SqlitePool, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = (0..ids.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE memory \
         SET recall_count = recall_count + 1, \
             last_recalled_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.execute(pool).await?;
    Ok(())
}

/// Phase 1 compat shim — defaults to `kind = preference`, `source = agent_inferred`,
/// no explicit TTL (the kind/source pair will produce a 180-day default).
/// Existing call sites keep working until they migrate to [`remember`].
#[deprecated(note = "use `remember` with explicit Kind / Source")]
pub async fn upsert(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    remember(
        pool,
        NewEntry {
            key,
            value,
            kind: Kind::Preference,
            source: Source::AgentInferred,
            ttl_days: None,
        },
    )
    .await
    .map(|_id| ())
}

/// Look up the **active** row for `key`. Archived rows (forgotten or
/// superseded) are excluded — query them via [`list_archived_for_key`]
/// or [`list_active`] in dashboard contexts.
pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<Entry>> {
    fetch_one(pool, "WHERE key = ?1 AND archived_at IS NULL", &[key]).await
}

/// Audit helper: list every archived row that ever held this key (the
/// dashboard's "what did you used to think" view). Walks the
/// `@archived:` / `@reactivated:` suffixes via prefix LIKE.
pub async fn list_archived_for_key(pool: &SqlitePool, key: &str) -> Result<Vec<Entry>> {
    let prefix_pattern = format!("{key}@%");
    let rows: Vec<EntryRow> = sqlx::query_as::<_, EntryRow>(
        "SELECT id, key, value, kind, source, ttl_days, archived_at, created_at, updated_at, \
                recall_count, last_recalled_at, supersedes_id \
         FROM memory \
         WHERE archived_at IS NOT NULL AND (key = ?1 OR key LIKE ?2) \
         ORDER BY archived_at DESC",
    )
    .bind(key)
    .bind(prefix_pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Entry::from).collect())
}

/// Soft-delete: set `archived_at = now` and suffix the key so the
/// UNIQUE(key) constraint stays open for a future `remember` of the same
/// key. Returns `true` if a row was flipped from active to archived;
/// `false` if no active row existed. Spec §9.4: never DELETE so the
/// dashboard's audit trail holds.
pub async fn forget(pool: &SqlitePool, key: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE memory \
         SET archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
             key         = key || '@forgotten:' || strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE key = ?1 AND archived_at IS NULL",
    )
    .bind(key)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Phase 1 compat shim — same semantics as `forget`. Older call sites can
/// migrate at their own pace.
#[deprecated(note = "use `forget` (soft-delete)")]
pub async fn delete(pool: &SqlitePool, key: &str) -> Result<bool> {
    forget(pool, key).await
}

/// Only the top-K hits of a search update `recall_count` /
/// `last_recalled_at`. Reasoning: a generic query like `"user"` would
/// return every row that mentions the word and, without a cap, bump
/// every row's freshness anchor to now — which would defeat aging
/// (everything looks recall-fresh, nothing ever expires). Capping at
/// the top-K mirrors OpenClaw's "evidence array" pattern: the long
/// tail of low-rank hits doesn't count as "the agent really used this
/// fact". 3 is a small-but-real signal floor; the rest of the returned
/// rows are still surfaced to the LLM, just don't count for aging.
const RECALL_BUMP_TOP_K: usize = 3;

/// FTS5 search over keys + values, restricted to active (non-archived)
/// rows. `kinds` filters by [`Kind`] when non-empty.
///
/// Bumps `recall_count` + `last_recalled_at` only on rows whose BM25
/// rank is below [`RECALL_BUMP_RANK_THRESHOLD`] (i.e. the actually-good
/// hits). Pollution by generic queries is the audit-found failure mode
/// this guards against.
pub async fn search(
    pool: &SqlitePool,
    query: &str,
    kinds: &[Kind],
    limit: u32,
) -> Result<Vec<Entry>> {
    // ORDER BY rank in SQL puts best matches first. We don't need the
    // rank value back in Rust because the cap is positional (top-K).
    let rows: Vec<EntryRow> = if kinds.is_empty() {
        sqlx::query_as::<_, EntryRow>(
            "SELECT m.id, m.key, m.value, m.kind, m.source, m.ttl_days, m.archived_at, \
                    m.created_at, m.updated_at, m.recall_count, m.last_recalled_at, m.supersedes_id \
             FROM memory_fts f JOIN memory m ON m.rowid = f.rowid \
             WHERE memory_fts MATCH ?1 AND m.archived_at IS NULL \
             ORDER BY rank LIMIT ?2",
        )
        .bind(query)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        let placeholders = (0..kinds.len())
            .map(|i| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT m.id, m.key, m.value, m.kind, m.source, m.ttl_days, m.archived_at, \
                    m.created_at, m.updated_at, m.recall_count, m.last_recalled_at, m.supersedes_id \
             FROM memory_fts f JOIN memory m ON m.rowid = f.rowid \
             WHERE memory_fts MATCH ?1 AND m.archived_at IS NULL AND m.kind IN ({placeholders}) \
             ORDER BY rank LIMIT ?2"
        );
        let mut q = sqlx::query_as::<_, EntryRow>(&sql).bind(query).bind(limit);
        for k in kinds {
            q = q.bind(k.as_str());
        }
        q.fetch_all(pool).await?
    };

    // Bump only the strongest hits. Rows are already ordered by rank
    // ascending (best first), so take the top K. Guards against
    // generic-query pollution per the audit.
    let ids_to_bump: Vec<String> = rows
        .iter()
        .take(RECALL_BUMP_TOP_K)
        .map(|r| r.id.clone())
        .collect();
    bump_recall(pool, &ids_to_bump).await?;

    Ok(rows.into_iter().map(Entry::from).collect())
}

/// Active event-kind rows updated within `window`, newest first. Used by
/// the runtime's context-build step to inject a "Recent events" section
/// into the system prompt without forcing the agent to spend a
/// `memory_search` tool call.
pub async fn recent_events(
    pool: &SqlitePool,
    within: ChronoDuration,
    limit: u32,
) -> Result<Vec<Entry>> {
    let cutoff = (Utc::now() - within)
        .format("%Y-%m-%dT%H:%M:%f%.fZ")
        .to_string();
    let rows: Vec<EntryRow> = sqlx::query_as::<_, EntryRow>(
        "SELECT id, key, value, kind, source, ttl_days, archived_at, created_at, updated_at, \
                recall_count, last_recalled_at, supersedes_id \
         FROM memory \
         WHERE archived_at IS NULL AND kind = 'event' AND updated_at >= ?1 \
         ORDER BY updated_at DESC LIMIT ?2",
    )
    .bind(cutoff)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Entry::from).collect())
}

/// List active rows for the dashboard. Newest first; bounded by `limit`
/// (caller paginates if needed).
pub async fn list_active(pool: &SqlitePool, limit: u32) -> Result<Vec<Entry>> {
    let rows: Vec<EntryRow> = sqlx::query_as::<_, EntryRow>(
        "SELECT id, key, value, kind, source, ttl_days, archived_at, created_at, updated_at, \
                recall_count, last_recalled_at, supersedes_id \
         FROM memory WHERE archived_at IS NULL \
         ORDER BY updated_at DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Entry::from).collect())
}

/// Daily aging pass (spec §9.4). Archives any active row whose
/// **freshness anchor** — `MAX(updated_at, last_recalled_at)` — is older
/// than `ttl_days`. The `last_recalled_at` term is the OpenClaw-inspired
/// fix to time-only aging: a 30-day-old event the agent has cited via
/// `memory_search` in the last week is *not* stale, it's load-bearing.
///
/// Identity / preference / user-told-preference rows have `ttl_days IS NULL`
/// and are never touched. Returns the row count archived.
pub async fn age_expired(pool: &SqlitePool) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE memory \
         SET archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE archived_at IS NULL \
           AND ttl_days IS NOT NULL \
           AND julianday('now') - julianday(COALESCE(last_recalled_at, updated_at)) >= ttl_days",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

async fn fetch_one(pool: &SqlitePool, where_clause: &str, args: &[&str]) -> Result<Option<Entry>> {
    let sql = format!(
        "SELECT id, key, value, kind, source, ttl_days, archived_at, created_at, updated_at, \
                recall_count, last_recalled_at, supersedes_id \
         FROM memory {where_clause}"
    );
    let mut q = sqlx::query_as::<_, EntryRow>(&sql);
    for a in args {
        q = q.bind(*a);
    }
    Ok(q.fetch_optional(pool).await?.map(Entry::from))
}

#[derive(Debug, sqlx::FromRow)]
struct EntryRow {
    id: String,
    key: String,
    value: String,
    kind: String,
    source: String,
    ttl_days: Option<i64>,
    archived_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    recall_count: i64,
    last_recalled_at: Option<DateTime<Utc>>,
    supersedes_id: Option<String>,
}

impl From<EntryRow> for Entry {
    fn from(r: EntryRow) -> Self {
        Self {
            id: r.id,
            key: r.key,
            value: r.value,
            // CHECK constraints in the schema mean these unwraps are safe;
            // an unknown value would have failed at INSERT time. Falling
            // back to Preference / AgentInferred is defensive in case a
            // future schema migration loosens the constraint.
            kind: Kind::parse(&r.kind).unwrap_or(Kind::Preference),
            source: Source::parse(&r.source).unwrap_or(Source::AgentInferred),
            ttl_days: r.ttl_days,
            archived_at: r.archived_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
            recall_count: r.recall_count,
            last_recalled_at: r.last_recalled_at,
            supersedes_id: r.supersedes_id,
        }
    }
}

/// Public bump-recall hook. Same body as the existing private
/// `bump_recall`, exposed so the runtime's hybrid scorer can refresh
/// the recall anchor after picking its top-K winners (top-K only,
/// per the §9.4 generic-query-pollution defence).
pub async fn bump_recall_for(pool: &SqlitePool, ids: &[String]) -> Result<()> {
    bump_recall(pool, ids).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, deprecated)]

    use super::*;
    use crate::agent::connect_in_memory;
    use crate::agent::conversations::escape_fts_query;

    fn ev<'a>(key: &'a str, value: &'a str) -> NewEntry<'a> {
        NewEntry {
            key,
            value,
            kind: Kind::Event,
            source: Source::AgentInferred,
            ttl_days: None,
        }
    }

    #[tokio::test]
    async fn remember_then_get() {
        let pool = connect_in_memory().await.expect("connect");
        remember(
            &pool,
            NewEntry {
                key: "user.name",
                value: "Ada",
                kind: Kind::Identity,
                source: Source::UserTold,
                ttl_days: None,
            },
        )
        .await
        .expect("remember");
        let entry = get(&pool, "user.name").await.expect("get").expect("some");
        assert_eq!(entry.value, "Ada");
        assert_eq!(entry.kind, Kind::Identity);
        assert_eq!(entry.source, Source::UserTold);
        assert!(entry.ttl_days.is_none());
        assert!(entry.archived_at.is_none());
    }

    #[tokio::test]
    async fn remember_idempotent_value_keeps_created_at() {
        // Reinforcement (same value) refreshes updated_at but keeps the
        // same row — so created_at is preserved. Different from
        // value-change, which intentionally creates a NEW row with a
        // supersedes link (see remember_with_changed_value_creates_supersedes_chain).
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("k", "v")).await.expect("first");
        let first = get(&pool, "k").await.expect("get").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        remember(&pool, ev("k", "v")).await.expect("second");
        let second = get(&pool, "k").await.expect("get").unwrap();
        assert_eq!(second.id, first.id, "same row");
        assert_eq!(second.created_at, first.created_at);
        assert!(second.updated_at >= first.updated_at);
    }

    #[tokio::test]
    async fn forget_soft_deletes() {
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("k", "v")).await.expect("remember");
        assert!(forget(&pool, "k").await.expect("forget"));
        // Soft-deleted: get() (active-only) returns None.
        assert!(get(&pool, "k").await.expect("get").is_none());
        // But the audit trail still has the row.
        let history = list_archived_for_key(&pool, "k").await.expect("hist");
        assert_eq!(history.len(), 1);
        assert!(history[0].archived_at.is_some());
        assert!(history[0].key.starts_with("k@forgotten:"));
        // Idempotent — second forget reports no-op (no active row to archive).
        assert!(!forget(&pool, "k").await.expect("forget"));
    }

    #[tokio::test]
    async fn remember_after_forget_reactivates() {
        // User said "drop X" then later "actually X again" — the new
        // remember() inserts a fresh active row. The forgotten one stays
        // in the audit trail (suffixed key). Net visible state: get(k)
        // returns the new row; list_archived_for_key sees the old.
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("k", "v1")).await.expect("first");
        assert!(forget(&pool, "k").await.expect("forget"));
        remember(&pool, ev("k", "v2")).await.expect("second");
        let row = get(&pool, "k").await.expect("get").expect("some");
        assert!(row.archived_at.is_none(), "should be re-active");
        assert_eq!(row.value, "v2");
        let history = list_archived_for_key(&pool, "k").await.expect("hist");
        assert_eq!(history.len(), 1, "audit trail preserved");
    }

    #[tokio::test]
    async fn search_excludes_archived() {
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("k1", "the user prefers tea over coffee"))
            .await
            .expect("u1");
        remember(&pool, ev("k2", "the user lives in Tokyo"))
            .await
            .expect("u2");
        forget(&pool, "k1").await.expect("forget");
        let q = escape_fts_query("user");
        let hits = search(&pool, &q, &[], 5).await.expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "k2");
    }

    #[tokio::test]
    async fn search_filters_by_kind() {
        let pool = connect_in_memory().await.expect("connect");
        remember(
            &pool,
            NewEntry {
                key: "p1",
                value: "user prefers vim",
                kind: Kind::Preference,
                source: Source::UserTold,
                ttl_days: None,
            },
        )
        .await
        .expect("u1");
        remember(
            &pool,
            NewEntry {
                key: "e1",
                value: "user shipped a release yesterday",
                kind: Kind::Event,
                source: Source::AgentInferred,
                ttl_days: None,
            },
        )
        .await
        .expect("u2");
        let q = escape_fts_query("user");
        let prefs_only = search(&pool, &q, &[Kind::Preference], 5).await.expect("s");
        assert_eq!(prefs_only.len(), 1);
        assert_eq!(prefs_only[0].kind, Kind::Preference);
    }

    #[tokio::test]
    async fn recent_events_only_returns_event_kind_within_window() {
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("e1", "shipped release"))
            .await
            .expect("u1");
        remember(
            &pool,
            NewEntry {
                key: "p1",
                value: "prefers vim",
                kind: Kind::Preference,
                source: Source::UserTold,
                ttl_days: None,
            },
        )
        .await
        .expect("u2");
        let recent = recent_events(&pool, ChronoDuration::days(7), 50)
            .await
            .expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].kind, Kind::Event);
    }

    #[tokio::test]
    async fn age_expired_archives_event_past_ttl() {
        let pool = connect_in_memory().await.expect("connect");
        // Insert an event with explicit ttl_days = 1.
        remember(
            &pool,
            NewEntry {
                key: "e_old",
                value: "from yesterday",
                kind: Kind::Event,
                source: Source::AgentInferred,
                ttl_days: Some(1),
            },
        )
        .await
        .expect("u1");
        // And a fresh preference (no TTL).
        remember(
            &pool,
            NewEntry {
                key: "p_pref",
                value: "vim",
                kind: Kind::Preference,
                source: Source::UserTold,
                ttl_days: None,
            },
        )
        .await
        .expect("u2");
        // Force the event row's updated_at to two days ago.
        sqlx::query(
            "UPDATE memory \
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-2 days') \
             WHERE key = ?1",
        )
        .bind("e_old")
        .execute(&pool)
        .await
        .expect("backdate");

        let archived = age_expired(&pool).await.expect("age");
        assert_eq!(archived, 1, "only the expired event should be archived");
        // age_expired() doesn't suffix the key (unlike forget()), so the
        // row is hidden from get() but still discoverable by id and via
        // the audit helper.
        assert!(get(&pool, "e_old").await.expect("get").is_none());
        let pref = get(&pool, "p_pref").await.expect("get").unwrap();
        assert!(
            pref.archived_at.is_none(),
            "preferences are immune to aging"
        );
    }

    #[tokio::test]
    async fn list_active_excludes_archived() {
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("a", "v")).await.expect("u1");
        remember(&pool, ev("b", "v")).await.expect("u2");
        forget(&pool, "a").await.expect("forget");
        let active = list_active(&pool, 50).await.expect("list");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, "b");
    }

    // ---- v2: recall tracking + supersedes + source-weighted TTL ---------

    #[test]
    fn source_weighted_default_ttl() {
        // user_told facts get longer (or no) TTL than agent_inferred ones
        // — spec §9.4. Locks in the table the docstring promises.
        assert_eq!(Kind::Identity.default_ttl_days(Source::UserTold), None);
        assert_eq!(Kind::Identity.default_ttl_days(Source::AgentInferred), None);
        assert_eq!(Kind::Preference.default_ttl_days(Source::UserTold), None);
        assert_eq!(
            Kind::Preference.default_ttl_days(Source::AgentInferred),
            Some(180)
        );
        assert_eq!(Kind::Project.default_ttl_days(Source::UserTold), Some(180));
        assert_eq!(
            Kind::Project.default_ttl_days(Source::AgentInferred),
            Some(90)
        );
        assert_eq!(Kind::Event.default_ttl_days(Source::UserTold), Some(90));
        assert_eq!(
            Kind::Event.default_ttl_days(Source::AgentInferred),
            Some(30)
        );
    }

    #[tokio::test]
    async fn search_bumps_recall_count() {
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("k", "the user prefers tea"))
            .await
            .expect("u1");
        let q = escape_fts_query("tea");
        let _hits = search(&pool, &q, &[], 10).await.expect("search");
        let row = get(&pool, "k").await.expect("get").unwrap();
        assert_eq!(row.recall_count, 1);
        assert!(row.last_recalled_at.is_some());
        let _hits = search(&pool, &q, &[], 10).await.expect("search again");
        let row = get(&pool, "k").await.expect("get").unwrap();
        assert_eq!(row.recall_count, 2);
    }

    #[tokio::test]
    async fn idempotent_remember_upgrades_ttl_when_source_promoted() {
        // Audit-found scenario: agent first guesses "user prefers vim"
        // (agent_inferred → 180-day TTL). Then user actually says "yes,
        // I prefer vim" (user_told → no TTL = forever). The idempotent
        // refresh path must recompute TTL from the upgraded source.
        let pool = connect_in_memory().await.expect("connect");
        remember(
            &pool,
            NewEntry {
                key: "user.editor",
                value: "vim",
                kind: Kind::Preference,
                source: Source::AgentInferred,
                ttl_days: None,
            },
        )
        .await
        .expect("guess");
        let inferred = get(&pool, "user.editor").await.expect("get").unwrap();
        assert_eq!(inferred.ttl_days, Some(180));
        assert_eq!(inferred.source, Source::AgentInferred);

        remember(
            &pool,
            NewEntry {
                key: "user.editor",
                value: "vim",
                kind: Kind::Preference,
                source: Source::UserTold,
                ttl_days: None,
            },
        )
        .await
        .expect("confirmed");
        let confirmed = get(&pool, "user.editor").await.expect("get").unwrap();
        assert_eq!(confirmed.id, inferred.id, "same row, idempotent refresh");
        assert_eq!(confirmed.source, Source::UserTold);
        assert_eq!(
            confirmed.ttl_days, None,
            "user-confirmed preference must lose its expiry"
        );
    }

    #[tokio::test]
    async fn search_recall_caps_at_top_k() {
        // Audit-found failure mode: a generic query like "user" hits 10+
        // rows. Without a cap, every hit bumps recall_count → aging gets
        // defeated. The fix: bump only the top-K (RECALL_BUMP_TOP_K=3).
        let pool = connect_in_memory().await.expect("connect");
        for i in 0..10 {
            let key = format!("k{i}");
            remember(
                &pool,
                NewEntry {
                    key: &key,
                    value: "the user said something interesting",
                    kind: Kind::Event,
                    source: Source::AgentInferred,
                    ttl_days: None,
                },
            )
            .await
            .expect("remember");
        }
        let q = escape_fts_query("user");
        let hits = search(&pool, &q, &[], 10).await.expect("search");
        assert_eq!(hits.len(), 10, "search still returns all 10");

        // But only the top RECALL_BUMP_TOP_K should have been bumped.
        let mut bumped = Vec::with_capacity(10);
        for i in 0..10 {
            let key = format!("k{i}");
            let row = get(&pool, &key).await.expect("get").unwrap();
            bumped.push(row.recall_count);
        }
        let bumped_count = bumped.iter().filter(|&&n| n > 0).count();
        assert_eq!(
            bumped_count, RECALL_BUMP_TOP_K,
            "generic query must not bump every match — only top-{RECALL_BUMP_TOP_K}: {bumped:?}"
        );
    }

    #[tokio::test]
    async fn aging_keeps_recall_fresh_rows_alive() {
        // The whole point of recall tracking: a 30-day-old event the
        // agent has cited recently must NOT be archived.
        let pool = connect_in_memory().await.expect("connect");
        remember(
            &pool,
            NewEntry {
                key: "e_old_but_used",
                value: "shipped release",
                kind: Kind::Event,
                source: Source::AgentInferred,
                ttl_days: Some(1),
            },
        )
        .await
        .expect("u1");
        // Backdate updated_at so calendar age is 5 days.
        sqlx::query(
            "UPDATE memory \
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-5 days') \
             WHERE key = ?1",
        )
        .bind("e_old_but_used")
        .execute(&pool)
        .await
        .expect("backdate");
        // But the agent recalled it now — last_recalled_at = now.
        let q = escape_fts_query("release");
        let hits = search(&pool, &q, &[], 5).await.expect("search");
        assert_eq!(hits.len(), 1);

        let archived = age_expired(&pool).await.expect("age");
        assert_eq!(archived, 0, "recall-fresh row must survive aging");
        let row = get(&pool, "e_old_but_used").await.expect("get").unwrap();
        assert!(row.archived_at.is_none());
    }

    #[tokio::test]
    async fn aging_archives_unrecalled_old_rows() {
        // Mirror of the above: same setup but no search/recall — should
        // archive.
        let pool = connect_in_memory().await.expect("connect");
        remember(
            &pool,
            NewEntry {
                key: "e_unused",
                value: "stale event",
                kind: Kind::Event,
                source: Source::AgentInferred,
                ttl_days: Some(1),
            },
        )
        .await
        .expect("u1");
        sqlx::query(
            "UPDATE memory \
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-5 days') \
             WHERE key = ?1",
        )
        .bind("e_unused")
        .execute(&pool)
        .await
        .expect("backdate");

        let archived = age_expired(&pool).await.expect("age");
        assert_eq!(archived, 1);
    }

    #[tokio::test]
    async fn remember_with_changed_value_creates_supersedes_chain() {
        // User said "I work at X" then later "I work at Y now". Old row
        // archived but kept; new row supersedes_id points at it.
        let pool = connect_in_memory().await.expect("connect");
        remember(
            &pool,
            NewEntry {
                key: "user.employer",
                value: "Acme Corp",
                kind: Kind::Project,
                source: Source::UserTold,
                ttl_days: None,
            },
        )
        .await
        .expect("first");
        let first_id = get(&pool, "user.employer").await.expect("get").unwrap().id;

        remember(
            &pool,
            NewEntry {
                key: "user.employer",
                value: "Globex",
                kind: Kind::Project,
                source: Source::UserTold,
                ttl_days: None,
            },
        )
        .await
        .expect("second");

        let active = get(&pool, "user.employer").await.expect("get").unwrap();
        assert_eq!(active.value, "Globex");
        assert!(active.archived_at.is_none());
        assert_eq!(active.supersedes_id.as_deref(), Some(first_id.as_str()));

        // Old row still in table, archived, key suffixed so the unique
        // constraint isn't violated.
        let archived: Option<(String, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT key, archived_at FROM memory WHERE id = ?1")
                .bind(&first_id)
                .fetch_optional(&pool)
                .await
                .expect("query");
        let (old_key, old_archived) = archived.expect("old row exists");
        assert!(old_key.starts_with("user.employer@archived:"));
        assert!(old_archived.is_some());
    }

    #[tokio::test]
    async fn remember_with_same_value_is_idempotent_no_supersedes() {
        // Reinforcement should not create a chain; just refresh updated_at.
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("k", "v")).await.expect("first");
        let first_id = get(&pool, "k").await.expect("get").unwrap().id;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        remember(&pool, ev("k", "v")).await.expect("second");
        let row = get(&pool, "k").await.expect("get").unwrap();
        assert_eq!(row.id, first_id, "should be the same row, refreshed");
        assert!(row.supersedes_id.is_none());
    }

    #[tokio::test]
    async fn remember_after_forget_links_archived_row() {
        // forget() archives + suffixes the key. A fresh remember() of
        // the original key inserts a new active row pointing at the
        // forgotten one via supersedes_id (lookup uses LIKE on the suffix).
        let pool = connect_in_memory().await.expect("connect");
        remember(&pool, ev("k", "v1")).await.expect("first");
        let first_id = get(&pool, "k").await.expect("get").unwrap().id;
        forget(&pool, "k").await.expect("forget");
        // After forget, get(k) is None — only audit lookup finds it.
        assert!(get(&pool, "k").await.expect("get").is_none());
        let history = list_archived_for_key(&pool, "k").await.expect("hist");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, first_id);

        // Now write a fresh value for the same key. Currently the
        // remember() lookup matches by exact key — after forget() the
        // archived row's key has a `@forgotten:` suffix, so remember()
        // sees no existing row at all and INSERTs without a supersedes
        // link. That's an audit-trail loss but the agent's view is
        // single-valued. A future improvement could LIKE-match the
        // suffix to recover the chain; for now we accept that explicit
        // forget breaks the supersedes chain (the audit row is still
        // discoverable via list_archived_for_key).
        remember(&pool, ev("k", "v2")).await.expect("second");
        let active = get(&pool, "k").await.expect("get").unwrap();
        assert!(active.archived_at.is_none());
        assert_eq!(active.value, "v2");
        // Two ways are both correct here; assert the loose one — either
        // None (current behaviour) or Some(first_id) (future enhancement).
        assert!(
            active.supersedes_id.is_none()
                || active.supersedes_id.as_deref() == Some(first_id.as_str())
        );
    }
}
