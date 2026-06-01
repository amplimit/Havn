//! Hybrid retrieval — combine vector cosine similarity with FTS5
//! BM25 ranks, MMR-rerank for diversity, then bump the recall
//! anchor on the strongest hits.
//!
//! Algorithm (ported from OpenClaw's validated 70/30 weighting +
//! MMR — see project_competitor_learnings.md and the `extensions/
//! memory-core/src/memory/hybrid.ts` reference there):
//!
//! 1. Embed the query → q_vec.
//! 2. Pull all active rows + their stored vectors via the
//!    [`HybridSource::embedded_candidates`] hook (the source pre-
//!    filters by dim and any source-specific predicate, so the
//!    scorer never sees junk).
//! 3. Compute cosine(q_vec, row_vec) for each candidate; cap at
//!    `per_signal_pool` so a huge index doesn't melt the cosine sweep.
//! 4. Pull FTS5 hits via [`HybridSource::fts_hits`]; convert BM25
//!    rank → score via 1 / (1 + rank).
//! 5. Min-max normalise each signal into [0, 1], then weight-combine
//!    with `vec_weight`/`fts_weight`. Rows that only have one signal
//!    get the other treated as 0.
//! 6. MMR rerank: greedily pick rows that maximise relevance −
//!    `mmr_lambda` × max-similarity-to-already-picked. Prevents the
//!    top-K being five paraphrases of the same row (a known FTS5
//!    failure mode the typed-memory aging pass would otherwise
//!    treat as "high recall = stay alive forever").
//! 7. Bump recall on the top-`recall_bump_top_k` via
//!    [`HybridSource::bump_top_k`] — memory bumps recall_count /
//!    last_recalled_at; skills bumps use_count / last_used_at.
//!
//! Tunables (all default to OpenClaw values + spec §9.4 invariants):
//!
//! ```text
//! vec_weight        0.7
//! fts_weight        0.3
//! mmr_lambda        0.3   // 0 = pure relevance, 1 = pure diversity
//! recall_bump_top_k 3     // matches existing FTS5 path
//! per_signal_pool   50    // hard cap on rows we score per signal
//! ```
//!
//! The scorer is generic over [`HybridSource`]; concrete impls live
//! in [`crate::mcp`]'s sibling vertical: [`MemorySource`] here for
//! typed memory, [`crate::embedding::skills_source::SkillsSource`]
//! for the skill index. New sources (conversations? curator reports?)
//! plug in by impl-ing the trait without touching the scorer.

use async_trait::async_trait;
use havn_db::DbError;
use havn_db::agent::conversations::escape_fts_query;
use havn_db::agent::hybrid_common::{EmbeddedCandidate, FtsHit};
use havn_db::agent::memory::{self, Entry, Kind};
use sqlx::SqlitePool;
use tracing::{debug, warn};

use super::{EmbedError, EmbedderHandle};

#[derive(Debug, Clone, Copy)]
pub struct HybridParams {
    pub vec_weight: f32,
    pub fts_weight: f32,
    pub mmr_lambda: f32,
    pub recall_bump_top_k: usize,
    pub per_signal_pool: u32,
}

impl Default for HybridParams {
    fn default() -> Self {
        Self {
            vec_weight: 0.7,
            fts_weight: 0.3,
            mmr_lambda: 0.3,
            recall_bump_top_k: 3,
            per_signal_pool: 50,
        }
    }
}

/// Pluggable backing store for hybrid retrieval. One impl per source
/// table — memory, skills, … — lets the scorer below stay uniform.
///
/// `Filter` is source-specific shape (memory: `Vec<Kind>`; skills:
/// `()`); the source pre-applies it inside `fts_hits` and
/// `embedded_candidates` so the scorer doesn't need to know what it
/// means. `Row` is what gets returned at the end after the scorer
/// picks ids — memory returns `Entry`, skills returns its row form.
#[async_trait]
pub trait HybridSource: Send + Sync {
    type Filter: Send + Sync;
    type Row: Send;

    /// FTS5 hits for the (already-FTS-escaped) query, restricted to
    /// active rows + the source-specific filter, ordered best-first,
    /// capped at `limit`.
    async fn fts_hits(
        &self,
        query: &str,
        filter: &Self::Filter,
        limit: u32,
    ) -> Result<Vec<FtsHit>, DbError>;

    /// Active rows that actually carry a vector at the expected dim.
    /// Source filters out wrong-dim and missing-vector rows server-
    /// side so the scorer's input is uniform.
    async fn embedded_candidates(
        &self,
        expected_dim: usize,
        filter: &Self::Filter,
    ) -> Result<Vec<EmbeddedCandidate>, DbError>;

    /// Materialise one ranked row by id. Called after MMR has picked
    /// winners — saves loading large bodies for rows that didn't
    /// make the top-K.
    async fn fetch_by_id(&self, id: &str) -> Result<Option<Self::Row>, DbError>;

    /// Bump the source's recall anchor for the (already-truncated to
    /// top-K) ids. Failure is logged inside the impl, not propagated:
    /// recall is observability, not correctness.
    async fn bump_top_k(&self, ids: &[String]);
}

/// Generic hybrid search over any [`HybridSource`]. When `embedder`
/// is `None`, falls back to FTS5-only — preserves byte-for-byte
/// behaviour for operators who haven't opted into embeddings.
pub async fn search<S: HybridSource>(
    source: &S,
    embedder: &EmbedderHandle,
    query: &str,
    filter: &S::Filter,
    limit: u32,
    params: HybridParams,
) -> Result<Vec<S::Row>, SearchError> {
    let fts_query = escape_fts_query(query);

    let fts = source
        .fts_hits(&fts_query, filter, params.per_signal_pool)
        .await?;

    let vec_hits = if let Some(emb) = embedder {
        let q_vec = match emb.embed(query).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "embedder failed; degrading to FTS-only this query");
                return finalise_fts_only(source, fts, limit, params).await;
            }
        };
        let candidates = source.embedded_candidates(emb.dimensions(), filter).await?;
        score_vectors(&q_vec, &candidates, params.per_signal_pool)
    } else {
        return finalise_fts_only(source, fts, limit, params).await;
    };

    let normalised = normalise_and_combine(&vec_hits, &fts, params);
    let ranked = mmr_rerank(normalised, &vec_hits, params, limit);

    let mut out: Vec<S::Row> = Vec::with_capacity(ranked.len());
    for id in &ranked {
        if let Some(row) = source.fetch_by_id(id).await? {
            out.push(row);
        }
    }
    let take: Vec<String> = ranked
        .iter()
        .take(params.recall_bump_top_k)
        .cloned()
        .collect();
    if !take.is_empty() {
        source.bump_top_k(&take).await;
    }
    Ok(out)
}

async fn finalise_fts_only<S: HybridSource>(
    source: &S,
    fts: Vec<FtsHit>,
    limit: u32,
    params: HybridParams,
) -> Result<Vec<S::Row>, SearchError> {
    let take = (limit as usize).min(fts.len());
    let ids: Vec<String> = fts.into_iter().take(take).map(|h| h.id).collect();
    let mut out = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(row) = source.fetch_by_id(id).await? {
            out.push(row);
        }
    }
    let bump: Vec<String> = ids.iter().take(params.recall_bump_top_k).cloned().collect();
    if !bump.is_empty() {
        source.bump_top_k(&bump).await;
    }
    Ok(out)
}

/// Cosine similarity for the query vector against each candidate.
/// Source has already filtered out wrong-dim / missing rows so we
/// can dot-product unconditionally. Returns (id, sim) sorted
/// best-first and capped at `pool_cap`.
fn score_vectors(
    q_vec: &[f32],
    candidates: &[EmbeddedCandidate],
    pool_cap: u32,
) -> Vec<(String, f32)> {
    let q_norm = norm(q_vec);
    if q_norm == 0.0 {
        return Vec::new();
    }
    let mut scored: Vec<(String, f32)> = candidates
        .iter()
        .filter_map(|c| {
            if c.embedding.len() != q_vec.len() {
                return None;
            }
            let sim = dot(q_vec, &c.embedding) / (q_norm * norm(&c.embedding).max(1e-12));
            Some((c.id.clone(), sim))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(pool_cap as usize);
    scored
}

/// Normalise both signal scores into [0, 1] then weight-combine.
/// Returns (id, combined) sorted descending.
fn normalise_and_combine(
    vec_hits: &[(String, f32)],
    fts_hits: &[FtsHit],
    params: HybridParams,
) -> Vec<(String, f32)> {
    use std::collections::HashMap;
    let mut combined: HashMap<String, (f32, f32)> = HashMap::new();

    if let (Some(min), Some(max)) = vec_minmax(vec_hits) {
        let span = (max - min).max(1e-9);
        for (id, raw) in vec_hits {
            let s = (raw - min) / span;
            combined.entry(id.clone()).or_insert((0.0, 0.0)).0 = s;
        }
    }
    if !fts_hits.is_empty() {
        let raws: Vec<f32> = fts_hits
            .iter()
            .map(|h| 1.0 / (1.0 + h.rank as f32))
            .collect();
        let min = raws.iter().copied().fold(f32::INFINITY, f32::min);
        let max = raws.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let span = (max - min).max(1e-9);
        for (h, raw) in fts_hits.iter().zip(raws.iter()) {
            let s = (raw - min) / span;
            combined.entry(h.id.clone()).or_insert((0.0, 0.0)).1 = s;
        }
    }

    let mut out: Vec<(String, f32)> = combined
        .into_iter()
        .map(|(id, (v, f))| (id, params.vec_weight * v + params.fts_weight * f))
        .collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn vec_minmax(hits: &[(String, f32)]) -> (Option<f32>, Option<f32>) {
    if hits.is_empty() {
        return (None, None);
    }
    let mut mn = f32::INFINITY;
    let mut mx = f32::NEG_INFINITY;
    for (_, s) in hits {
        if *s < mn {
            mn = *s;
        }
        if *s > mx {
            mx = *s;
        }
    }
    (Some(mn), Some(mx))
}

/// Greedy MMR pick: at each step choose the candidate that maximises
/// (1 - λ) · relevance − λ · max-similarity-to-picked. Similarity
/// between picked rows is approximated via co-rank in the vec_score
/// map (saves cloning all candidate vectors into a NxN matrix).
fn mmr_rerank(
    combined: Vec<(String, f32)>,
    vec_hits: &[(String, f32)],
    params: HybridParams,
    limit: u32,
) -> Vec<String> {
    use std::collections::HashMap;
    if combined.is_empty() || limit == 0 {
        return Vec::new();
    }
    let vec_score: HashMap<&str, f32> = vec_hits.iter().map(|(id, s)| (id.as_str(), *s)).collect();

    let mut picked: Vec<String> = Vec::with_capacity(limit as usize);
    let mut remaining: Vec<(String, f32)> = combined;

    while !remaining.is_empty() && picked.len() < limit as usize {
        let lambda = params.mmr_lambda;
        let (best_idx, _) = remaining
            .iter()
            .enumerate()
            .map(|(i, (id, rel))| {
                let cand_v = vec_score.get(id.as_str()).copied().unwrap_or(0.0);
                let max_sim_to_picked = picked
                    .iter()
                    .map(|p| {
                        let p_v = vec_score.get(p.as_str()).copied().unwrap_or(0.0);
                        (1.0 - (cand_v - p_v).abs()).clamp(0.0, 1.0)
                    })
                    .fold(0.0f32, f32::max);
                let mmr = (1.0 - lambda) * rel - lambda * max_sim_to_picked;
                (i, mmr)
            })
            .fold((0, f32::NEG_INFINITY), |(bi, bs), (i, s)| {
                if s > bs { (i, s) } else { (bi, bs) }
            });
        let (id, _) = remaining.swap_remove(best_idx);
        picked.push(id);
    }
    debug!(picked = picked.len(), "MMR rerank done");
    picked
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    #[error("db: {0}")]
    Db(#[from] DbError),
    #[error("embed: {0}")]
    Embed(#[from] EmbedError),
}

// ---------------------------------------------------------------- //
// MemorySource — `HybridSource` adapter for typed memory rows.     //
// ---------------------------------------------------------------- //

/// Adapter that wires the typed-memory table into [`search`].
/// Filter is `Vec<Kind>` (empty = all kinds); Row is the full
/// `memory::Entry`.
pub struct MemorySource<'a> {
    pub pool: &'a SqlitePool,
}

impl<'a> MemorySource<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl HybridSource for MemorySource<'_> {
    type Filter = Vec<Kind>;
    type Row = Entry;

    async fn fts_hits(
        &self,
        query: &str,
        filter: &Vec<Kind>,
        limit: u32,
    ) -> Result<Vec<FtsHit>, DbError> {
        memory::fts_hits(self.pool, query, filter, limit).await
    }

    async fn embedded_candidates(
        &self,
        expected_dim: usize,
        filter: &Vec<Kind>,
    ) -> Result<Vec<EmbeddedCandidate>, DbError> {
        memory::embedded_candidates(self.pool, expected_dim, filter).await
    }

    async fn fetch_by_id(&self, id: &str) -> Result<Option<Entry>, DbError> {
        memory::fetch_by_id(self.pool, id).await
    }

    async fn bump_top_k(&self, ids: &[String]) {
        if let Err(e) = memory::bump_recall_for(self.pool, ids).await {
            warn!(error = %e, "memory hybrid: recall bump failed");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn cosine_orthogonal_is_zero_aligned_is_one() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let c = vec![1.0, 0.0, 0.0];
        assert!(dot(&a, &b).abs() < 1e-6);
        assert!((dot(&a, &c) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalise_clamps_to_unit_range() {
        let vec_hits = vec![("a".into(), 0.9), ("b".into(), 0.5), ("c".into(), 0.1)];
        let fts: Vec<FtsHit> = Vec::new();
        let combined = normalise_and_combine(&vec_hits, &fts, HybridParams::default());
        assert_eq!(combined.len(), 3);
        assert_eq!(combined[0].0, "a");
        for (_, s) in &combined {
            assert!(*s >= 0.0 && *s <= 0.7 + 1e-6, "score {s} out of range");
        }
    }

    #[test]
    fn mmr_picks_diverse_winners() {
        let vec_hits = vec![("a".into(), 0.9), ("b".into(), 0.85), ("c".into(), 0.1)];
        let combined = vec![("a".into(), 0.9), ("b".into(), 0.88), ("c".into(), 0.7)];
        let params = HybridParams {
            mmr_lambda: 0.7,
            ..HybridParams::default()
        };
        let picked = mmr_rerank(combined, &vec_hits, params, 2);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0], "a");
        assert_eq!(picked[1], "c", "MMR should diversify away from 'b'");
    }

    #[test]
    fn empty_inputs_return_empty() {
        let combined = normalise_and_combine(&[], &[], HybridParams::default());
        assert!(combined.is_empty());
        let picked = mmr_rerank(Vec::new(), &[], HybridParams::default(), 5);
        assert!(picked.is_empty());
    }

    /// End-to-end: walk the entire hybrid-retrieval data path with a
    /// real (in-memory) agent SQLite + the deterministic HRR embedder
    /// against [`MemorySource`], to validate that the trait wiring
    /// composes the same way as before the refactor.
    #[tokio::test]
    async fn end_to_end_remember_backfill_search_recall() {
        use crate::embedding::EmbeddingProvider;
        use crate::embedding::backfill;
        use crate::embedding::hrr::{HrrConfig, HrrEmbedder};
        use havn_db::agent::connect_in_memory;
        use havn_db::agent::memory as mem;
        use std::sync::Arc;

        let pool = connect_in_memory().await.expect("connect agent db");
        let embedder: Arc<dyn EmbeddingProvider> =
            Arc::new(HrrEmbedder::new(HrrConfig::default()).expect("hrr"));

        let seed: &[(mem::Kind, mem::Source, &str, &str)] = &[
            (
                mem::Kind::Preference,
                mem::Source::UserTold,
                "user.editor",
                "vim editor with custom keybindings",
            ),
            (
                mem::Kind::Project,
                mem::Source::UserTold,
                "project.havn",
                "open infrastructure for autonomous agents",
            ),
            (
                mem::Kind::Identity,
                mem::Source::UserTold,
                "user.name",
                "Alice",
            ),
            (
                mem::Kind::Event,
                mem::Source::AgentInferred,
                "event.standup.last",
                "discussed sqlite migrations",
            ),
        ];
        for (kind, source, key, value) in seed {
            let id = mem::remember(
                &pool,
                mem::NewEntry {
                    key,
                    value,
                    kind: *kind,
                    source: *source,
                    ttl_days: None,
                },
            )
            .await
            .expect("remember");
            assert!(!id.is_empty(), "remember should return the active row id");
        }

        backfill::run_to_completion(pool.clone(), embedder.clone()).await;

        let candidates = mem::embedded_candidates(&pool, embedder.dimensions(), &[])
            .await
            .expect("embedded_candidates");
        assert_eq!(
            candidates.len(),
            4,
            "all rows should carry vectors after backfill"
        );
        for c in &candidates {
            assert_eq!(
                c.embedding.len(),
                embedder.dimensions(),
                "row {:?} has wrong vector dimension",
                c.id
            );
        }

        let handle: EmbedderHandle = Some(embedder.clone());
        let source = MemorySource::new(&pool);
        let hits = search(
            &source,
            &handle,
            "what vim editor preference do I use",
            &Vec::<mem::Kind>::new(),
            4,
            HybridParams::default(),
        )
        .await
        .expect("hybrid search");
        assert!(!hits.is_empty(), "search should return at least one hit");
        assert_eq!(
            hits[0].key,
            "user.editor",
            "user.editor should rank first; got order: {:?}",
            hits.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
        );

        let bumped = mem::get(&pool, "user.editor")
            .await
            .expect("get")
            .expect("user.editor row");
        assert_eq!(bumped.recall_count, 1);
        assert!(bumped.last_recalled_at.is_some());
    }
}
