//! OpenAI embeddings provider — `text-embedding-3-small` by default
//! (1536 dimensions). Pure REST against `/v1/embeddings`; no
//! `async-openai` SDK dependency to keep the dependency surface
//! small and version-pinning predictable.
//!
//! API spec: https://platform.openai.com/docs/api-reference/embeddings/create
//!
//! Usage notes baked in:
//! - Batches up to 2048 inputs per call (OpenAI limit). Caller can
//!   hand a longer slice; we chunk internally.
//! - Pulls the API key from `$OPENAI_API_KEY` by default; operators
//!   point at a different env var via `api_key_env`.
//! - Supports OpenAI-compatible endpoints (Azure OpenAI, Together,
//!   OpenRouter for embeddings, vLLM `/v1` etc.) via `base_url`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{EmbedError, EmbeddingProvider};

const DEFAULT_MODEL: &str = "text-embedding-3-small";
const DEFAULT_DIM: usize = 1536;
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// OpenAI's per-request input cap. We chunk longer batches so the
/// caller never has to think about it.
const PER_REQUEST_MAX_INPUTS: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiConfig {
    /// Model id. Default `text-embedding-3-small` (1536d). Other
    /// known options: `text-embedding-3-large` (3072d),
    /// `text-embedding-ada-002` (legacy, 1536d).
    pub model: String,
    /// Output dimension. `text-embedding-3-*` supports server-side
    /// dimensionality reduction via the `dimensions` field — set
    /// this to a smaller number than the model's native dim to save
    /// storage. Defaulted to the model's native dim.
    pub dimensions: usize,
    /// Where to read the API key from. Default `OPENAI_API_KEY`.
    pub api_key_env: String,
    /// Override the API base. Use to target Azure OpenAI, vLLM,
    /// Together AI's `/v1`, or any OpenAI-compatible endpoint.
    pub base_url: String,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            dimensions: DEFAULT_DIM,
            api_key_env: DEFAULT_API_KEY_ENV.into(),
            base_url: DEFAULT_BASE_URL.into(),
        }
    }
}

#[derive(Debug)]
pub struct OpenAiEmbedder {
    cfg: OpenAiConfig,
    api_key: String,
    http: reqwest::Client,
}

impl OpenAiEmbedder {
    pub fn new(cfg: OpenAiConfig) -> Result<Self, EmbedError> {
        let api_key = std::env::var(&cfg.api_key_env).map_err(|_| {
            EmbedError::Config(format!(
                "OpenAI API key env var {:?} is not set",
                cfg.api_key_env
            ))
        })?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| EmbedError::Config(format!("HTTP client: {e}")))?;
        Ok(Self { cfg, api_key, http })
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedder {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn dimensions(&self) -> usize {
        self.cfg.dimensions
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(PER_REQUEST_MAX_INPUTS) {
            let body = OpenAiRequest {
                input: chunk.to_vec(),
                model: &self.cfg.model,
                // `text-embedding-3-*` supports `dimensions`; legacy
                // `ada-002` does NOT and OpenAI 400's if you pass it.
                // Send only when we want to override the model's native
                // dim — i.e. when the model name signals 3-* AND the
                // configured dim is non-zero (we always set non-zero
                // by Default impl, but defensive).
                dimensions: if self.cfg.model.starts_with("text-embedding-3-")
                    && self.cfg.dimensions > 0
                {
                    Some(self.cfg.dimensions)
                } else {
                    None
                },
            };
            let url = format!("{}/embeddings", self.cfg.base_url.trim_end_matches('/'));
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| EmbedError::Transport(e.to_string()))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(EmbedError::Transport(format!(
                    "{status}: {}",
                    text.chars().take(500).collect::<String>()
                )));
            }
            let parsed: OpenAiResponse = resp
                .json()
                .await
                .map_err(|e| EmbedError::Transport(format!("bad JSON: {e}")))?;
            if parsed.data.len() != chunk.len() {
                warn!(
                    expected = chunk.len(),
                    got = parsed.data.len(),
                    "openai embedding response length mismatch"
                );
            }
            // OpenAI guarantees output order matches input order.
            for d in parsed.data {
                if d.embedding.len() != self.cfg.dimensions {
                    return Err(EmbedError::DimMismatch {
                        got: d.embedding.len(),
                        expected: self.cfg.dimensions,
                    });
                }
                out.push(d.embedding);
            }
        }
        Ok(out)
    }
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    input: Vec<&'a str>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    data: Vec<OpenAiEmbedding>,
}

#[derive(Deserialize)]
struct OpenAiEmbedding {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn config_default_matches_text_embedding_3_small() {
        let c = OpenAiConfig::default();
        assert_eq!(c.model, "text-embedding-3-small");
        assert_eq!(c.dimensions, 1536);
        assert_eq!(c.api_key_env, "OPENAI_API_KEY");
        assert_eq!(c.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn missing_env_var_yields_config_error() {
        let mut c = OpenAiConfig::default();
        c.api_key_env = "DEFINITELY_NOT_SET_havn_test_var".into();
        let r = OpenAiEmbedder::new(c);
        assert!(matches!(r, Err(EmbedError::Config(_))));
    }
}
