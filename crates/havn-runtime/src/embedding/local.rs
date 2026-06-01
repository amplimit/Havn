//! Local embedder slot — wired for `fastembed-rs` (pure-Rust ONNX
//! runtime) but compiled out by default to keep the standard build
//! lean.
//!
//! **Why feature-gated:**
//! - `fastembed`'s transitive `ort` (ONNX Runtime wrapper) downloads
//!   ~80 MB of platform-specific native libs during `cargo build`,
//!   which is a meaningful first-build cost shock for operators who
//!   don't actually want local embeddings.
//! - The model file itself is another ~30-200 MB lazy download to
//!   `~/.cache/havn/models` on first use.
//!
//! **How to enable:** add `fastembed = "5"` to `havn-runtime`'s
//! `[dependencies]` and build with `--features local-embed`. With
//! that combo the `LocalEmbedder` you get from
//! [`super::EmbeddingConfig::Local`] is a real implementation
//! using `BAAI/bge-small-en-v1.5` (384d) by default.
//!
//! Without the feature flag, instantiating `Local` returns a clear
//! `EmbedError::Config` so the operator knows to rebuild — better
//! than a confusing missing-symbol panic at runtime.
//!
//! The trait + config enum are present in all builds so operators
//! can `provider = "local"` in config.toml and the gateway will
//! validate it; the runtime materialises either the real impl or
//! the stub depending on how it was compiled.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{EmbedError, EmbeddingProvider};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    /// Hugging Face model id. Default `BAAI/bge-small-en-v1.5` (384d).
    /// Other tested options when `local-embed` is enabled:
    /// `BAAI/bge-base-en-v1.5` (768d), `BAAI/bge-large-en-v1.5` (1024d),
    /// `nomic-ai/nomic-embed-text-v1.5` (768d),
    /// `intfloat/multilingual-e5-small` (384d).
    pub model: String,
    /// Where the ONNX file is cached. Defaults to
    /// `$XDG_CACHE_HOME/havn/models` or `~/.cache/havn/models`.
    pub cache_dir: Option<String>,
    /// Output dimension. Must match the model's native dim.
    pub dimensions: usize,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            model: "BAAI/bge-small-en-v1.5".into(),
            cache_dir: None,
            dimensions: 384,
        }
    }
}

pub struct LocalEmbedder {
    cfg: LocalConfig,
    #[cfg(feature = "local-embed")]
    inner: std::sync::Mutex<fastembed::TextEmbedding>,
}

// Hand-rolled Debug because fastembed::TextEmbedding doesn't impl Debug
// (it owns ONNX session handles). Surface the configured model + dim
// so log lines stay useful — that's all anyone wants from {:?} here.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for LocalEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEmbedder")
            .field("model", &self.cfg.model)
            .field("dimensions", &self.cfg.dimensions)
            .finish()
    }
}

impl LocalEmbedder {
    #[cfg(feature = "local-embed")]
    pub fn new(cfg: LocalConfig) -> Result<Self, EmbedError> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        let model = match cfg.model.as_str() {
            "BAAI/bge-small-en-v1.5" => EmbeddingModel::BGESmallENV15,
            "BAAI/bge-base-en-v1.5" => EmbeddingModel::BGEBaseENV15,
            "BAAI/bge-large-en-v1.5" => EmbeddingModel::BGELargeENV15,
            "nomic-ai/nomic-embed-text-v1.5" => EmbeddingModel::NomicEmbedTextV15,
            "intfloat/multilingual-e5-small" => EmbeddingModel::MultilingualE5Small,
            other => {
                return Err(EmbedError::Config(format!(
                    "fastembed doesn't recognise model {other:?}; \
                     supported: BAAI/bge-{{small,base,large}}-en-v1.5, \
                     nomic-ai/nomic-embed-text-v1.5, intfloat/multilingual-e5-small"
                )));
            }
        };
        let mut opts = InitOptions::new(model);
        if let Some(dir) = &cfg.cache_dir {
            opts = opts.with_cache_dir(std::path::PathBuf::from(dir));
        }
        let inner = TextEmbedding::try_new(opts)
            .map_err(|e| EmbedError::Config(format!("fastembed init: {e}")))?;
        Ok(Self {
            cfg,
            inner: std::sync::Mutex::new(inner),
        })
    }

    #[cfg(not(feature = "local-embed"))]
    pub fn new(_cfg: LocalConfig) -> Result<Self, EmbedError> {
        Err(EmbedError::Config(
            "local embedder not built into this binary — recompile havn-runtime \
             with --features local-embed (adds the fastembed-rs / ONNX runtime \
             dependency, ~80 MB native libs at build time + ~30-200 MB model \
             file lazily downloaded on first use)"
                .into(),
        ))
    }
}

#[async_trait]
impl EmbeddingProvider for LocalEmbedder {
    fn name(&self) -> &'static str {
        "local"
    }

    fn dimensions(&self) -> usize {
        self.cfg.dimensions
    }

    #[cfg(feature = "local-embed")]
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let owned: Vec<String> = texts.iter().map(|s| (*s).to_string()).collect();
        let cfg_dim = self.cfg.dimensions;
        // fastembed's embed is sync + needs &mut self → run inside
        // block_in_place so the worker thread can re-schedule other
        // tasks while we wait. Requires multi-thread runtime (havn-
        // runtime uses default `#[tokio::main]` which is multi-thread).
        let res = tokio::task::block_in_place(|| {
            let mut g = self
                .inner
                .lock()
                .map_err(|_| EmbedError::Transport("local embedder mutex poisoned".into()))?;
            g.embed(owned, None)
                .map_err(|e| EmbedError::Transport(format!("fastembed: {e}")))
        })?;
        for v in &res {
            if v.len() != cfg_dim {
                return Err(EmbedError::DimMismatch {
                    got: v.len(),
                    expected: cfg_dim,
                });
            }
        }
        Ok(res)
    }

    #[cfg(not(feature = "local-embed"))]
    async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Config(
            "local embedder not compiled in — see new() error".into(),
        ))
    }
}
