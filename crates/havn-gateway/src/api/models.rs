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
    pub version: String,
    pub default_model: Option<String>,
    pub effective_default: Option<String>,
    /// The model that writes chats' titles; `None` for each agent's own.
    pub title_model: Option<String>,
    pub models: Vec<ModelEntry>,
    pub editable: bool,
}

fn view(state: &AppState) -> ModelsView {
    let catalog = state.models.load();
    ModelsView {
        version: catalog.version(),
        default_model: catalog.default_model.clone(),
        effective_default: catalog.default_entry().map(|m| m.id.clone()),
        title_model: catalog.title_model.clone(),
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
    pub version: String,
    #[serde(default)]
    pub default_model: Option<String>,
    /// Absent (a client from before title models): the current one, as
    /// long as it is still listed. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    pub title_model: Option<Option<String>>,
    pub models: Vec<ModelEntry>,
}

/// A field that is present, `null` included.
fn present<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
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
    let title_model = match req.title_model {
        Some(sent) => sent,
        None => state
            .models
            .load()
            .title_model
            .clone()
            .filter(|t| req.models.iter().any(|m| &m.id == t)),
    };
    let catalog = ModelCatalog {
        default_model: req.default_model,
        title_model,
        models: req.models,
    };
    catalog
        .validate()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let _writing = state.models_write.lock().await;
    let path = state.config_path.clone();
    let written = catalog.clone();
    let expected = req.version;
    let applied = tokio::task::spawn_blocking(move || {
        // The file is compared, not the running list: a hand edit the
        // watcher has not applied yet must not be overwritten either.
        if model_catalog::write_if_unchanged(&path, &expected, &written)? {
            return Ok(None);
        }
        // Retrying only helps if the gateway can load what is there now.
        Ok::<_, anyhow::Error>(Some(match crate::config::GatewayConfig::load_from(&path) {
            Ok(_) => {
                "the model list changed since it was read; load it again and retry".to_string()
            }
            Err(e) => format!(
                "config.toml has changes the gateway could not load ({e:#}); fix the file, \
                 then retry"
            ),
        }))
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
    if let Some(conflict) = applied {
        return Err(ApiError::Conflict(conflict));
    }
    let ids: Vec<String> = catalog.models.iter().map(|m| m.id.clone()).collect();
    let default_model = catalog.default_model.clone();
    let title_model = catalog.title_model.clone();
    state.models.store(Arc::new(catalog));

    audit::record_user_action(
        &state.db,
        user.id,
        "llm.models.updated",
        serde_json::json!({ "models": ids, "default_model": default_model, "title_model": title_model }),
    )
    .await;
    info!(models = ?ids, default = ?default_model, by = %user.id, "model list written to config.toml");
    Ok(Json(view(&state)))
}

/// The models an agent may use, for a chat's model menu (issue #190).
#[derive(Debug, Serialize)]
pub struct AgentModelsView {
    /// The agent's own model; `None` for the gateway's default.
    pub agent_model: Option<String>,
    /// The gateway's default.
    pub default_model: Option<String>,
    pub models: Vec<ModelEntry>,
}

/// `GET /agents/{id}/models`.
pub async fn for_agent(
    State(state): State<AppState>,
    user: AuthedUser,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<AgentModelsView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let models = crate::commands::usable_models(&state, agent.id)
        .await
        .unwrap_or_default();
    Ok(Json(AgentModelsView {
        agent_model: agent
            .config
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        default_model: state.models.load().default_entry().map(|m| m.id.clone()),
        models,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCheck {
    /// The entry to check, saved or not: its thinking as set there.
    pub entry: ModelEntry,
}

/// What a model check found (issue #190).
#[derive(Debug, Serialize)]
pub struct ModelCheckView {
    /// `reasoning`: it answered and showed its reasoning; `no_reasoning`:
    /// it answered without; `error`: the provider refused.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Characters of a check's reasoning and reply shown.
const CHECK_EXCERPT: usize = 300;

/// `POST /llm/models/check`: send `entry` a very short request, with its
/// thinking at its default, on the caller's credentials, and say how it
/// answered.
pub async fn check(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<ModelCheck>,
) -> Result<Json<ModelCheckView>, ApiError> {
    let entry = req.entry;
    ModelCatalog {
        default_model: None,
        title_model: None,
        models: vec![entry.clone()],
    }
    .validate()
    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let policy = crate::policy_resolver::for_user(&state.db, user.id).await;
    if !crate::llm_proxy::model_allowed(&policy.allowed_models, &entry.id) {
        return Err(ApiError::Forbidden(format!(
            "model {:?} is not allowed by your policy",
            entry.id
        )));
    }
    let ring = state
        .keyring
        .as_ref()
        .ok_or_else(|| ApiError::NoCredential(entry.provider().to_string()))?;
    let reasoning = entry.thinking.as_ref().map(|t| t.resolve(None));
    let budget = reasoning
        .as_ref()
        .and_then(crate::llm_proxy::Reasoning::budget);
    let request = crate::llm_proxy::AnthropicRequest {
        model: entry.model().to_string(),
        max_tokens: budget.map_or(512, |b| b.saturating_add(1024)),
        reasoning,
        messages: vec![crate::llm_proxy::AnthropicMessage {
            role: "user".into(),
            content: serde_json::json!("What is 17 + 25? Answer with the number."),
        }],
        system: None,
        temperature: None,
        tools: None,
    };
    let answer = crate::llm_proxy::complete_with_provider(
        &state.db,
        &state.http,
        ring,
        &crate::llm_proxy::Billing::user(user.id),
        entry.provider(),
        entry.max_output_tokens,
        &request,
        &state.llm,
    )
    .await;
    let excerpt = |s: &str| -> String { s.trim().chars().take(CHECK_EXCERPT).collect() };
    let view = match answer {
        Ok(resp) => {
            let of = |kind: &str, key: &str| -> String {
                resp.content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b: &&serde_json::Value| {
                        b.get("type").and_then(serde_json::Value::as_str) == Some(kind)
                    })
                    .filter_map(|b: &serde_json::Value| {
                        b.get(key).and_then(serde_json::Value::as_str)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let (reasoning, reply) = (of("thinking", "thinking"), of("text", "text"));
            ModelCheckView {
                outcome: if reasoning.trim().is_empty() {
                    "no_reasoning"
                } else {
                    "reasoning"
                },
                reasoning: (!reasoning.trim().is_empty()).then(|| excerpt(&reasoning)),
                reply: Some(excerpt(&reply)),
                error: None,
            }
        }
        Err(e) => ModelCheckView {
            outcome: "error",
            reasoning: None,
            reply: None,
            error: Some(excerpt(&e.to_string())),
        },
    };
    Ok(Json(view))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::auth::tests::state_with;

    fn body(version: &str, ids: &[&str]) -> Json<ModelsPut> {
        Json(ModelsPut {
            version: version.into(),
            default_model: None,
            title_model: None,
            models: ids
                .iter()
                .map(|id| ModelEntry {
                    id: (*id).into(),
                    label: None,
                    max_output_tokens: None,
                    vision: false,
                    thinking: None,
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

    fn empty() -> String {
        ModelCatalog::default().version()
    }

    #[tokio::test]
    async fn put_writes_the_config_file_and_takes_effect() {
        let (state, user_id) = state_with(false).await;
        let first = get(State(state.clone()), user(user_id)).await.0.version;
        let view = put(
            State(state.clone()),
            user(user_id),
            body(&first, &["openai:gpt-5", "anthropic:claude-opus-4-7"]),
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
    async fn a_client_that_leaves_out_the_title_model_keeps_it() {
        let (state, user_id) = state_with(false).await;
        let put_with = |version: String, ids: &[&str], title: Option<Option<&str>>| {
            let mut b = body(&version, ids);
            b.0.title_model = title.map(|t| t.map(String::from));
            put(State(state.clone()), user(user_id), b)
        };
        let first = get(State(state.clone()), user(user_id)).await.0.version;
        let view = put_with(first, &["openai:a", "openai:b"], Some(Some("openai:b")))
            .await
            .unwrap()
            .0;
        assert_eq!(view.title_model.as_deref(), Some("openai:b"));
        // An older client: the field left out keeps it.
        let view = put_with(view.version, &["openai:a", "openai:b"], None)
            .await
            .unwrap()
            .0;
        assert_eq!(view.title_model.as_deref(), Some("openai:b"));
        // Not when it removed the model.
        let view = put_with(view.version, &["openai:a"], None).await.unwrap().0;
        assert_eq!(view.title_model, None);
        let view = put_with(view.version, &["openai:a"], Some(Some("openai:a")))
            .await
            .unwrap()
            .0;
        assert_eq!(view.title_model.as_deref(), Some("openai:a"));
        // `null` clears it.
        let view = put_with(view.version, &["openai:a"], Some(None))
            .await
            .unwrap()
            .0;
        assert_eq!(view.title_model, None);
        let parsed: crate::config::GatewayConfig =
            toml::from_str(&std::fs::read_to_string(&state.config_path).unwrap()).unwrap();
        assert_eq!(parsed.llm.title_model, None);
        let _ = std::fs::remove_file(&state.config_path);
    }

    #[test]
    fn the_title_model_field_tells_absent_from_null() {
        let absent: ModelsPut =
            serde_json::from_str(r#"{"version":"v","models":[]}"#).expect("parse");
        assert_eq!(absent.title_model, None);
        let null: ModelsPut =
            serde_json::from_str(r#"{"version":"v","title_model":null,"models":[]}"#)
                .expect("parse");
        assert_eq!(null.title_model, Some(None));
        let set: ModelsPut =
            serde_json::from_str(r#"{"version":"v","title_model":"openai:a","models":[]}"#)
                .expect("parse");
        assert_eq!(set.title_model, Some(Some("openai:a".into())));
    }

    #[tokio::test]
    async fn put_refuses_a_writer_that_read_an_older_list() {
        let (state, user_id) = state_with(false).await;
        let first = put(
            State(state.clone()),
            user(user_id),
            body(&empty(), &["openai:gpt-5"]),
        )
        .await
        .unwrap()
        .0
        .version;
        let err = put(
            State(state.clone()),
            user(user_id),
            body(&empty(), &["gemini:gemini-2.5-pro"]),
        )
        .await
        .expect_err("stale version");
        assert!(matches!(err, ApiError::Conflict(_)), "{err:?}");

        // A hand edit the watcher has not applied yet counts as a change.
        let edited = ModelCatalog {
            default_model: None,
            title_model: None,
            models: vec![],
        };
        model_catalog::write_to_file(&state.config_path, &edited).unwrap();
        let err = put(
            State(state.clone()),
            user(user_id),
            body(&first, &["gemini:gemini-2.5-pro"]),
        )
        .await
        .expect_err("file changed underneath");
        assert!(matches!(err, ApiError::Conflict(_)), "{err:?}");
        assert_eq!(
            model_catalog::read_from_file(&state.config_path).unwrap(),
            edited
        );

        // A file the gateway cannot load is named as the reason.
        let mut raw = std::fs::read_to_string(&state.config_path).unwrap();
        raw.push_str("\n[llm.base_url]\nnobody = \"https://x.example\"\n");
        std::fs::write(&state.config_path, raw).unwrap();
        let err = put(
            State(state.clone()),
            user(user_id),
            body(&first, &["gemini:gemini-2.5-pro"]),
        )
        .await
        .expect_err("broken file");
        assert!(
            matches!(&err, ApiError::Conflict(m) if m.contains("could not load")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(&state.config_path);
    }

    #[tokio::test]
    async fn put_rejects_an_invalid_list() {
        let (state, user_id) = state_with(false).await;
        let err = put(
            State(state.clone()),
            user(user_id),
            body(&empty(), &["gpt-5"]),
        )
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
            body(&empty(), &["openai:gpt-5"]),
        )
        .await
        .expect_err("trust_header mode must refuse");
        assert!(matches!(err, ApiError::Forbidden(_)), "{err:?}");
        assert!(state.models.load().models.is_empty());
        assert!(!get(State(state), user(user_id)).await.0.editable);
    }
}
