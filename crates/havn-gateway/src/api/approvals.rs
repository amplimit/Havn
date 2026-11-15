//! `/agents/{id}/approvals`: the tools whose every call waits for the
//! user's approval (issue #127), kept in the agent's policy override as
//! hook rules asking `(?s).*`. A name may end in `*` to cover a prefix,
//! such as all of one MCP server's tools (`mcp__github__*`). Other hook
//! rules of the override are left as they are; rules from the owner's role
//! are shown and cannot be removed here. Applies on the agent's next start.

use axum::Json;
use axum::extract::{Path, State};
use havn_core::policy::ASK_EVERY_CALL;
use havn_core::{Policy, ToolHookRule};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::AuthedUser;
use crate::policy_resolver;

/// Most tool names in the list.
const MAX_TOOLS: usize = 32;

#[derive(Debug, Serialize)]
pub struct ApprovalsView {
    /// The agent's own list.
    pub tools: Vec<String>,
    /// From the owner's role; not removable here.
    pub inherited: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalsPut {
    pub tools: Vec<String>,
}

fn asks_every_call(rule: &ToolHookRule) -> bool {
    rule.deny.is_empty() && rule.ask.len() == 1 && rule.ask[0] == ASK_EVERY_CALL
}

fn always_asked(policy: &Policy) -> Vec<String> {
    policy
        .hooks
        .pre_tool_use
        .iter()
        .filter(|r| asks_every_call(r))
        .map(|r| r.tool.clone())
        .collect()
}

/// The override's hook rules, from `config.policy.hooks`.
fn override_rules(config: &Value) -> Result<Vec<ToolHookRule>, ApiError> {
    match config.pointer("/policy/hooks/pre_tool_use") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            ApiError::BadRequest(format!("the agent's stored hook rules do not parse: {e}"))
        }),
    }
}

async fn view(
    state: &AppState,
    agent: &havn_db::repo::agents::Agent,
) -> Result<ApprovalsView, ApiError> {
    let owner = policy_resolver::for_owner(&state.db, agent).await;
    let inherited = always_asked(&owner);
    let tools = override_rules(&agent.config)?
        .into_iter()
        .filter(asks_every_call)
        .map(|r| r.tool)
        .filter(|t| !inherited.contains(t))
        .collect();
    Ok(ApprovalsView { tools, inherited })
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<ApprovalsView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    Ok(Json(view(&state, &agent).await?))
}

/// Replace the agent's list.
pub async fn put(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<ApprovalsPut>,
) -> Result<Json<ApprovalsView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let mut tools: Vec<String> = Vec::new();
    for tool in &req.tools {
        let tool = tool.trim();
        let name = tool.strip_suffix('*').unwrap_or(tool);
        let valid = !name.is_empty()
            && name.len() <= 128
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if !valid {
            return Err(ApiError::BadRequest(format!(
                "{tool:?} is not a tool name such as shell or mcp__github__*"
            )));
        }
        if !tools.iter().any(|t| t == tool) {
            tools.push(tool.to_string());
        }
    }
    if tools.len() > MAX_TOOLS {
        return Err(ApiError::BadRequest(format!("more than {MAX_TOOLS} tools")));
    }
    let updated = havn_db::repo::agents::update_config(&state.db, agent.id, None, |current| {
        let mut rules: Vec<ToolHookRule> = override_rules(current)?
            .into_iter()
            .filter(|r| !asks_every_call(r))
            .collect();
        rules.extend(tools.iter().map(|tool| ToolHookRule {
            tool: tool.clone(),
            deny: Vec::new(),
            ask: vec![ASK_EVERY_CALL.to_string()],
        }));
        let mut config = current.as_object().cloned().unwrap_or_default();
        let mut policy = config
            .get("policy")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut hooks = policy
            .get("hooks")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        hooks.insert(
            "pre_tool_use".into(),
            serde_json::to_value(&rules).map_err(|e| ApiError::BadRequest(e.to_string()))?,
        );
        policy.insert("hooks".into(), Value::Object(hooks));
        let policy = Value::Object(policy);
        serde_json::from_value::<Policy>(policy.clone())
            .map_err(|e| ApiError::BadRequest(format!("config.policy: {e}")))?
            .validate()?;
        config.insert("policy".into(), policy);
        Ok::<_, ApiError>(Value::Object(config))
    })
    .await??;
    audit::record_agent_action(
        &state.db,
        user.id,
        agent.id,
        "agent.approvals.updated",
        serde_json::json!({ "tools": tools }),
    )
    .await;
    Ok(Json(view(&state, &updated).await?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn only_ask_every_call_rules_count() {
        let rules = override_rules(&json!({"policy": {"hooks": {"pre_tool_use": [
            {"tool": "shell", "ask": [ASK_EVERY_CALL]},
            {"tool": "shell", "deny": ["^rm"]},
            {"tool": "web_fetch", "ask": ["internal"]}
        ]}}}))
        .unwrap();
        let counted: Vec<&str> = rules
            .iter()
            .filter(|r| asks_every_call(r))
            .map(|r| r.tool.as_str())
            .collect();
        assert_eq!(counted, ["shell"]);
        assert!(override_rules(&json!({})).unwrap().is_empty());
        assert!(override_rules(&json!({"policy": {"hooks": {"pre_tool_use": "x"}}})).is_err());
    }
}
