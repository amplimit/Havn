//! `/embedding` — system-level hybrid retrieval config (spec §9.4 v0.7,
//! §13 Phase 3).
//!
//! `GET` returns the currently-active provider + raw config block so
//! the dashboard can show "memory: hybrid (openai)" or
//! "memory: keyword-only".
//!
//! `PATCH` swaps the in-memory config; new agent spawns pick it up at
//! their next Welcome handshake. Already-running agents keep their
//! Welcome-snapshot config (spec §9.4 frozen-prompt invariant) until
//! the next restart. Persistence across gateway restart still flows
//! through `~/.config/havn/config.toml` — that file is the canonical
//! operator-facing source of truth (spec §1.6 "infrastructure not
//! SaaS"); the dashboard PATCH is a runtime override on top of it.
//! The dashboard surfaces both facts in the editor's hint text.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;

const VALID_PROVIDERS: &[&str] = &["disabled", "openai", "local", "hrr"];

#[derive(Debug, Serialize)]
pub struct EmbeddingStatus {
    /// "disabled" | "openai" | "local" | "hrr". Whatever the
    /// gateway has currently materialised.
    pub provider: String,
    /// Raw config sub-block so the dashboard can show model id,
    /// dimensions, etc. without us inventing a per-provider DTO.
    /// Strips secrets — `api_key_env` is just the env-var NAME, not
    /// the value, so this is safe to surface.
    pub config: serde_json::Value,
    /// True when memory_search will actually run a hybrid (vector +
    /// BM25) query on this gateway. Mirrors `provider != "disabled"`.
    pub hybrid_enabled: bool,
}

/// PATCH body — pass the full new `[embedding]` block. We replace
/// rather than merge because the per-provider sub-blocks are
/// mutually exclusive (you don't want OpenAI's `model` field stuck
/// on after switching to HRR).
///
/// Validation here is structural only — `provider` must be one of the
/// known names. The runtime does the deeper provider-specific
/// validation (env-var present, dim > 0, …) at the next agent's
/// Welcome materialisation; failures there fall back to "embedding
/// disabled" with a warn, which the GET will then reflect.
#[derive(Debug, Deserialize)]
pub struct EmbeddingPatch {
    pub provider: String,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

pub async fn status(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<EmbeddingStatus>, ApiError> {
    let cfg = state.embedding_config.load().as_ref().clone();
    Ok(Json(view_from_value(cfg)))
}

pub async fn patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<EmbeddingPatch>,
) -> Result<Json<EmbeddingStatus>, ApiError> {
    if !VALID_PROVIDERS.iter().any(|p| *p == req.provider) {
        return Err(ApiError::BadRequest(format!(
            "unknown provider {:?} — must be one of {:?}",
            req.provider, VALID_PROVIDERS
        )));
    }
    // Re-assemble the value with `provider` first, then any
    // provider-specific sub-block(s) the caller sent. The runtime
    // deserializer (`EmbeddingConfig` in havn-runtime) is tagged on
    // `provider` and ignores irrelevant keys, so passing along
    // unrecognised sub-blocks is harmless.
    let mut new_cfg = serde_json::Map::new();
    new_cfg.insert(
        "provider".into(),
        serde_json::Value::String(req.provider.clone()),
    );
    for (k, v) in req.rest {
        if k == "provider" {
            continue;
        }
        new_cfg.insert(k, v);
    }
    let new_value = serde_json::Value::Object(new_cfg);

    state
        .embedding_config
        .store(std::sync::Arc::new(new_value.clone()));
    audit::record_user_action(
        &state.db,
        user.id,
        "embedding.config.updated",
        serde_json::json!({ "provider": req.provider }),
    )
    .await;
    tracing::info!(
        provider = %req.provider,
        by = %user.id,
        "embedding config swapped (in-memory; persist via config.toml)"
    );
    Ok(Json(view_from_value(new_value)))
}

fn view_from_value(cfg: serde_json::Value) -> EmbeddingStatus {
    let provider = cfg
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("disabled")
        .to_string();
    EmbeddingStatus {
        hybrid_enabled: provider != "disabled",
        provider,
        config: cfg,
    }
}
