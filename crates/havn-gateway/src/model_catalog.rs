use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, value};
use tracing::{error, info};

const MAX_ID_LEN: usize = 256;
const MAX_LABEL_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    if id.len() > MAX_ID_LEN || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (provider, model) = id.split_once(':')?;
    (!model.is_empty() && crate::llm_proxy::is_provider(provider)).then_some((provider, model))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
                    "[[llm.models]] id {:?} must be provider:model, without spaces, with a \
                     provider of {}",
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
            if let Some(label) = &entry.label
                && (label.len() > MAX_LABEL_LEN || label.chars().any(char::is_control))
            {
                anyhow::bail!(
                    "[[llm.models]] {:?}: label must be at most {MAX_LABEL_LEN} bytes without \
                     control characters",
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

    // Changes whenever the list or the default does; a writer passes the
    // version it read so that an edit made since then is not overwritten.
    #[must_use]
    pub fn version(&self) -> String {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileLlm {
    #[serde(default)]
    default_model: Option<String>,
    #[serde(default)]
    models: Vec<ModelEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    llm: FileLlm,
}

// The list as the config file at `path` has it now; empty for a missing file.
#[cfg(test)]
pub fn read_from_file(path: &Path) -> anyhow::Result<ModelCatalog> {
    use anyhow::Context as _;
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ModelCatalog::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse_catalog(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn parse_catalog(raw: &str) -> anyhow::Result<ModelCatalog> {
    let file: FileConfig = toml::from_str(raw)?;
    Ok(ModelCatalog {
        default_model: file.llm.default_model,
        models: file.llm.models,
    })
}

fn visit_tables(table: &mut Table, f: &mut dyn FnMut(&mut Table)) {
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(t) => {
                f(t);
                visit_tables(t, f);
            }
            Item::ArrayOfTables(a) => {
                for t in a.iter_mut() {
                    f(t);
                    visit_tables(t, f);
                }
            }
            _ => {}
        }
    }
}

fn entry_table(value: toml_edit::Value) -> anyhow::Result<Table> {
    match value {
        toml_edit::Value::InlineTable(t) => Ok(t.into_table()),
        _ => anyhow::bail!("an [llm] models entry is not a table"),
    }
}

// Write `catalog` into the `[llm]` table of the config file at `path` (the
// file a symlink points to), keeping the rest of the file, comments
// included, and the comments of entries that stay. The entries go right
// after the other `[llm]` tables, or before the first table when there are
// none. A missing file is created.
pub fn write_to_file(path: &Path, catalog: &ModelCatalog) -> anyhow::Result<()> {
    write_checked(path, catalog, None).map(|_| ())
}

// Like `write_to_file`, but only if the file's list still has `version`;
// false when it does not. The check uses the same read as the write.
pub fn write_if_unchanged(
    path: &Path,
    version: &str,
    catalog: &ModelCatalog,
) -> anyhow::Result<bool> {
    write_checked(path, catalog, Some(version))
}

fn write_checked(
    path: &Path,
    catalog: &ModelCatalog,
    expected: Option<&str>,
) -> anyhow::Result<bool> {
    use anyhow::Context as _;

    catalog.validate()?;
    let target: PathBuf = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e).with_context(|| format!("resolving {}", path.display())),
    };
    let raw = match std::fs::read_to_string(&target) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", target.display())),
    };
    if let Some(expected) = expected {
        let current =
            parse_catalog(&raw).with_context(|| format!("parsing {}", target.display()))?;
        if current.version() != expected {
            return Ok(false);
        }
    }
    let mut doc: DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", target.display()))?;

    let llm_item = doc.entry("llm").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    if let Some(inline) = llm_item.as_inline_table_mut() {
        let table = std::mem::replace(inline, InlineTable::new()).into_table();
        *llm_item = Item::Table(table);
    }
    let llm = llm_item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("llm in {} is not a table", target.display()))?;

    let mut previous: Vec<Table> = match llm.remove("models") {
        Some(Item::ArrayOfTables(old)) => old.into_iter().collect(),
        Some(Item::Value(toml_edit::Value::Array(old))) => old
            .into_iter()
            .map(entry_table)
            .collect::<anyhow::Result<_>>()?,
        Some(Item::None) | None => Vec::new(),
        Some(_) => anyhow::bail!(
            "[llm] models in {} is not a list of [[llm.models]] tables",
            target.display()
        ),
    };
    match &catalog.default_model {
        Some(id) => llm["default_model"] = value(id.as_str()),
        None => {
            llm.remove("default_model");
        }
    }
    if catalog.models.is_empty() {
        // An empty array of tables would not be written at all, and a file
        // without the key looks like one from before the model list.
        llm.insert("models", value(toml_edit::Array::new()));
    }
    let header = !llm.is_dotted() && llm.iter().any(|(_, item)| item.is_value());
    if header {
        llm.set_implicit(false);
    }
    let mut after_llm: Option<isize> = llm.position();
    visit_tables(llm, &mut |t| {
        after_llm = after_llm.max(t.position());
    });
    let place_llm = header && llm.position().is_none();

    let mut first_table: Option<isize> = None;
    visit_tables(doc.as_table_mut(), &mut |t| {
        if let Some(p) = t.position() {
            first_table = Some(first_table.map_or(p, |f| f.min(p)));
        }
    });
    let at = after_llm.map(|p| p + 1).or(first_table);
    let count =
        isize::try_from(catalog.models.len()).unwrap_or(isize::MAX / 2) + isize::from(place_llm);
    if let Some(at) = at {
        visit_tables(doc.as_table_mut(), &mut |t| {
            if let Some(p) = t.position()
                && p >= at
            {
                t.set_position(Some(p + count));
            }
        });
    }

    let llm = doc["llm"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("llm in {} is not a table", target.display()))?;
    let mut next = at;
    if place_llm {
        llm.set_position(next);
        next = next.map(|p| p + 1);
    }
    if !catalog.models.is_empty() {
        let mut models = ArrayOfTables::new();
        for entry in &catalog.models {
            let kept = previous
                .iter()
                .position(|t| t.get("id").and_then(Item::as_str) == Some(entry.id.as_str()));
            let mut table = kept.map_or_else(Table::new, |i| previous.swap_remove(i));
            table.set_position(next);
            next = next.map(|p| p + 1);
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
        llm.insert("models", Item::ArrayOfTables(models));
    }

    replace_file(&target, doc.to_string().as_bytes())?;
    Ok(true)
}

// Write a sibling file, flush it, give it the original's owner and mode,
// and rename it over `target`.
fn replace_file(target: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        target
            .file_name()
            .map_or_else(|| "config.toml".into(), |n| n.to_string_lossy()),
        std::process::id()
    ));
    let original = std::fs::metadata(target).ok();
    let written = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        if let Some(meta) = &original {
            // Best effort: an unprivileged gateway cannot hand the file to
            // another owner, and keeps the mode either way.
            #[cfg(unix)]
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
            std::fs::set_permissions(&tmp, meta.permissions())?;
        } else {
            // A new file gets the usual config mode, readable by the CLI
            // run as another user.
            #[cfg(unix)]
            std::fs::set_permissions(
                &tmp,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
            )?;
        }
        std::fs::rename(&tmp, target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", target.display()))
}

fn lists_models(path: &Path) -> anyhow::Result<bool> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let doc: DocumentMut = raw.parse()?;
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

// A name that already starts with a provider is left as it is, so a second
// run changes nothing.
fn upgraded(name: &str) -> Option<String> {
    let named = name
        .split_once(':')
        .is_some_and(|(provider, _)| crate::llm_proxy::is_provider(provider));
    (!named).then(|| legacy_model_ref(name))
}

fn upgrade_allowed_models(policy: &mut serde_json::Value) -> bool {
    let Some(list) = policy
        .get_mut("allowed_models")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let before = list.len();
    list.retain(|m| m.as_str().is_none_or(|m| !m.trim().is_empty()));
    let mut changed = list.len() != before;
    for entry in list {
        if let Some(new) = entry.as_str().filter(|m| *m != "*").and_then(upgraded) {
            *entry = serde_json::Value::String(new);
            changed = true;
        }
    }
    changed
}

// One-time upgrade of a database from before the model list, which
// migration 0010 marks with a row in `model_names_upgrade` (a new database
// never gets one): model names without a provider in agent configs, agent
// policy overrides and role policies are rewritten with the provider older
// gateways inferred. Then, if config.toml has no `[llm] models` at all, the
// models agents use are written there, led by the model older gateways gave
// agents without one, if any agent has none. Returns the list to run with.
// When that write fails the marker stays, so the next start tries again.
pub async fn upgrade(
    db: &SqlitePool,
    config_path: &Path,
    catalog: ModelCatalog,
) -> anyhow::Result<ModelCatalog> {
    let pending: Option<(i64,)> = sqlx::query_as("SELECT id FROM model_names_upgrade")
        .fetch_optional(db)
        .await?;
    if pending.is_none() {
        return Ok(catalog);
    }
    let mut tx = db.begin().await?;
    let agents: Vec<(String, String)> =
        sqlx::query_as("SELECT id, config FROM agents ORDER BY created_at, id")
            .fetch_all(&mut *tx)
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
            Some(model) if model.trim().is_empty() => {
                if let Some(obj) = config.as_object_mut() {
                    obj.remove("model");
                }
                changed = true;
                agent_without_model = true;
            }
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
                .execute(&mut *tx)
                .await?;
            rewritten += 1;
        }
    }
    let roles: Vec<(String, String)> = sqlx::query_as("SELECT id, policy FROM roles")
        .fetch_all(&mut *tx)
        .await?;
    for (id, raw) in roles {
        let Ok(mut policy) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if upgrade_allowed_models(&mut policy) {
            sqlx::query("UPDATE roles SET policy = ? WHERE id = ?")
                .bind(policy.to_string())
                .bind(&id)
                .execute(&mut *tx)
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

    let mut result = catalog;
    let mut done = true;
    if !lists_models(config_path)? {
        let mut seeded: Vec<String> = Vec::new();
        if agent_without_model {
            seeded.push(LEGACY_DEFAULT_MODEL.to_string());
        }
        for model in used {
            if !seeded.contains(&model) && split_model_ref(&model).is_some() {
                seeded.push(model);
            }
        }
        if !seeded.is_empty() {
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
                Ok(()) => {
                    info!(
                        path = %config_path.display(),
                        models = ?ids,
                        "added the models existing agents use to [[llm.models]]"
                    );
                    result = catalog;
                }
                Err(e) => {
                    error!(
                        path = %config_path.display(),
                        models = ?ids,
                        error = %format!("{e:#}"),
                        "could not add the models existing agents use to [[llm.models]]; \
                         agents cannot chat until they are listed there (the next start \
                         tries again)"
                    );
                    done = false;
                }
            }
        }
    }
    if done {
        sqlx::query("DELETE FROM model_names_upgrade")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(result)
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

    fn parsed(path: &Path) -> crate::config::GatewayConfig {
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn writing_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# operator notes\nlisten = \"127.0.0.1:8080\"\n\n[llm]\n# ceiling comment\n\
             max_tokens_ceiling = 64000\n\n[[llm.models]]\n# keep me\nid = \"openai:gpt-5\"\n\n\
             [llm.base_url]\nanthropic = \"http://127.0.0.1:1\"\n\n[trust_header]\n\
             enabled = false\n# match_user_id = true\n",
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
        // The entries stay with the [llm] tables; the comment that ends the
        // file stays after [trust_header].
        let trust = text.find("[trust_header]").unwrap();
        assert!(text.rfind("[[llm.models]]").unwrap() < trust, "{text}");
        assert!(text.find("# match_user_id").unwrap() > trust, "{text}");
        assert!(
            text.find("id = \"anthropic:claude-opus-4-7\"").unwrap()
                < text.find("id = \"openai:gpt-5\"").unwrap(),
            "{text}"
        );
        let cfg = parsed(&path);
        assert_eq!(cfg.llm.catalog(), c);
        assert_eq!(cfg.llm.max_tokens_ceiling, 64_000);
        assert!(!cfg.trust_header.enabled);
        assert_eq!(read_from_file(&path).unwrap(), c);

        write_to_file(&path, &catalog(&[], None)).unwrap();
        assert_eq!(parsed(&path).llm.catalog(), ModelCatalog::default());
        assert!(lists_models(&path).unwrap(), "an emptied list stays a list");
        write_to_file(&path, &catalog(&["openai:gpt-5"], None)).unwrap();
        assert_eq!(
            parsed(&path).llm.catalog(),
            catalog(&["openai:gpt-5"], None)
        );
    }

    #[test]
    fn entries_go_before_the_first_table_without_an_llm_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "listen = \"127.0.0.1:8080\"\n\n[reload]\ndebounce_ms = 500\n# reload note\n",
        )
        .unwrap();
        let c = catalog(&["openai:gpt-5"], Some("openai:gpt-5"));
        write_to_file(&path, &c).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("listen ="), "{text}");
        let reload = text.find("[reload]").unwrap();
        let header = text.find("[llm]").unwrap();
        let entry = text.find("[[llm.models]]").unwrap();
        assert!(header < entry && entry < reload, "{text}");
        assert!(text.find("# reload note").unwrap() > reload, "{text}");
        assert_eq!(parsed(&path).llm.catalog(), c);
        assert_eq!(parsed(&path).reload.debounce_ms, 500);
    }

    #[test]
    fn inline_llm_tables_and_entries_are_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "llm = { max_tokens_ceiling = 1000 }\n").unwrap();
        let c = catalog(&["openai:gpt-5"], None);
        write_to_file(&path, &c).unwrap();
        assert_eq!(parsed(&path).llm.catalog(), c);
        assert_eq!(parsed(&path).llm.max_tokens_ceiling, 1000);

        std::fs::write(
            &path,
            "[llm]\nmodels = [{ id = \"openai:gpt-5\", label = \"x\" }]\n",
        )
        .unwrap();
        let c = catalog(&["gemini:gemini-2.5-pro", "openai:gpt-5"], None);
        write_to_file(&path, &c).unwrap();
        assert_eq!(parsed(&path).llm.catalog(), c);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("managed.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&real, "listen = \"127.0.0.1:8080\"\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let c = catalog(&["openai:gpt-5"], None);
        write_to_file(&link, &c).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read_from_file(&real).unwrap(), c);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_mode_is_kept_and_no_temp_file_is_left() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_to_file(&path, &catalog(&["openai:gpt-5"], None)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn writing_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        let c = catalog(&["openrouter:deepseek/deepseek-r1:free"], None);
        write_to_file(&path, &c).unwrap();
        assert_eq!(parsed(&path).llm.catalog(), c);
        assert_eq!(
            read_from_file(&dir.path().join("none.toml")).unwrap(),
            ModelCatalog::default()
        );
    }

    #[test]
    fn the_version_follows_the_list_and_the_default() {
        let a = catalog(&["openai:gpt-5", "gemini:gemini-2.5-pro"], None);
        let mut b = a.clone();
        assert_eq!(a.version(), b.version());
        b.models.reverse();
        assert_ne!(a.version(), b.version());
        let mut c = a.clone();
        c.default_model = Some("openai:gpt-5".into());
        assert_ne!(a.version(), c.version());
    }

    #[test]
    fn ids_with_spaces_or_control_characters_are_rejected() {
        for id in [
            "anthropic:claude-x ",
            "anthropic: claude-x",
            "anthropic:a\u{7}",
        ] {
            assert!(split_model_ref(id).is_none(), "{id:?}");
        }
        assert!(split_model_ref(&format!("openai:{}", "x".repeat(300))).is_none());
        let mut c = catalog(&["openai:gpt-5"], None);
        c.models[0].label = Some("bad\nlabel".into());
        assert!(c.validate().is_err());
    }

    async fn db() -> SqlitePool {
        havn_db::connect_in_memory().await.unwrap()
    }

    async fn marked(db: &SqlitePool) -> bool {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM model_names_upgrade")
            .fetch_optional(db)
            .await
            .unwrap();
        row.is_some()
    }

    async fn mark_upgrade(db: &SqlitePool) {
        sqlx::query("INSERT INTO model_names_upgrade (id) VALUES (1)")
            .execute(db)
            .await
            .unwrap();
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

    async fn stored(db: &SqlitePool) -> Vec<String> {
        let agents: Vec<(String,)> = sqlx::query_as("SELECT config FROM agents ORDER BY name")
            .fetch_all(db)
            .await
            .unwrap();
        let roles: Vec<(String,)> = sqlx::query_as("SELECT policy FROM roles ORDER BY id")
            .fetch_all(db)
            .await
            .unwrap();
        agents.into_iter().chain(roles).map(|(s,)| s).collect()
    }

    #[tokio::test]
    async fn upgrade_rewrites_old_names_and_seeds_the_list() {
        let db = db().await;
        insert_agent(
            &db,
            "a",
            serde_json::json!({"model": "deepseek/deepseek-r1:free",
                               "policy": {"allowed_models": ["*", "gpt-4o", ""]}}),
        )
        .await;
        insert_agent(&db, "b", serde_json::json!({"model": ""})).await;
        insert_agent(&db, "c", serde_json::json!({"model": "openai:gpt-5"})).await;
        sqlx::query("INSERT INTO roles (id, team_id, name, policy) VALUES (?, NULL, ?, ?)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind("legacy")
            .bind(serde_json::json!({"allowed_models": ["claude-sonnet-4-6"]}).to_string())
            .execute(&db)
            .await
            .unwrap();
        mark_upgrade(&db).await;
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
        assert_eq!(parsed(&path).llm.catalog(), c);

        let rows = stored(&db).await;
        let a: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
        assert_eq!(a["model"], "openrouter:deepseek/deepseek-r1:free");
        assert_eq!(
            a["policy"]["allowed_models"],
            serde_json::json!(["*", "openai:gpt-4o"])
        );
        let b: serde_json::Value = serde_json::from_str(&rows[1]).unwrap();
        assert!(b.get("model").is_none(), "an empty model is dropped: {b}");
        assert!(
            rows.iter()
                .any(|r| r.contains("\"anthropic:claude-sonnet-4-6\"")),
            "{rows:?}"
        );
        assert!(!marked(&db).await, "done once");

        // Running it again, as if the marker were back, changes nothing.
        mark_upgrade(&db).await;
        let again = upgrade(&db, &path, c.clone()).await.unwrap();
        assert_eq!(again, c);
        assert_eq!(stored(&db).await, rows);
    }

    // Migration 0010 again on the current rows, as on an older database.
    async fn marks(db: &SqlitePool) -> bool {
        sqlx::query("DROP TABLE model_names_upgrade")
            .execute(db)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../havn-db/migrations/0010_model_names_upgrade.sql"
        ))
        .execute(db)
        .await
        .unwrap();
        marked(db).await
    }

    #[tokio::test]
    async fn the_migration_marks_databases_with_agents_or_custom_roles() {
        let db = db().await;
        assert!(!marks(&db).await, "seeded roles alone need no upgrade");
        sqlx::query("INSERT INTO roles (id, team_id, name, policy) VALUES (?, NULL, ?, ?)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind("legacy")
            .bind(r#"{ "allowed_models" : [ "claude-sonnet-4-6" ] }"#)
            .execute(&db)
            .await
            .unwrap();
        assert!(marks(&db).await, "a role naming models");
        sqlx::query("DELETE FROM roles WHERE name = 'legacy'")
            .execute(&db)
            .await
            .unwrap();
        assert!(!marks(&db).await);
        insert_agent(&db, "a", serde_json::json!({})).await;
        assert!(marks(&db).await, "an agent");
    }

    #[test]
    fn a_conditional_write_needs_the_files_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let first = catalog(&["openai:gpt-5"], None);
        assert!(write_if_unchanged(&path, &ModelCatalog::default().version(), &first).unwrap());
        let other = catalog(&["gemini:gemini-2.5-pro"], None);
        assert!(!write_if_unchanged(&path, &ModelCatalog::default().version(), &other).unwrap());
        assert_eq!(read_from_file(&path).unwrap(), first);
        assert!(write_if_unchanged(&path, &first.version(), &other).unwrap());
        assert_eq!(read_from_file(&path).unwrap(), other);
    }

    #[cfg(unix)]
    #[test]
    fn a_created_file_is_readable_by_others() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_to_file(&path, &catalog(&["openai:gpt-5"], None)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[tokio::test]
    async fn an_existing_list_is_left_alone() {
        let db = db().await;
        insert_agent(&db, "a", serde_json::json!({"model": "gpt-4o"})).await;
        mark_upgrade(&db).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_to_file(&path, &catalog(&[], None)).unwrap();
        let c = upgrade(&db, &path, ModelCatalog::default()).await.unwrap();
        assert_eq!(c, ModelCatalog::default());
        assert_eq!(parsed(&path).llm.catalog(), ModelCatalog::default());
        assert!(!marked(&db).await);
    }

    #[tokio::test]
    async fn agents_of_a_fresh_install_are_never_seeded() {
        // A database created by this gateway has no marker, whatever agents
        // are created in it later.
        let db = db().await;
        assert!(!marked(&db).await);
        insert_agent(&db, "a", serde_json::json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = upgrade(&db, &path, ModelCatalog::default()).await.unwrap();
        assert_eq!(c, ModelCatalog::default());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_seed_write_keeps_the_marker() {
        use std::os::unix::fs::PermissionsExt as _;
        // Root writes into a read-only directory anyway.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let db = db().await;
        insert_agent(&db, "a", serde_json::json!({"model": "gpt-4o"})).await;
        mark_upgrade(&db).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let path = dir.path().join("config.toml");
        let c = upgrade(&db, &path, ModelCatalog::default()).await.unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(c, ModelCatalog::default());
        assert!(marked(&db).await);
        let a: (String,) = sqlx::query_as("SELECT config FROM agents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(
            a.0.contains("openai:gpt-4o"),
            "the rewrite is kept: {}",
            a.0
        );
    }
}
