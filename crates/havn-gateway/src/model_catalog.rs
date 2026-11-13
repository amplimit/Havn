use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

impl ModelEntry {
    #[must_use]
    pub fn provider(&self) -> &str {
        split_model_ref(&self.id).map_or("", |(provider, _)| provider)
    }

    #[must_use]
    pub fn model(&self) -> &str {
        split_model_ref(&self.id).map_or("", |(_, model)| model)
    }
}

#[must_use]
pub fn split_model_ref(id: &str) -> Option<(&str, &str)> {
    let (provider, model) = id.split_once(':')?;
    (!model.trim().is_empty() && crate::llm_proxy::is_provider(provider))
        .then_some((provider, model))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub default_model: Option<String>,
    pub models: Vec<ModelEntry>,
}

impl ModelCatalog {
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = HashSet::new();
        for entry in &self.models {
            if split_model_ref(&entry.id).is_none() {
                anyhow::bail!(
                    "[[llm.models]] id {:?} must be provider:model with a provider of {}",
                    entry.id,
                    crate::llm_proxy::PROVIDERS.join(", ")
                );
            }
            if !seen.insert(entry.id.as_str()) {
                anyhow::bail!("[[llm.models]] lists {:?} more than once", entry.id);
            }
            if entry.max_output_tokens == Some(0) {
                anyhow::bail!(
                    "[[llm.models]] {:?}: max_output_tokens must be at least 1",
                    entry.id
                );
            }
        }
        if let Some(default) = &self.default_model
            && !seen.contains(default.as_str())
        {
            anyhow::bail!("[llm] default_model {default:?} is not one of the [[llm.models]] ids");
        }
        Ok(())
    }

    #[must_use]
    pub fn default_entry(&self) -> Option<&ModelEntry> {
        match &self.default_model {
            Some(id) => self.get(id),
            None => self.models.first(),
        }
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn resolve(&self, requested: &str) -> Result<&ModelEntry, String> {
        if requested.is_empty() {
            return self.default_entry().ok_or_else(|| {
                "no model is configured: add one under [[llm.models]] in the gateway's \
                 config.toml, with `havn model add`, or on the dashboard"
                    .to_string()
            });
        }
        self.get(requested).ok_or_else(|| {
            format!("model {requested:?} is not in the gateway's model list ([[llm.models]])")
        })
    }
}

pub fn write_to_file(path: &Path, catalog: &ModelCatalog) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

    catalog.validate()?;
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let llm = doc
        .entry("llm")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[llm] in {} is not a table", path.display()))?;

    let mut previous: Vec<Table> = match llm.remove("models") {
        Some(Item::ArrayOfTables(old)) => old.into_iter().collect(),
        Some(Item::Value(toml_edit::Value::Array(old))) if old.is_empty() => Vec::new(),
        Some(Item::None) | None => Vec::new(),
        Some(_) => anyhow::bail!(
            "[llm] models in {} is not an array of [[llm.models]] tables",
            path.display()
        ),
    };
    let mut models = ArrayOfTables::new();
    for entry in &catalog.models {
        let kept = previous
            .iter()
            .position(|t| t.get("id").and_then(Item::as_str) == Some(entry.id.as_str()));
        let mut table = kept.map_or_else(Table::new, |i| previous.swap_remove(i));
        // Entries are written in list order, after the rest of the file.
        table.set_position(None);
        table["id"] = value(entry.id.as_str());
        match &entry.label {
            Some(label) => table["label"] = value(label.as_str()),
            None => {
                table.remove("label");
            }
        }
        match entry.max_output_tokens {
            Some(n) => table["max_output_tokens"] = value(i64::from(n)),
            None => {
                table.remove("max_output_tokens");
            }
        }
        models.push(table);
    }
    match &catalog.default_model {
        Some(id) => {
            llm["default_model"] = value(id.as_str());
            llm.set_implicit(false);
        }
        None => {
            llm.remove("default_model");
        }
    }
    if models.is_empty() {
        // An empty array of tables would not be written at all, and a file
        // without the key is seeded again on the next start.
        llm.insert("models", value(toml_edit::Array::new()));
    } else {
        llm.insert("models", Item::ArrayOfTables(models));
    }

    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map_or_else(|| "config.toml".into(), |n| n.to_string_lossy()),
        std::process::id()
    ));
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, meta.permissions())
            .with_context(|| format!("setting permissions on {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn lists_models(path: &Path) -> anyhow::Result<bool> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let doc: toml_edit::DocumentMut = raw.parse()?;
    Ok(doc.get("llm").and_then(|llm| llm.get("models")).is_some())
}

const LEGACY_DEFAULT_MODEL: &str = "anthropic:claude-opus-4-6";

#[must_use]
pub fn legacy_model_ref(name: &str) -> String {
    let provider = if name.starts_with("claude-") {
        "anthropic"
    } else if ["gpt-", "o1", "o3", "o4", "chatgpt-"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        "openai"
    } else if name.starts_with("gemini-") {
        "gemini"
    } else if name.contains('/') {
        "openrouter"
    } else {
        "anthropic"
    };
    format!("{provider}:{name}")
}

fn upgraded(name: &str) -> Option<String> {
    (split_model_ref(name).is_none()).then(|| legacy_model_ref(name))
}

fn upgrade_allowed_models(policy: &mut serde_json::Value) -> bool {
    let Some(list) = policy
        .get_mut("allowed_models")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for entry in list {
        if let Some(new) = entry.as_str().filter(|m| *m != "*").and_then(upgraded) {
            *entry = serde_json::Value::String(new);
            changed = true;
        }
    }
    changed
}

pub async fn upgrade(
    db: &SqlitePool,
    config_path: &Path,
    catalog: ModelCatalog,
) -> anyhow::Result<ModelCatalog> {
    let agents: Vec<(String, String)> = sqlx::query_as("SELECT id, config FROM agents")
        .fetch_all(db)
        .await?;
    let mut used: Vec<String> = Vec::new();
    let mut agent_without_model = false;
    let mut rewritten = 0usize;
    for (id, raw) in agents {
        let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let mut changed = false;
        match config.get("model").and_then(serde_json::Value::as_str) {
            Some(model) => {
                let model = match upgraded(model) {
                    Some(new) => {
                        config["model"] = serde_json::Value::String(new.clone());
                        changed = true;
                        new
                    }
                    None => model.to_string(),
                };
                if !used.contains(&model) {
                    used.push(model);
                }
            }
            None => agent_without_model = true,
        }
        if let Some(policy) = config.get_mut("policy") {
            changed |= upgrade_allowed_models(policy);
        }
        if changed {
            sqlx::query("UPDATE agents SET config = ? WHERE id = ?")
                .bind(config.to_string())
                .bind(&id)
                .execute(db)
                .await?;
            rewritten += 1;
        }
    }
    let roles: Vec<(String, String)> = sqlx::query_as("SELECT id, policy FROM roles")
        .fetch_all(db)
        .await?;
    for (id, raw) in roles {
        let Ok(mut policy) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if upgrade_allowed_models(&mut policy) {
            sqlx::query("UPDATE roles SET policy = ? WHERE id = ?")
                .bind(policy.to_string())
                .bind(&id)
                .execute(db)
                .await?;
            rewritten += 1;
        }
    }
    if rewritten > 0 {
        info!(
            rows = rewritten,
            "model names in agent configs and policies rewritten to provider:model"
        );
    }

    if lists_models(config_path)? {
        return Ok(catalog);
    }
    let mut seeded: Vec<String> = Vec::new();
    if agent_without_model {
        seeded.push(LEGACY_DEFAULT_MODEL.to_string());
    }
    for model in used {
        if !seeded.contains(&model) && split_model_ref(&model).is_some() {
            seeded.push(model);
        }
    }
    if seeded.is_empty() {
        return Ok(catalog);
    }
    let catalog = ModelCatalog {
        default_model: None,
        models: seeded
            .into_iter()
            .map(|id| ModelEntry {
                id,
                label: None,
                max_output_tokens: None,
            })
            .collect(),
    };
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    match write_to_file(config_path, &catalog) {
        Ok(()) => info!(
            path = %config_path.display(),
            models = ?ids,
            "added the models existing agents use to [[llm.models]]"
        ),
        Err(e) => warn!(
            path = %config_path.display(),
            models = ?ids,
            error = %e,
            "running with the models existing agents use, but could not add them to              [[llm.models]]; add them there to keep them after a restart"
        ),
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            label: None,
            max_output_tokens: None,
        }
    }

    fn catalog(ids: &[&str], default: Option<&str>) -> ModelCatalog {
        ModelCatalog {
            default_model: default.map(str::to_string),
            models: ids.iter().copied().map(entry).collect(),
        }
    }

    #[test]
    fn ids_split_at_the_first_colon() {
        assert_eq!(
            split_model_ref("openrouter:deepseek/deepseek-r1:free"),
            Some(("openrouter", "deepseek/deepseek-r1:free"))
        );
        assert_eq!(
            split_model_ref("anthropic:claude-opus-4-7"),
            Some(("anthropic", "claude-opus-4-7"))
        );
        assert_eq!(split_model_ref("claude-opus-4-7"), None);
        assert_eq!(split_model_ref("deepseek/deepseek-r1:free"), None);
        assert_eq!(split_model_ref("nobody:model"), None);
        assert_eq!(split_model_ref("openai:"), None);
    }

    #[test]
    fn validation_rejects_bad_ids_duplicates_and_an_unlisted_default() {
        assert!(
            catalog(&["anthropic:claude-opus-4-7"], None)
                .validate()
                .is_ok()
        );
        assert!(catalog(&["claude-opus-4-7"], None).validate().is_err());
        assert!(
            catalog(&["openai:gpt-5", "openai:gpt-5"], None)
                .validate()
                .is_err()
        );
        assert!(
            catalog(&["openai:gpt-5"], Some("openai:gpt-4o"))
                .validate()
                .is_err()
        );
        let mut zero = catalog(&["openai:gpt-5"], None);
        zero.models[0].max_output_tokens = Some(0);
        assert!(zero.validate().is_err());
    }

    #[test]
    fn the_default_is_the_named_entry_or_the_first() {
        let c = catalog(&["openai:gpt-5", "gemini:gemini-2.5-pro"], None);
        assert_eq!(c.resolve("").unwrap().id, "openai:gpt-5");
        let c = catalog(
            &["openai:gpt-5", "gemini:gemini-2.5-pro"],
            Some("gemini:gemini-2.5-pro"),
        );
        assert_eq!(c.resolve("").unwrap().id, "gemini:gemini-2.5-pro");
        assert!(ModelCatalog::default().resolve("").is_err());
    }

    #[test]
    fn only_listed_models_resolve() {
        let c = catalog(&["openai:gpt-5"], None);
        let e = c.resolve("openai:gpt-5").unwrap();
        assert_eq!((e.provider(), e.model()), ("openai", "gpt-5"));
        assert!(c.resolve("openai:gpt-4o").is_err());
        assert!(c.resolve("gpt-5").is_err());
    }

    #[test]
    fn legacy_names_get_the_provider_older_gateways_inferred() {
        assert_eq!(
            legacy_model_ref("claude-opus-4-7"),
            "anthropic:claude-opus-4-7"
        );
        assert_eq!(legacy_model_ref("gpt-4o"), "openai:gpt-4o");
        assert_eq!(legacy_model_ref("o3-mini"), "openai:o3-mini");
        assert_eq!(legacy_model_ref("gemini-2.5-pro"), "gemini:gemini-2.5-pro");
        assert_eq!(
            legacy_model_ref("deepseek/deepseek-r1:free"),
            "openrouter:deepseek/deepseek-r1:free"
        );
        assert_eq!(legacy_model_ref("mystery"), "anthropic:mystery");
    }

    #[test]
    fn writing_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# operator notes\nlisten = \"127.0.0.1:8080\"\n\n[llm]\n# ceiling comment\n\
             max_tokens_ceiling = 64000\n\n[[llm.models]]\n# keep me\nid = \"openai:gpt-5\"\n\n\
             [llm.base_url]\nanthropic = \"http://127.0.0.1:1\"\n",
        )
        .unwrap();
        let mut c = catalog(
            &["anthropic:claude-opus-4-7", "openai:gpt-5"],
            Some("openai:gpt-5"),
        );
        c.models[0].label = Some("Opus".into());
        write_to_file(&path, &c).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for kept in [
            "# operator notes",
            "# ceiling comment",
            "# keep me",
            "max_tokens_ceiling = 64000",
            "anthropic = \"http://127.0.0.1:1\"",
        ] {
            assert!(text.contains(kept), "{kept:?} missing from:\n{text}");
        }
        let parsed: crate::config::GatewayConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.llm.catalog(), c);
        assert_eq!(parsed.llm.max_tokens_ceiling, 64_000);

        write_to_file(&path, &catalog(&[], None)).unwrap();
        let parsed: crate::config::GatewayConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.llm.catalog(), ModelCatalog::default());
        assert!(lists_models(&path).unwrap(), "an emptied list stays a list");
    }

    #[test]
    fn writing_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        let c = catalog(&["openrouter:deepseek/deepseek-r1:free"], None);
        write_to_file(&path, &c).unwrap();
        let parsed: crate::config::GatewayConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.llm.catalog(), c);
    }

    async fn db() -> SqlitePool {
        havn_db::connect_in_memory().await.unwrap()
    }

    async fn insert_agent(db: &SqlitePool, name: &str, config: serde_json::Value) {
        let user = havn_db::repo::users::ensure_default(db).await.unwrap();
        sqlx::query("INSERT INTO agents (id, owner_id, name, config) VALUES (?, ?, ?, ?)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(user.id.to_string())
            .bind(name)
            .bind(config.to_string())
            .execute(db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn upgrade_rewrites_old_names_and_seeds_the_list() {
        let db = db().await;
        insert_agent(
            &db,
            "a",
            serde_json::json!({"model": "deepseek/deepseek-r1:free",
                               "policy": {"allowed_models": ["*", "gpt-4o"]}}),
        )
        .await;
        insert_agent(&db, "b", serde_json::json!({})).await;
        insert_agent(&db, "c", serde_json::json!({"model": "openai:gpt-5"})).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let c = upgrade(&db, &path, ModelCatalog::default()).await.unwrap();
        let ids: Vec<&str> = c.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                LEGACY_DEFAULT_MODEL,
                "openrouter:deepseek/deepseek-r1:free",
                "openai:gpt-5"
            ]
        );
        let parsed: crate::config::GatewayConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.llm.catalog(), c);

        let configs: Vec<(String,)> = sqlx::query_as("SELECT config FROM agents ORDER BY name")
            .fetch_all(&db)
            .await
            .unwrap();
        let a: serde_json::Value = serde_json::from_str(&configs[0].0).unwrap();
        assert_eq!(a["model"], "openrouter:deepseek/deepseek-r1:free");
        assert_eq!(
            a["policy"]["allowed_models"],
            serde_json::json!(["*", "openai:gpt-4o"])
        );

        let roles: Vec<(String,)> = sqlx::query_as("SELECT policy FROM roles")
            .fetch_all(&db)
            .await
            .unwrap();
        assert!(!roles.is_empty());
        for (raw,) in roles {
            let policy: serde_json::Value = serde_json::from_str(&raw).unwrap();
            for m in policy["allowed_models"].as_array().into_iter().flatten() {
                let m = m.as_str().unwrap();
                assert!(m == "*" || split_model_ref(m).is_some(), "{m}");
            }
        }

        write_to_file(&path, &catalog(&["openai:gpt-5"], None)).unwrap();
        let again = upgrade(&db, &path, catalog(&["openai:gpt-5"], None))
            .await
            .unwrap();
        assert_eq!(again, catalog(&["openai:gpt-5"], None));
    }

    #[tokio::test]
    async fn a_fresh_install_gets_no_list() {
        let db = db().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = upgrade(&db, &path, ModelCatalog::default()).await.unwrap();
        assert_eq!(c, ModelCatalog::default());
        assert!(!path.exists());
    }
}
