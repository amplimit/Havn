//! `GET /search`: which `web_search` backend the caller's agents get, so
//! the dashboard and `havn doctor` can say whether search works and how to
//! turn it on.
//!
//! `/agents/{id}/web`: one agent's web access, the `web_search`,
//! `web_fetch` and egress switches and its own domain lists, in its policy
//! override. The switches apply on the agent's next start; the lists apply
//! to `web_fetch` at once, as the gateway resolves them on every call.
//!
//! `/me/domains`: the caller's own domain lists, added to every agent they
//! own, beside the blocked domains of their roles and the operator, which
//! they cannot remove.

use axum::Json;
use axum::extract::{Path, State};
use havn_db::repo::agents::Agent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

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
    /// The agent's policy allows `web_fetch` (from its next start).
    pub can_fetch_web: bool,
    /// The agent's policy allows `transcribe` (from its next start).
    pub can_transcribe: bool,
    /// The gateway transcribes audio: `[transcription]` is set.
    pub transcription: bool,
    /// The agent's sandbox has network egress (from its next start).
    pub egress_allowed: bool,
    /// That egress goes through the egress proxy, which asks about new
    /// domains.
    pub egress_proxy: bool,
    /// What the owner's policy permits. With `trust_header` on, the
    /// agent's own settings cannot go beyond it.
    pub search_permitted: bool,
    pub fetch_permitted: bool,
    pub transcribe_permitted: bool,
    pub egress_permitted: bool,
    /// The backend this agent's searches would use, or `None` when its
    /// owner has none set up.
    pub search_backend: Option<&'static str>,
    /// The agent's own domain lists, from its policy override.
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
    /// What the agent gets from its owner on top: their role's lists,
    /// their own and the operator's blocked domains.
    pub inherited_allowed_domains: Vec<String>,
    pub inherited_blocked_domains: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWebPatch {
    pub can_search_web: Option<bool>,
    pub can_fetch_web: Option<bool>,
    pub can_transcribe: Option<bool>,
    pub egress_allowed: Option<bool>,
    pub egress_proxy: Option<bool>,
    /// Replace the agent's own lists.
    pub allowed_domains: Option<Vec<String>>,
    pub blocked_domains: Option<Vec<String>>,
}

impl AgentWebPatch {
    fn is_empty(&self) -> bool {
        self.can_search_web.is_none()
            && self.can_fetch_web.is_none()
            && self.can_transcribe.is_none()
            && self.egress_allowed.is_none()
            && self.egress_proxy.is_none()
            && self.allowed_domains.is_none()
            && self.blocked_domains.is_none()
    }
}

/// The agent's own `network_policy.<key>` list from its override.
fn own_domains(agent: &Agent, key: &str) -> Vec<String> {
    agent
        .config
        .pointer(&format!("/policy/network_policy/{key}"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Entries trimmed and lowercased, blanks and repeats dropped.
fn tidy_domains(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in list {
        let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
        if !entry.is_empty() && !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

async fn view(state: &AppState, agent: &Agent) -> Result<AgentWebView, ApiError> {
    let multi_user = state.trusted_proxies.is_some();
    let policy = policy_resolver::for_session(&state.db, agent, multi_user).await;
    let owner = policy_resolver::for_owner(&state.db, agent).await;
    // Shown for information only: a failed lookup must not fail a PATCH
    // that has already been saved.
    let backend = state
        .search
        .backend_for(
            &state.db,
            agent.owner_id,
            agent.team_id,
            &state.models.load(),
        )
        .await
        .unwrap_or_else(|e| {
            warn!(agent_id = %agent.id, error = %e, "search credential lookup failed");
            None
        });
    let egress =
        |p: &havn_core::Policy| p.permissions.can_access_network && p.network_policy.egress_allowed;
    Ok(AgentWebView {
        can_search_web: policy.permissions.can_search_web,
        can_fetch_web: policy.permissions.can_fetch_web,
        can_transcribe: policy.permissions.can_transcribe,
        transcription: state.transcription.load().is_some(),
        egress_allowed: egress(&policy),
        egress_proxy: policy.network_policy.egress_proxy,
        search_permitted: !multi_user || owner.permissions.can_search_web,
        fetch_permitted: !multi_user || owner.permissions.can_fetch_web,
        transcribe_permitted: !multi_user || owner.permissions.can_transcribe,
        egress_permitted: !multi_user || egress(&owner),
        search_backend: backend.map(crate::web_search::Backend::name),
        allowed_domains: own_domains(agent, "allowed_domains"),
        blocked_domains: own_domains(agent, "blocked_domains"),
        inherited_allowed_domains: owner.network_policy.allowed_domains,
        inherited_blocked_domains: owner.network_policy.blocked_domains,
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
    if req.is_empty() {
        return Ok(Json(view(&state, &agent).await?));
    }
    let updated = havn_db::repo::agents::update_config(&state.db, agent.id, None, |current| {
        let config = with_web_access(current, &req);
        if let Some(policy) = config.get("policy") {
            serde_json::from_value::<havn_core::Policy>(policy.clone())
                .map_err(|e| ApiError::BadRequest(format!("config.policy: {e}")))?
                .validate()?;
        }
        Ok::<_, ApiError>(config)
    })
    .await??;
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.web.updated",
        serde_json::json!({
            "can_search_web": req.can_search_web,
            "can_fetch_web": req.can_fetch_web,
            "can_transcribe": req.can_transcribe,
            "egress_allowed": req.egress_allowed,
            "egress_proxy": req.egress_proxy,
            "allowed_domains": req.allowed_domains.as_deref().map(tidy_domains),
            "blocked_domains": req.blocked_domains.as_deref().map(tidy_domains),
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
    let mut set = |section: &str, key: &str, value: Value| {
        let mut map = policy
            .get(section)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        map.insert(key.into(), value);
        policy.insert(section.into(), Value::Object(map));
    };
    if let Some(on) = patch.can_search_web {
        set("permissions", "can_search_web", Value::Bool(on));
    }
    if let Some(on) = patch.can_fetch_web {
        set("permissions", "can_fetch_web", Value::Bool(on));
    }
    if let Some(on) = patch.can_transcribe {
        set("permissions", "can_transcribe", Value::Bool(on));
    }
    if let Some(list) = &patch.allowed_domains {
        set(
            "network_policy",
            "allowed_domains",
            tidy_domains(list).into(),
        );
    }
    if let Some(list) = &patch.blocked_domains {
        set(
            "network_policy",
            "blocked_domains",
            tidy_domains(list).into(),
        );
    }
    if let Some(on) = patch.egress_proxy {
        set("network_policy", "egress_proxy", Value::Bool(on));
    }
    if let Some(on) = patch.egress_allowed {
        set("network_policy", "egress_allowed", Value::Bool(on));
        // Egress without the permission would leave the agent offline.
        if on {
            set("permissions", "can_access_network", Value::Bool(true));
        }
    }
    config.insert("policy".into(), Value::Object(policy));
    Value::Object(config)
}

#[derive(Debug, Serialize)]
pub struct MyDomainsView {
    /// The caller's own lists.
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
    /// Set by their roles (as for their personal agents; a team agent uses
    /// its team's role) and the operator; they cannot remove these.
    pub admin_allowed_domains: Vec<String>,
    pub admin_blocked_domains: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MyDomainsPut {
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
}

async fn my_view(state: &AppState, user: &AuthedUser) -> Result<MyDomainsView, ApiError> {
    let own = havn_db::repo::users::domains(&state.db, user.id).await?;
    let roles = policy_resolver::for_user(&state.db, user.id).await;
    Ok(MyDomainsView {
        allowed_domains: own.allowed_domains,
        blocked_domains: own.blocked_domains,
        admin_allowed_domains: roles.network_policy.allowed_domains,
        admin_blocked_domains: havn_core::policy::union(
            &roles.network_policy.blocked_domains,
            policy_resolver::operator_blocked_domains(),
        ),
    })
}

pub async fn me_get(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<MyDomainsView>, ApiError> {
    Ok(Json(my_view(&state, &user).await?))
}

/// Replace the caller's own lists. They apply to the caller's agents on
/// their next start.
pub async fn me_put(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<MyDomainsPut>,
) -> Result<Json<MyDomainsView>, ApiError> {
    let lists = havn_db::repo::users::UserDomains {
        allowed_domains: tidy_domains(&req.allowed_domains),
        blocked_domains: tidy_domains(&req.blocked_domains),
    };
    havn_core::policy::validate_domains("allowed_domains", &lists.allowed_domains)?;
    havn_core::policy::validate_domains("blocked_domains", &lists.blocked_domains)?;
    havn_db::repo::users::set_domains(&state.db, user.id, &lists).await?;
    audit::record_user_action(
        &state.db,
        user.id,
        "user.domains.updated",
        serde_json::json!({
            "allowed_domains": lists.allowed_domains,
            "blocked_domains": lists.blocked_domains,
        }),
    )
    .await;
    Ok(Json(my_view(&state, &user).await?))
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
                can_fetch_web: Some(false),
                can_transcribe: Some(false),
                egress_allowed: Some(true),
                ..AgentWebPatch::default()
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
                        "can_search_web": false,
                        "can_fetch_web": false,
                        "can_transcribe": false
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
        assert!(!policy.permissions.can_fetch_web);
        assert!(!policy.permissions.can_transcribe);
        assert!(policy.network_policy.egress_allowed);
        assert!(!policy.permissions.can_use_shell);

        let off = with_web_access(
            &json!({}),
            &AgentWebPatch {
                egress_allowed: Some(false),
                ..AgentWebPatch::default()
            },
        );
        assert_eq!(
            off,
            json!({"policy": {"network_policy": {"egress_allowed": false}}})
        );
    }

    #[test]
    fn domain_lists_replace_the_agents_own_and_are_tidied() {
        let config = json!({
            "policy": {"network_policy": {"blocked_domains": ["old.example"], "egress_allowed": true}}
        });
        let patched = with_web_access(
            &config,
            &AgentWebPatch {
                allowed_domains: Some(vec![" Docs.RS. ".into(), "docs.rs".into(), String::new()]),
                blocked_domains: Some(vec!["ads.example".into()]),
                ..AgentWebPatch::default()
            },
        );
        assert_eq!(
            patched,
            json!({"policy": {"network_policy": {
                "allowed_domains": ["docs.rs"],
                "blocked_domains": ["ads.example"],
                "egress_allowed": true
            }}})
        );
    }
}
