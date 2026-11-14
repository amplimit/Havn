//! `GET /search`: which `web_search` backend the caller's agents get, so
//! the dashboard and `havn doctor` can say whether search works and how to
//! turn it on.
//!
//! `/agents/{id}/web`: one agent's web access, `web_search` and network
//! egress, as two switches in its policy override. Edits apply on the
//! agent's next start, like every policy change.

use axum::Json;
use axum::extract::{Path, State};
use havn_db::repo::agents::Agent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::info;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::policy_resolver;

#[derive(Debug, Serialize)]
pub struct SearchStatus {
    /// `[search] provider`: `auto`, `off` or one backend.
    pub provider: String,
    /// The backend the caller's personal agents use first, or `None` when
    /// they have no `web_search` tool.
    pub backend: Option<&'static str>,
}

pub async fn status(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<SearchStatus>, ApiError> {
    let backend = state
        .search
        .backend_for(&state.db, user.id, None, &state.models.load())
        .await
        .map_err(|e| ApiError::Internal(format!("search credential lookup failed: {e}")))?;
    Ok(Json(SearchStatus {
        provider: state.search.provider().to_string(),
        backend: backend.map(crate::web_search::Backend::name),
    }))
}

#[derive(Debug, Serialize)]
pub struct AgentWebView {
    /// The agent's policy allows `web_search` (from its next start).
    pub can_search_web: bool,
    /// The agent has network egress, and with it the in-sandbox
    /// `web_fetch` (from its next start).
    pub egress_allowed: bool,
    /// What the owner's policy permits. With `trust_header` on, the
    /// agent's own settings cannot go beyond it.
    pub search_permitted: bool,
    pub egress_permitted: bool,
    /// The backend this agent's searches would use, or `None` when its
    /// owner has none set up.
    pub search_backend: Option<&'static str>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWebPatch {
    pub can_search_web: Option<bool>,
    pub egress_allowed: Option<bool>,
}

async fn view(state: &AppState, agent: &Agent) -> Result<AgentWebView, ApiError> {
    let multi_user = state.trusted_proxies.is_some();
    let policy = policy_resolver::for_session(&state.db, agent, multi_user).await;
    let owner = policy_resolver::for_owner(&state.db, agent).await;
    let backend = state
        .search
        .backend_for(
            &state.db,
            agent.owner_id,
            agent.team_id,
            &state.models.load(),
        )
        .await
        .map_err(|e| ApiError::Internal(format!("search credential lookup failed: {e}")))?;
    let egress =
        |p: &havn_core::Policy| p.permissions.can_access_network && p.network_policy.egress_allowed;
    Ok(AgentWebView {
        can_search_web: policy.permissions.can_search_web,
        egress_allowed: egress(&policy),
        search_permitted: !multi_user || owner.permissions.can_search_web,
        egress_permitted: !multi_user || egress(&owner),
        search_backend: backend.map(crate::web_search::Backend::name),
    })
}

pub async fn agent_get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<AgentWebView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    Ok(Json(view(&state, &agent).await?))
}

/// Set the switches in `config.policy`, leaving every other key of the
/// override as it is.
pub async fn agent_patch(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<AgentWebPatch>,
) -> Result<Json<AgentWebView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let config = with_web_access(&agent.config, &req);
    if let Some(policy) = config.get("policy") {
        serde_json::from_value::<havn_core::Policy>(policy.clone())
            .map_err(|e| ApiError::BadRequest(format!("config.policy: {e}")))?
            .validate()?;
    }
    let updated = havn_db::repo::agents::patch(&state.db, agent.id, None, Some(&config)).await?;
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.web.updated",
        serde_json::json!({
            "can_search_web": req.can_search_web,
            "egress_allowed": req.egress_allowed,
        }),
    )
    .await;
    info!(agent_id = %agent.id, "web access settings patched");
    Ok(Json(view(&state, &updated).await?))
}

/// `config` with the switches of `patch` set in its policy override.
fn with_web_access(config: &Value, patch: &AgentWebPatch) -> Value {
    let mut config = config.as_object().cloned().unwrap_or_default();
    let mut policy = config
        .get("policy")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut set = |section: &str, key: &str, value: bool| {
        let mut map = policy
            .get(section)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        map.insert(key.into(), Value::Bool(value));
        policy.insert(section.into(), Value::Object(map));
    };
    if let Some(on) = patch.can_search_web {
        set("permissions", "can_search_web", on);
    }
    if let Some(on) = patch.egress_allowed {
        set("network_policy", "egress_allowed", on);
        // Egress without the permission would leave the agent offline.
        if on {
            set("permissions", "can_access_network", true);
        }
    }
    config.insert("policy".into(), Value::Object(policy));
    Value::Object(config)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn web_access_switches_keep_the_rest_of_the_config() {
        let config = json!({
            "model": "openrouter:x/y",
            "policy": {
                "permissions": {"can_use_shell": false, "can_access_network": false},
                "network_policy": {"blocked_domains": ["example.com"]}
            }
        });
        let patched = with_web_access(
            &config,
            &AgentWebPatch {
                can_search_web: Some(false),
                egress_allowed: Some(true),
            },
        );
        assert_eq!(
            patched,
            json!({
                "model": "openrouter:x/y",
                "policy": {
                    "permissions": {
                        "can_use_shell": false,
                        "can_access_network": true,
                        "can_search_web": false
                    },
                    "network_policy": {
                        "blocked_domains": ["example.com"],
                        "egress_allowed": true
                    }
                }
            })
        );
        let policy: havn_core::Policy =
            serde_json::from_value(patched["policy"].clone()).expect("policy");
        assert!(!policy.permissions.can_search_web);
        assert!(policy.network_policy.egress_allowed);
        assert!(!policy.permissions.can_use_shell);

        let off = with_web_access(
            &json!({}),
            &AgentWebPatch {
                can_search_web: None,
                egress_allowed: Some(false),
            },
        );
        assert_eq!(
            off,
            json!({"policy": {"network_policy": {"egress_allowed": false}}})
        );
    }
}
