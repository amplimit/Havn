//! `havn model ...` — the gateway's model list (`[[llm.models]]` in its
//! config.toml) through the gateway REST API, which writes the file.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::Gateway;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ModelsView {
    pub version: String,
    pub default_model: Option<String>,
    pub effective_default: Option<String>,
    pub models: Vec<ModelEntry>,
}

// `version` is the one the list was read at: the gateway refuses the write
// if the list changed since.
async fn save(
    gw: &Gateway,
    version: &str,
    default_model: Option<&str>,
    models: &[ModelEntry],
) -> anyhow::Result<ModelsView> {
    gw.put_json(
        "/llm/models",
        &json!({ "version": version, "default_model": default_model, "models": models }),
    )
    .await
}

pub async fn list() -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let view: ModelsView = gw.get_json("/llm/models").await?;
    if view.models.is_empty() {
        println!(
            "no models — `havn model add <provider>:<model>` to add one, e.g. \
             `havn model add anthropic:claude-sonnet-4-6`"
        );
        return Ok(());
    }
    for m in &view.models {
        let default = if view.effective_default.as_deref() == Some(m.id.as_str()) {
            "  (default)"
        } else {
            ""
        };
        let label = m
            .label
            .as_deref()
            .map(|l| format!("  {l}"))
            .unwrap_or_default();
        let limit = m
            .max_output_tokens
            .map(|n| format!("  max_output_tokens={n}"))
            .unwrap_or_default();
        println!("{}{label}{limit}{default}", m.id);
    }
    Ok(())
}

pub async fn add(
    id: &str,
    label: Option<&str>,
    max_output_tokens: Option<u32>,
    make_default: bool,
) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let view: ModelsView = gw.get_json("/llm/models").await?;
    let mut models = view.models;
    if models.iter().any(|m| m.id == id) {
        anyhow::bail!("{id} is already listed");
    }
    models.push(ModelEntry {
        id: id.to_string(),
        label: label.map(str::to_string),
        max_output_tokens,
    });
    let default_model = if make_default {
        Some(id)
    } else {
        view.default_model.as_deref()
    };
    let saved = save(&gw, &view.version, default_model, &models).await?;
    println!(
        "added {id}; default model: {}",
        saved.effective_default.as_deref().unwrap_or("none")
    );
    Ok(())
}

pub async fn remove(id: &str) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let view: ModelsView = gw.get_json("/llm/models").await?;
    let mut models = view.models;
    let before = models.len();
    models.retain(|m| m.id != id);
    if models.len() == before {
        anyhow::bail!("{id} is not listed");
    }
    let default_model = view.default_model.as_deref().filter(|d| *d != id);
    let saved = save(&gw, &view.version, default_model, &models).await?;
    println!(
        "removed {id}; default model: {}",
        saved.effective_default.as_deref().unwrap_or("none")
    );
    Ok(())
}

pub async fn set_default(id: &str) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let view: ModelsView = gw.get_json("/llm/models").await?;
    if !view.models.iter().any(|m| m.id == id) {
        anyhow::bail!("{id} is not listed; add it with `havn model add {id}`");
    }
    save(&gw, &view.version, Some(id), &view.models).await?;
    println!("default model: {id}");
    Ok(())
}
