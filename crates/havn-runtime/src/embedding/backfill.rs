//! Automatic embedding backfill — runs in the background after the
//! agent connects, scans for active memory rows with no embedding,
//! and embeds them in small batches. Converges silently; no CLI
//! command, no operator action required.
//!
//! Triggers:
//! - The runtime has an embedder configured (otherwise no-op).
//! - Some rows exist with `embedding IS NULL` (or with the wrong
//!   dim — operator switched providers; fixed via [`bump_dim`]).
//!
//! Cadence: one shot at startup. If the operator switches providers
//! mid-flight (config edit + reload) the next agent restart picks
//! up the new config and re-runs. Spec §1.6 design philosophy:
//! converge automatically rather than expose a knob.

use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{info, warn};

use super::EmbeddingProvider;

/// Batch size for embedder calls during backfill. Picked to balance
/// network round-trips (OpenAI charges per call too) against
/// memory pressure when materialising a batch's text + vectors.
const BATCH_SIZE: usize = 50;

/// Run a single backfill pass to convergence. Spawned as a tokio
/// task at runtime startup; logs progress, never aborts the agent
/// on failure.
pub async fn run_to_completion(pool: SqlitePool, embedder: Arc<dyn EmbeddingProvider>) {
    let dim = embedder.dimensions();
    info!(
        provider = embedder.name(),
        dim,
        batch = BATCH_SIZE,
        "memory embedding backfill starting"
    );
    loop {
        let batch = match next_batch(&pool, dim, BATCH_SIZE as i64).await {
            Ok(b) if b.is_empty() => {
                info!("memory embedding backfill: caught up");
                return;
            }
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "backfill: query failed; aborting");
                return;
            }
        };
        let n = batch.len();
        let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = match embedder.embed_batch(&texts).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "backfill: provider returned error; aborting");
                return;
            }
        };
        if vectors.len() != batch.len() {
            warn!(
                expected = batch.len(),
                got = vectors.len(),
                "backfill: provider returned wrong batch length; aborting"
            );
            return;
        }
        let mut written = 0u64;
        for ((key, _), vec) in batch.into_iter().zip(vectors.into_iter()) {
            match havn_db::agent::memory::set_embedding(&pool, &key, &vec).await {
                Ok(true) => written += 1,
                Ok(false) => {
                    // Row was archived/deleted between query and write.
                    // Skip silently — converges next pass.
                }
                Err(e) => {
                    warn!(key, error = %e, "backfill: set_embedding failed; skipping");
                }
            }
        }
        info!(batch = n, written, "backfill: batch persisted");
        // Tiny breath so we don't pin a CPU when the embedder is
        // synchronous (HRR / Local). For OpenAI this is essentially
        // free — the network round-trip dominates.
        tokio::task::yield_now().await;
    }
}

/// Pull the next batch of active rows that need an embedding. We
/// embed `"<key>: <value>"` to match `MemoryRememberTool`'s
/// behaviour — same input shape → same vector space.
async fn next_batch(
    pool: &SqlitePool,
    dim: usize,
    limit: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let dim_i64 = i64::try_from(dim).unwrap_or(i64::MAX);
    // "Needs embedding" = no vector OR a vector with the wrong dim
    // (operator switched providers). The latter case is rare but
    // matters: switching from openai (1536d) to hrr (1024d) without
    // backfill would mean stored vectors get filtered out by
    // `fetch_active_with_embeddings` and the row reverts to FTS-only.
    sqlx::query_as::<_, (String, String)>(
        "SELECT key, key || ': ' || value AS text \
         FROM memory \
         WHERE archived_at IS NULL \
           AND (embedding IS NULL OR embedding_dim IS NULL OR embedding_dim != ?1) \
         ORDER BY updated_at \
         LIMIT ?2",
    )
    .bind(dim_i64)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Skills-side analog of [`run_to_completion`] (spec §13 Phase 3).
/// Walks the `skills_index` table and embeds any active row that
/// doesn't yet carry a vector at the configured dim. Same converge-
/// quietly pattern as memory: spawned at agent startup, runs once,
/// logs at INFO when it catches up. Embeds `"<name>: <description>"`
/// to match `skill_manage`'s embed-on-write text — same vector space
/// for live writes and backfilled rows.
pub async fn skills_run_to_completion(pool: SqlitePool, embedder: Arc<dyn EmbeddingProvider>) {
    let dim = embedder.dimensions();
    info!(
        provider = embedder.name(),
        dim,
        batch = BATCH_SIZE,
        "skills embedding backfill starting"
    );
    loop {
        let batch = match skills_next_batch(&pool, dim, BATCH_SIZE as i64).await {
            Ok(b) if b.is_empty() => {
                info!("skills embedding backfill: caught up");
                return;
            }
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "skills backfill: query failed; aborting");
                return;
            }
        };
        let n = batch.len();
        let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = match embedder.embed_batch(&texts).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "skills backfill: provider returned error; aborting");
                return;
            }
        };
        if vectors.len() != batch.len() {
            warn!(
                expected = batch.len(),
                got = vectors.len(),
                "skills backfill: provider returned wrong batch length; aborting"
            );
            return;
        }
        let mut written = 0u64;
        for ((name, _), vec) in batch.into_iter().zip(vectors.into_iter()) {
            match havn_db::agent::skills_index::set_embedding(&pool, &name, &vec).await {
                Ok(true) => written += 1,
                Ok(false) => {
                    // Row was archived between query and write. Skip;
                    // the next pass converges (or the row stays
                    // archived, in which case it's correctly invisible
                    // to retrieval anyway).
                }
                Err(e) => {
                    warn!(name, error = %e, "skills backfill: set_embedding failed; skipping");
                }
            }
        }
        info!(batch = n, written, "skills backfill: batch persisted");
        tokio::task::yield_now().await;
    }
}

async fn skills_next_batch(
    pool: &SqlitePool,
    dim: usize,
    limit: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let dim_i64 = i64::try_from(dim).unwrap_or(i64::MAX);
    sqlx::query_as::<_, (String, String)>(
        "SELECT name, name || ': ' || description AS text \
         FROM skills_index \
         WHERE archived_at IS NULL \
           AND (embedding IS NULL OR embedding_dim IS NULL OR embedding_dim != ?1) \
         ORDER BY name \
         LIMIT ?2",
    )
    .bind(dim_i64)
    .bind(limit)
    .fetch_all(pool)
    .await
}
