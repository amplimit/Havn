//! Shared types for hybrid (FTS5 + vector) retrieval, used by every
//! source that participates in the runtime's `embedding::hybrid`
//! pipeline (spec §13 Phase 3).
//!
//! The actual search algorithm — cosine + BM25 + 70/30 weighted +
//! MMR diversification + recall-bump — lives in
//! `havn-runtime::embedding::hybrid` and is generic over the
//! `HybridSource` trait. Source impls (memory, skills_index)
//! return rows of these types so the scorer doesn't need to know
//! which table they came from.

/// One FTS5 hit: row id + raw BM25 rank (lower = better).
///
/// SQLite's `rank` column is "lower is better" by convention; the
/// scorer converts via `1 / (1 + rank)` before normalising.
#[derive(Debug, Clone)]
pub struct FtsHit {
    pub id: String,
    pub rank: f64,
}

/// One row at the candidate stage of vector scoring: just the id and
/// its decoded embedding. The full row content (memory Entry, skill
/// body, …) is materialised later via the source's `fetch_by_id`,
/// after MMR has picked winners — saves loading large bodies for
/// rows that won't make the top-K.
#[derive(Debug, Clone)]
pub struct EmbeddedCandidate {
    pub id: String,
    /// Always `Some` for rows the source returns; the source itself
    /// already filtered out vectors of mismatching dim or `None`. The
    /// scorer therefore doesn't need an "is the vector usable" check.
    pub embedding: Vec<f32>,
}

/// Decode a native-byte-order f32 BLOB back into `Vec<f32>`. Used by
/// every source's `embedded_candidates` query — same byte layout
/// as the writer-side `bytemuck::cast_slice`. Endian-portable across
/// the same-host SQLite instance only (spec §1.4 single-host scope).
pub fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    // We own the result so a small copy is safer than a borrow whose
    // lifetime is tied to SQLite's row buffer.
    let mut out = vec![0f32; bytes.len() / 4];
    let dst: &mut [u8] = bytemuck::cast_slice_mut(&mut out);
    let n = dst.len().min(bytes.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    out
}
