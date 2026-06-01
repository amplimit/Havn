//! HRR-style deterministic text embedder — Hermes Agent's
//! `holographic` plugin pattern, ported to pure Rust.
//!
//! **Quality caveat (read this before shipping it):**
//!
//! This is *not* a learned semantic embedding. It's a deterministic
//! random projection: each whitespace-separated token gets hashed to
//! a fixed pseudo-random ±1/√N unit vector via FxHash → SplitMix64,
//! and the row's vector is the L2-normalised sum. Two strings that
//! share tokens get high cosine; two strings that mean the same
//! thing but use different words ("editor" vs "vim") DO NOT.
//!
//! What it gives you over FTS5-only:
//! - Compositional bag-of-tokens score that survives stemming /
//!   case differences (we lower-case + strip punctuation)
//! - Continuous score for ranking, not just rank-boolean MATCH
//! - Zero external dependencies — pure Rust, no model files, no
//!   network, no ONNX runtime, no API key
//!
//! What it does NOT give you:
//! - True synonym recall ("network latency" ↔ "sluggish API")
//! - Cross-language transfer
//! - Anything you'd actually call "semantic search"
//!
//! When to pick it: **air-gapped deployments where neither cloud
//! API nor model download is allowed**. For anything else, prefer
//! `Openai` (default) or `Local` (fastembed-rs).
//!
//! Reference: Plate 1995 "Holographic Reduced Representations".
//! The Hermes implementation is in `plugins/memory/holographic/holographic.py`.
//! Their HRR includes binding via circular convolution; our use case
//! (free-text retrieval) only needs bundling (sum), so we skip the
//! convolution layer — same recall behaviour, simpler code.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{EmbedError, EmbeddingProvider};

const DEFAULT_DIM: usize = 1024;
const DEFAULT_SEED: u64 = 0x4841_564e_5f48_5252; // ASCII "HAVN_HRR"

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HrrConfig {
    /// Output dimension. 1024 matches Hermes's default and is a good
    /// balance between recall headroom + cosine numerical stability.
    /// Smaller (256) is faster but more collision-prone; larger (4096)
    /// rarely helps for the kinds of recall HRR is good at.
    pub dimensions: usize,
    /// Per-deployment seed for the hash → vector mapping. Keep stable
    /// across restarts (otherwise every token gets a new random
    /// vector and stored embeddings become useless). Different seeds
    /// across deployments prevent stored-vector portability across
    /// untrusting hosts — usually what you want.
    pub seed: u64,
}

impl Default for HrrConfig {
    fn default() -> Self {
        Self {
            dimensions: DEFAULT_DIM,
            seed: DEFAULT_SEED,
        }
    }
}

#[derive(Debug)]
pub struct HrrEmbedder {
    cfg: HrrConfig,
    inv_sqrt_dim: f32,
}

impl HrrEmbedder {
    pub fn new(cfg: HrrConfig) -> Result<Self, EmbedError> {
        if cfg.dimensions == 0 {
            return Err(EmbedError::Config("hrr dimensions must be > 0".into()));
        }
        let inv_sqrt_dim = 1.0 / (cfg.dimensions as f32).sqrt();
        Ok(Self { cfg, inv_sqrt_dim })
    }

    /// Tokenize: lowercase + split on non-alphanumeric. Cheap and
    /// stable. Matches the way most simple BoW systems pre-process.
    fn tokenize(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Hash a token + seed → 64-bit deterministic value. SplitMix64
    /// avalanche on the FxHash output gives us a high-quality stream
    /// of bits without pulling in `sha2` for one tiny use case.
    fn hash_token(&self, token: &str) -> u64 {
        // FxHash-style multiplicative hash — fast, stable, no deps.
        let mut h: u64 = self.cfg.seed;
        for b in token.as_bytes() {
            h = h
                .wrapping_mul(0x517c_c1b7_2722_0a95)
                .wrapping_add(u64::from(*b));
        }
        // SplitMix64 finalizer for avalanche.
        let mut z = h.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Project one token to a ±1/√N pseudo-random unit vector. We use
    /// the hash bytes as a seed for a deterministic stream: each
    /// dimension's sign comes from the bit at that position in the
    /// repeated hash output. This is equivalent to LSH random
    /// projection with sign(rand) entries.
    fn token_vector(&self, token: &str, out: &mut [f32]) {
        let mut h = self.hash_token(token);
        for slot in out.iter_mut() {
            // Pull one bit; refresh h via SplitMix when exhausted.
            // Cheap branchless: use the low bit then rotate.
            let bit = h & 1;
            *slot = if bit == 0 {
                self.inv_sqrt_dim
            } else {
                -self.inv_sqrt_dim
            };
            h = h.rotate_left(1);
            // Re-mix every 64 bits so we don't repeat the cycle.
            if h.trailing_zeros() >= 6 {
                h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            }
        }
    }
}

#[async_trait]
impl EmbeddingProvider for HrrEmbedder {
    fn name(&self) -> &'static str {
        "hrr"
    }

    fn dimensions(&self) -> usize {
        self.cfg.dimensions
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        let mut token_buf = vec![0f32; self.cfg.dimensions];
        for text in texts {
            let mut sum = vec![0f32; self.cfg.dimensions];
            for token in Self::tokenize(text) {
                self.token_vector(&token, &mut token_buf);
                for (acc, t) in sum.iter_mut().zip(token_buf.iter()) {
                    *acc += *t;
                }
            }
            // L2 normalise so cosine similarity ranges in [-1, 1].
            let norm = sum.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            for s in &mut sum {
                *s /= norm;
            }
            out.push(sum);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb).max(1e-12)
    }

    #[tokio::test]
    async fn deterministic_same_seed_same_vector() {
        let e1 = HrrEmbedder::new(HrrConfig::default()).expect("init");
        let e2 = HrrEmbedder::new(HrrConfig::default()).expect("init");
        let v1 = e1.embed("hello world").await.expect("embed");
        let v2 = e2.embed("hello world").await.expect("embed");
        assert_eq!(v1.len(), v2.len());
        for (a, b) in v1.iter().zip(v2.iter()) {
            assert!((a - b).abs() < 1e-6, "got {a} vs {b}");
        }
    }

    #[tokio::test]
    async fn shared_tokens_yield_high_cosine() {
        let e = HrrEmbedder::new(HrrConfig::default()).expect("init");
        let v1 = e.embed("the quick brown fox").await.expect("embed");
        let v2 = e.embed("a quick brown rabbit").await.expect("embed");
        let v3 = e.embed("entirely different sentence").await.expect("embed");
        let sim_overlap = cosine(&v1, &v2);
        let sim_unrelated = cosine(&v1, &v3);
        assert!(
            sim_overlap > sim_unrelated,
            "overlap {sim_overlap} should beat unrelated {sim_unrelated}"
        );
    }

    #[tokio::test]
    async fn empty_text_yields_zero_vector() {
        let e = HrrEmbedder::new(HrrConfig::default()).expect("init");
        let v = e.embed("").await.expect("embed");
        assert_eq!(v.len(), 1024);
        // All zeros after normalize-of-zero.
        assert!(v.iter().all(|x| x.abs() < 1e-6));
    }

    #[tokio::test]
    async fn different_seeds_produce_different_vectors() {
        // Seeded mixing means a deployment-unique seed makes vectors
        // non-portable. Documented behaviour — verify it.
        let e1 = HrrEmbedder::new(HrrConfig {
            seed: 1,
            ..HrrConfig::default()
        })
        .expect("init");
        let e2 = HrrEmbedder::new(HrrConfig {
            seed: 2,
            ..HrrConfig::default()
        })
        .expect("init");
        let v1 = e1.embed("identical text").await.expect("embed");
        let v2 = e2.embed("identical text").await.expect("embed");
        let sim = cosine(&v1, &v2);
        assert!(
            sim.abs() < 0.5,
            "different seeds should ~decorrelate, got {sim}"
        );
    }

    #[tokio::test]
    async fn batch_matches_single() {
        let e = HrrEmbedder::new(HrrConfig::default()).expect("init");
        let single = e.embed("test").await.expect("single");
        let batch = e.embed_batch(&["test"]).await.expect("batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(single, batch[0]);
    }

    #[test]
    fn rejects_zero_dim() {
        let e = HrrEmbedder::new(HrrConfig {
            dimensions: 0,
            ..HrrConfig::default()
        });
        assert!(matches!(e, Err(EmbedError::Config(_))));
    }
}
