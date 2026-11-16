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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vision: bool,
}

#[derive(Debug, Deserialize)]
pub struct ModelsView {
    pub version: String,
    pub default_model: Option<String>,
    pub effective_default: Option<String>,
    #[serde(default)]
    pub title_model: Option<String>,
    pub models: Vec<ModelEntry>,
}

// `version` is the one the list was read at: the gateway refuses the write
// if the list changed since.
// `title_model` is sent only when it changes (`Some`, with `None` to clear
// it): a gateway from before title models refuses the field, and one since
// keeps its own when the field is left out.
async fn save(
    gw: &Gateway,
    version: &str,
    default_model: Option<&str>,
    title_model: Option<Option<&str>>,
    models: &[ModelEntry],
) -> anyhow::Result<ModelsView> {
    let mut body = json!({ "version": version, "default_model": default_model, "models": models });
    if let Some(title) = title_model {
        body["title_model"] = json!(title);
    }
    gw.put_json("/llm/models", &body).await
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
        let title = if view.title_model.as_deref() == Some(m.id.as_str()) {
            "  (titles)"
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
        let vision = if m.vision { "  sees images" } else { "" };
        println!("{}{label}{limit}{vision}{default}{title}", m.id);
    }
    Ok(())
}

pub async fn add(
    id: &str,
    label: Option<&str>,
    max_output_tokens: Option<u32>,
    vision: bool,
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
        vision,
    });
    let default_model = if make_default {
        Some(id)
    } else {
        view.default_model.as_deref()
    };
    let saved = save(&gw, &view.version, default_model, None, &models).await?;
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
    let title_model = (view.title_model.as_deref() == Some(id)).then_some(None);
    let saved = save(&gw, &view.version, default_model, title_model, &models).await?;
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
    save(&gw, &view.version, Some(id), None, &view.models).await?;
    println!("default model: {id}");
    Ok(())
}

/// `havn model title [ID]`: the model that writes chats' titles, or each
/// agent's own model when `id` is `None`.
pub async fn set_title(id: Option<&str>) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let view: ModelsView = gw.get_json("/llm/models").await?;
    if let Some(id) = id
        && !view.models.iter().any(|m| m.id == id)
    {
        anyhow::bail!("{id} is not listed; add it with `havn model add {id}`");
    }
    save(
        &gw,
        &view.version,
        view.default_model.as_deref(),
        Some(id),
        &view.models,
    )
    .await?;
    match id {
        Some(id) => {
            println!("chat titles are written by {id} (agents use it from their next start)")
        }
        None => println!(
            "chat titles are written by each agent's own model (agents use it from their next start)"
        ),
    }
    Ok(())
}
