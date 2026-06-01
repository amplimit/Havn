//! Embedding-provider abstraction for hybrid memory retrieval
//! (spec §9.4 v0.7 / §13 Phase 3).
//!
//! Three built-in providers, each addressing a different operator
//! priority:
//!
//! | Provider | When to pick it |
//! |---|---|
//! | [`openai::OpenAiEmbedder`] | Default. Most users already have an OPENAI_API_KEY; high-quality embeddings; small client-side footprint. |
//! | `local::FastEmbedder` (Stage 2) | Air-gapped or high-volume — pure-Rust ONNX runtime, ~30 MB model lazily downloaded. |
//! | `hrr::HrrEmbedder` (Stage 2) | Zero external dependencies; mathematical "pseudo-semantic" via Hermes-style HRR. **Quality noticeably worse than real embeddings** — for completion's sake, not the recommended default. |
//!
//! The trait is intentionally minimal — `dimensions()` for storage
//! validation, `embed()` for one query, `embed_batch()` for bulk
//! reindex. No streaming / no truncation policy in the trait;
//! impls handle their own provider quirks.
//!
//! Configuration lives in the gateway's `embedding` section and is
//! shipped to each runtime via the Welcome frame, so the agent
//! socket handshake is the single source of truth for "what
//! embedder is this session using".
//!
//! Storage lives at the DB layer (`havn_db::agent::memory`) — the
//! runtime computes vectors and hands them to `set_embedding`.
//! Hybrid scoring is in [`hybrid`].

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

pub mod backfill;
pub mod hrr;
pub mod hybrid;
pub mod local;
pub mod openai;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EmbedError {
    #[error("provider transport: {0}")]
    Transport(String),
    #[error("provider returned no embeddings for {0} input(s)")]
    NoOutput(usize),
    #[error("provider returned vector of dim {got}, expected {expected}")]
    DimMismatch { got: usize, expected: usize },
    #[error("provider mis-configured: {0}")]
    Config(String),
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync + std::fmt::Debug {
    /// Stable name for diagnostics + the `havn doctor` command.
    fn name(&self) -> &'static str;

    /// Output dimension. Must be constant for the lifetime of an
    /// instance (operators reconfigure → restart, not hot-swap).
    /// The DB layer uses this to filter mismatched stored vectors.
    fn dimensions(&self) -> usize;

    /// Embed one string. Default impl delegates to `embed_batch` so
    /// providers only have to implement the bulk path.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed_batch(&[text]).await?;
        out.pop().ok_or(EmbedError::NoOutput(1))
    }

    /// Embed multiple strings in one provider round-trip when
    /// possible. Order MUST match input order. Used by the (Stage 2)
    /// reindex command — single-call paths use [`Self::embed`].
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Live handle the runtime hands to memory ops. `None` means
/// embedding is disabled — `memory::remember` skips the
/// `set_embedding` write and `hybrid_search` falls through to FTS5
/// only. Encoded as `Option` rather than a no-op impl so the trait
/// surface stays honest about the capability.
pub type EmbedderHandle = Option<Arc<dyn EmbeddingProvider>>;

/// Provider configuration shape, mirrored in the gateway config and
/// the proto Welcome frame. Defaulted to `Disabled` so agents that
/// haven't opted in keep the v0.6 FTS5-only behaviour byte-identical.
///
/// The serde tag is `provider`. TOML / JSON form:
/// ```toml
/// [embedding]
/// provider = "openai"   # or "local", "hrr", "disabled"
/// [embedding.openai]
/// model = "text-embedding-3-small"
/// ```
///
/// Per-provider tuning lives in nested `[embedding.<provider>]`
/// blocks. Each provider's struct is `#[serde(default)]` so omitting
/// the block uses sensible defaults.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum EmbeddingConfig {
    #[default]
    Disabled,
    /// **Recommended default.** OpenAI's `/v1/embeddings` API.
    /// `text-embedding-3-small` (1536d) at $0.02 / 1M tokens.
    /// Highest semantic quality, zero local compute, requires
    /// network + an OpenAI-compatible endpoint.
    Openai(openai::OpenAiConfig),
    /// **Local ONNX via fastembed-rs.** Cargo-feature gated
    /// (`local-embed`); pulls ~80 MB of native libs at build time
    /// and downloads ~30-200 MB models lazily. Best for offline /
    /// air-gapped deployments where you can afford the binary
    /// bloat.
    Local(local::LocalConfig),
    /// **Hermes-style HRR** — pure Rust, zero deps, deterministic
    /// random projection over hashed tokens. Quality noticeably
    /// worse than real semantic embeddings (it's effectively a
    /// continuous bag-of-words). Pick only if `Openai` and
    /// `Local` are both off the table.
    Hrr(hrr::HrrConfig),
}

impl EmbeddingConfig {
    pub fn provider_name(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Openai(_) => "openai",
            Self::Local(_) => "local",
            Self::Hrr(_) => "hrr",
        }
    }

    /// Materialise the configured provider. Network / API-key
    /// validation happens lazily on first embed call so a misconfig
    /// doesn't block agent startup — the runtime logs the failure
    /// per-call and the row gets written without an embedding.
    pub fn instantiate(&self) -> Result<EmbedderHandle, EmbedError> {
        match self {
            Self::Disabled => Ok(None),
            Self::Openai(cfg) => Ok(Some(Arc::new(openai::OpenAiEmbedder::new(cfg.clone())?))),
            Self::Local(cfg) => Ok(Some(Arc::new(local::LocalEmbedder::new(cfg.clone())?))),
            Self::Hrr(cfg) => Ok(Some(Arc::new(hrr::HrrEmbedder::new(cfg.clone())?))),
        }
    }
}
