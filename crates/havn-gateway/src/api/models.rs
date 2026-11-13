use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::model_catalog::{self, ModelCatalog, ModelEntry};

#[derive(Debug, Serialize)]
pub struct ModelsView {
    pub default_model: Option<String>,
    pub effective_default: Option<String>,
    pub models: Vec<ModelEntry>,
    pub editable: bool,
}

fn view(state: &AppState) -> ModelsView {
    let catalog = state.models.load();
    ModelsView {
        default_model: catalog.default_model.clone(),
        effective_default: catalog.default_entry().map(|m| m.id.clone()),
        models: catalog.models.clone(),
        editable: state.trusted_proxies.is_none(),
    }
}

pub async fn get(State(state): State<AppState>, _user: AuthedUser) -> Json<ModelsView> {
    Json(view(&state))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsPut {
    #[serde(default)]
    pub default_model: Option<String>,
    pub models: Vec<ModelEntry>,
}

pub async fn put(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<ModelsPut>,
) -> Result<Json<ModelsView>, ApiError> {
    if state.trusted_proxies.is_some() {
        return Err(ApiError::Forbidden(
            "the model list is gateway-wide; with trust_header enabled it can only be \
             changed in config.toml"
                .into(),
        ));
    }
    let catalog = ModelCatalog {
        default_model: req.default_model,
        models: req.models,
    };
    catalog
        .validate()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let _writing = state.models_write.lock().await;
    let path = state.config_path.clone();
    let written = catalog.clone();
    tokio::task::spawn_blocking(move || model_catalog::write_to_file(&path, &written))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
    let ids: Vec<String> = catalog.models.iter().map(|m| m.id.clone()).collect();
    let default_model = catalog.default_model.clone();
    state.models.store(Arc::new(catalog));

    audit::record_user_action(
        &state.db,
        user.id,
        "llm.models.updated",
        serde_json::json!({ "models": ids, "default_model": default_model }),
    )
    .await;
    info!(models = ?ids, default = ?default_model, by = %user.id, "model list written to config.toml");
    Ok(Json(view(&state)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::auth::tests::state_with;

    fn body(ids: &[&str], default: Option<&str>) -> Json<ModelsPut> {
        Json(ModelsPut {
            default_model: default.map(str::to_string),
            models: ids
                .iter()
                .map(|id| ModelEntry {
                    id: (*id).into(),
                    label: None,
                    max_output_tokens: None,
                })
                .collect(),
        })
    }

    fn user(id: havn_core::UserId) -> AuthedUser {
        AuthedUser {
            id,
            display_name: "operator".into(),
        }
    }

    #[tokio::test]
    async fn put_writes_the_config_file_and_takes_effect() {
        let (state, user_id) = state_with(false).await;
        let view = put(
            State(state.clone()),
            user(user_id),
            body(&["openai:gpt-5", "anthropic:claude-opus-4-7"], None),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(view.effective_default.as_deref(), Some("openai:gpt-5"));
        assert!(view.editable);
        assert!(
            state
                .models
                .load()
                .get("anthropic:claude-opus-4-7")
                .is_some()
        );
        let parsed: crate::config::GatewayConfig =
            toml::from_str(&std::fs::read_to_string(&state.config_path).unwrap()).unwrap();
        assert_eq!(parsed.llm.catalog(), *state.models.load_full());
        let _ = std::fs::remove_file(&state.config_path);
    }

    #[tokio::test]
    async fn put_rejects_an_invalid_list() {
        let (state, user_id) = state_with(false).await;
        let err = put(State(state.clone()), user(user_id), body(&["gpt-5"], None))
            .await
            .expect_err("a model without a provider");
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
        assert!(!state.config_path.exists());
    }

    #[tokio::test]
    async fn put_is_refused_in_multi_user_mode() {
        let (state, user_id) = state_with(true).await;
        let err = put(
            State(state.clone()),
            user(user_id),
            body(&["openai:gpt-5"], None),
        )
        .await
        .expect_err("trust_header mode must refuse");
        assert!(matches!(err, ApiError::Forbidden(_)), "{err:?}");
        assert!(state.models.load().models.is_empty());
        assert!(!get(State(state), user(user_id)).await.0.editable);
    }
}
