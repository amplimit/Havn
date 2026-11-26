//! `/agents/:id/secrets` and `/agents/:id/shell-env` (issue #233): the
//! secrets an agent's tools use, and the variables its shell commands get.
//!
//! Only the agent's owner may see or change them (`resolve_agent`; anyone
//! else gets 404). No response carries a value: the list has names, whether
//! each is set, and what the agent said when it asked for one. Setting and
//! removing are audited as `secret.set` and `secret.removed`, with the name
//! and never the value; changing the shell environment as
//! `agent.shell_env`, with the variables' names. A running agent gets the
//! change at once: shell commands from their next run, MCP servers from
//! the agent's next start.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use havn_core::secret_ref;
use havn_db::repo::agent_secrets::{self, Was};
use havn_db::repo::agents::Agent;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::secrets::{self, SetError};

#[derive(Debug, Serialize)]
pub struct SecretsView {
    pub secrets: Vec<SecretView>,
    /// Variable name to its value: text, or `secret:<name>`.
    pub shell_env: BTreeMap<String, String>,
    /// Secrets can be set: the gateway has `HAVN_AGE_KEY`.
    pub encrypts: bool,
    /// Values shorter than this are not redacted from tool output.
    pub min_redacted_chars: usize,
    /// MCP servers that refer to secrets but run with settings the owner's
    /// role supplies, not the agent's own: their references are not
    /// resolved, so they do not start.
    pub refused_mcp_servers: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SecretView {
    pub name: String,
    /// False while it is only asked for.
    pub set: bool,
    pub updated_at: DateTime<Utc>,
    /// The agent asked for it and it is not set yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<RequestView>,
    /// The shell variables that get it.
    pub variables: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RequestView {
    pub reason: String,
    /// The variable the agent asked to get it as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    pub at: DateTime<Utc>,
    /// Who the agent was answering: a name, or a chat-app sender's id;
    /// absent for a scheduled task or a heartbeat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asked_by: Option<String>,
    /// Where: `web chat`, a chat app, `group on <app>`, `webhook`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asked_in: Option<String>,
    /// The owner, in their own chat.
    pub by_owner: bool,
    /// Times the agent asked again since; the first request is kept.
    pub again: i64,
}

async fn view(state: &AppState, agent: &Agent) -> Result<SecretsView, ApiError> {
    let shell_env = secrets::shell_env(&agent.config);
    let rows = agent_secrets::list(&state.db, agent.id).await?;
    let secrets = rows
        .into_iter()
        .map(|row| SecretView {
            variables: secrets::variables_of(&agent.config, &row.name),
            set: row.value_ciphertext.is_some(),
            updated_at: row.updated_at,
            requested: row.request.map(|r| RequestView {
                reason: r.reason,
                env: r.env,
                at: r.at,
                asked_by: r.asked_by,
                asked_in: r.asked_in,
                by_owner: r.by_owner,
                again: r.again,
            }),
            name: row.name,
        })
        .collect();
    let policy =
        crate::policy_resolver::for_session(&state.db, agent, state.trusted_proxies.is_some())
            .await;
    let refused_mcp_servers = if policy.permissions.can_use_mcp {
        secrets::refused_mcp_servers(&agent.config, &policy)
    } else {
        Vec::new()
    };
    Ok(SecretsView {
        secrets,
        shell_env,
        encrypts: state.secrets.encrypts(),
        min_redacted_chars: secret_ref::MIN_REDACTED_CHARS,
        refused_mcp_servers,
    })
}

/// `GET /agents/:id/secrets`.
pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<SecretsView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    Ok(Json(view(&state, &agent).await?))
}

#[derive(Deserialize)]
pub struct SetRequest {
    pub value: String,
    /// A shell variable to give it as, added to the shell environment.
    #[serde(default)]
    pub env: Option<String>,
}

impl std::fmt::Debug for SetRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetRequest")
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

/// Write the agent's shell environment as `change` makes it from the one
/// stored, in the same transaction, so a change made meanwhile is not
/// lost. The result is checked before it is written.
async fn write_shell_env(
    state: &AppState,
    agent: &Agent,
    change: impl FnOnce(&mut BTreeMap<String, String>),
) -> Result<Agent, ApiError> {
    let updated = havn_db::repo::agents::update_config(&state.db, agent.id, None, |current| {
        let mut env = secrets::shell_env(current);
        change(&mut env);
        secrets::check_shell_env(&env)?;
        let mut config = current.as_object().cloned().unwrap_or_default();
        if env.is_empty() {
            config.remove("shell_env");
        } else {
            let value = serde_json::to_value(&env).map_err(|e| e.to_string())?;
            config.insert("shell_env".into(), value);
        }
        Ok::<_, String>(serde_json::Value::Object(config))
    })
    .await?;
    updated.map_err(ApiError::BadRequest)
}

/// `PUT /agents/:id/secrets/:name` with `{value, env?}` — set the secret,
/// ending the agent's request for it. With `env`, shell commands get it as
/// that variable.
pub async fn set(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, name)): Path<(String, String)>,
    Json(body): Json<SetRequest>,
) -> Result<Json<SecretsView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    secret_ref::check_name(&name).map_err(|e| ApiError::BadRequest(e.into()))?;
    secrets::check_value(&body.value).map_err(ApiError::BadRequest)?;
    let env = body.env.as_deref().map(str::trim).filter(|e| !e.is_empty());
    let reference = format!("{}{name}", secret_ref::PREFIX);
    if let Some(variable) = env {
        // Checked against the environment as it is now, so a variable that
        // cannot be added refuses the whole change; written below from the
        // one stored then.
        secrets::check_env_name(variable).map_err(ApiError::BadRequest)?;
        let mut next = secrets::shell_env(&agent.config);
        next.insert(variable.to_string(), reference.clone());
        secrets::check_shell_env(&next).map_err(ApiError::BadRequest)?;
    }
    let was = state
        .secrets
        .set(agent.id, &name, &body.value)
        .await
        .map_err(|e| match e {
            SetError::NoKey | SetError::TooLarge(_) => ApiError::BadRequest(e.to_string()),
            SetError::Encrypt(_) | SetError::Read(_) => {
                tracing::error!(error = %e, "setting an agent secret failed");
                ApiError::Internal("setting the secret failed".into())
            }
            SetError::Db(e) => ApiError::from(e),
        })?;
    let written = match env {
        Some(variable) => write_shell_env(&state, &agent, |stored| {
            stored.insert(variable.to_string(), reference.clone());
        })
        .await
        .map(Some),
        None => Ok(None),
    };
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "secret.set",
        serde_json::json!({
            "name": name,
            "was": was.as_str(),
            "env": if written.as_ref().is_ok_and(Option::is_some) { env } else { None },
        }),
    )
    .await;
    state.secrets.push(&state.registry, agent.id).await;
    let agent = match written {
        Ok(Some(updated)) => updated,
        Ok(None) => agent,
        Err(e) => {
            return Err(ApiError::Conflict(format!(
                "the secret {name} is set, but shell commands do not get it as ${}: {e}",
                env.unwrap_or_default()
            )));
        }
    };
    Ok(Json(view(&state, &agent).await?))
}

/// `DELETE /agents/:id/secrets/:name` — remove the secret, or the agent's
/// request for it. 404 when there is neither.
pub async fn remove(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<SecretsView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let was = state.secrets.remove(agent.id, &name).await?;
    if was == Was::Absent {
        return Err(ApiError::NotFound);
    }
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "secret.removed",
        serde_json::json!({"name": name, "was": was.as_str()}),
    )
    .await;
    state.secrets.push(&state.registry, agent.id).await;
    Ok(Json(view(&state, &agent).await?))
}

#[derive(Debug, Deserialize)]
pub struct ShellEnvRequest {
    pub env: BTreeMap<String, String>,
}

/// `PUT /agents/:id/shell-env` with `{env}` — replace the variables shell
/// commands get.
pub async fn put_shell_env(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(body): Json<ShellEnvRequest>,
) -> Result<Json<SecretsView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    secrets::check_shell_env(&body.env).map_err(ApiError::BadRequest)?;
    let agent = write_shell_env(&state, &agent, |stored| stored.clone_from(&body.env)).await?;
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.shell_env",
        serde_json::json!({"variables": body.env.keys().collect::<Vec<_>>()}),
    )
    .await;
    state.secrets.push(&state.registry, agent.id).await;
    Ok(Json(view(&state, &agent).await?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_db::repo::agents::{NewAgent, create as create_agent};
    use havn_db::repo::audit::{self as audit_repo, ListFilter};
    use havn_db::repo::users::{NewUser, create as create_user};

    async fn setup() -> (AppState, AuthedUser, AuthedUser, Agent) {
        let (mut state, _) = crate::auth::tests::state_with(false).await;
        state.secrets = crate::secrets::SecretStore::new(
            state.db.clone(),
            Some(crate::keyring::KeyRing::from_passphrase("secrets-api-test")),
        );
        let owner = create_user(&state.db, NewUser { display_name: "o" })
            .await
            .expect("owner");
        let other = create_user(&state.db, NewUser { display_name: "x" })
            .await
            .expect("other");
        let agent = create_agent(
            &state.db,
            NewAgent {
                owner_id: owner.id,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        (state, owner.into(), other.into(), agent)
    }

    fn body(value: &str, env: Option<&str>) -> Json<SetRequest> {
        Json(SetRequest {
            value: value.into(),
            env: env.map(Into::into),
        })
    }

    #[tokio::test]
    async fn only_the_owner_sets_and_removes_and_no_value_comes_back() {
        let (state, owner, other, agent) = setup().await;
        let id = agent.id.to_string();
        let path = |name: &str| Path((id.clone(), name.to_string()));

        let refused = set(
            State(state.clone()),
            other.clone(),
            path("github"),
            body("ghp_value_0123456789", None),
        )
        .await;
        assert!(matches!(refused, Err(ApiError::NotFound)));
        assert!(matches!(
            list(State(state.clone()), other.clone(), Path(id.clone())).await,
            Err(ApiError::NotFound)
        ));

        let Json(view) = set(
            State(state.clone()),
            owner.clone(),
            path("github"),
            body("ghp_value_0123456789", Some("GH_TOKEN")),
        )
        .await
        .expect("set");
        assert_eq!(view.secrets.len(), 1);
        assert!(view.secrets[0].set);
        assert_eq!(view.secrets[0].variables, vec!["GH_TOKEN".to_string()]);
        assert_eq!(view.shell_env["GH_TOKEN"], "secret:github");
        let json = serde_json::to_string(&view).expect("json");
        assert!(!json.contains("ghp_value_0123456789"));

        let bad = set(
            State(state.clone()),
            owner.clone(),
            path("bad name"),
            body("ghp_value_0123456789", None),
        )
        .await;
        assert!(matches!(bad, Err(ApiError::BadRequest(_))));

        assert!(matches!(
            remove(State(state.clone()), other.clone(), path("github")).await,
            Err(ApiError::NotFound)
        ));
        let Json(view) = remove(State(state.clone()), owner.clone(), path("github"))
            .await
            .expect("removed");
        assert!(view.secrets.is_empty());
        assert!(matches!(
            remove(State(state.clone()), owner.clone(), path("github")).await,
            Err(ApiError::NotFound)
        ));

        let rows = audit_repo::list(
            &state.db,
            ListFilter {
                action_prefix: Some("secret.".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("audit");
        let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
        assert!(actions.contains(&"secret.set") && actions.contains(&"secret.removed"));
        for row in &rows {
            assert_eq!(row.details["name"], "github");
            assert!(!row.details.to_string().contains("ghp_value_0123456789"));
        }
    }

    #[tokio::test]
    async fn the_shell_environment_is_checked_and_kept() {
        let (state, owner, _, agent) = setup().await;
        let id = agent.id.to_string();
        let env = |pairs: &[(&str, &str)]| {
            Json(ShellEnvRequest {
                env: pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            })
        };
        let bad = put_shell_env(
            State(state.clone()),
            owner.clone(),
            Path(id.clone()),
            env(&[("HOME", "/x")]),
        )
        .await;
        assert!(matches!(bad, Err(ApiError::BadRequest(_))));
        let Json(view) = put_shell_env(
            State(state.clone()),
            owner.clone(),
            Path(id.clone()),
            env(&[("GH_TOKEN", "secret:github"), ("REGION", "eu")]),
        )
        .await
        .expect("put");
        assert_eq!(view.shell_env.len(), 2);
        let stored = havn_db::repo::agents::find_by_id(&state.db, agent.id)
            .await
            .expect("find")
            .expect("agent");
        assert_eq!(stored.config["shell_env"]["REGION"], "eu");
        let Json(view) = put_shell_env(State(state.clone()), owner, Path(id), env(&[]))
            .await
            .expect("cleared");
        assert!(view.shell_env.is_empty());
    }
}
