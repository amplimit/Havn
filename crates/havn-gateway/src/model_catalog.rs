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
    /// Whether it takes images (issues #106, #202): pictures sent in a chat
    /// are shown to it with `true`, never with `false`, and unset, tried:
    /// a model that refuses them is then sent a note in their place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// How it thinks, when it can (issue #190): absent, it is not asked to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingSpec>,
    /// Tokens it takes in one request, input and output (issue #214);
    /// unset, what its family is known to take ([`ModelEntry::context`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Models, by id, that answer in its place when it fails for now on
    /// every credential (issue #216), the first the agent's policy allows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
    /// What its tokens cost (issue #235), for the usage page; unset, its
    /// use is shown in tokens only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<ModelPrice>,
}

/// What a model's tokens cost, in US dollars per million tokens. Cache
/// reads and writes cost what input does when unset.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl ModelPrice {
    fn bits(&self) -> [Option<u64>; 4] {
        [
            Some(self.input.to_bits()),
            Some(self.output.to_bits()),
            self.cache_read.map(f64::to_bits),
            self.cache_write.map(f64::to_bits),
        ]
    }

    /// What `input` tokens (`cache_read` and `cache_write` of them from and
    /// to a cache) and `output` tokens cost, in US dollars.
    #[must_use]
    pub fn cost(&self, input: i64, cache_read: i64, cache_write: i64, output: i64) -> f64 {
        #[allow(clippy::cast_precision_loss, reason = "token counts, far below 2^52")]
        let count = |n: i64| n.max(0) as f64;
        let plain = count(input - cache_read - cache_write);
        (plain * self.input
            + count(cache_read) * self.cache_read.unwrap_or(self.input)
            + count(cache_write) * self.cache_write.unwrap_or(self.input)
            + count(output) * self.output)
            / 1_000_000.0
    }

    fn validate(&self, id: &str) -> anyhow::Result<()> {
        let prices = [
            Some(self.input),
            Some(self.output),
            self.cache_read,
            self.cache_write,
        ];
        if prices
            .into_iter()
            .flatten()
            .any(|p| !p.is_finite() || p.is_sign_negative())
        {
            anyhow::bail!("[[llm.models]] {id:?}: a price is a number of dollars, 0 or more");
        }
        Ok(())
    }
}

impl PartialEq for ModelPrice {
    fn eq(&self, other: &Self) -> bool {
        self.bits() == other.bits()
    }
}

impl Eq for ModelPrice {}

impl std::hash::Hash for ModelPrice {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bits().hash(state);
    }
}

/// How a model can be asked to think, as its admin set it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingSpec {
    pub style: ThinkingStyle,
    /// The levels it takes, for `effort`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
    /// The token budgets offered, for `budget`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub budgets: Vec<u32>,
    /// The choice of a chat that made none: `off`, `on`, a level or a
    /// budget. Unset, `off` (or the first choice, when it must think).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// It always thinks: `off` is not a choice.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mandatory: bool,
}

/// How a model's thinking is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingStyle {
    /// On or off.
    Toggle,
    /// By level, such as `low` / `medium` / `high`.
    Effort,
    /// By a number of tokens.
    Budget,
}

/// Longest level name, and most levels or budgets one model offers.
const MAX_LEVEL_LEN: usize = 16;
const MAX_CHOICES: usize = 8;

impl ThinkingSpec {
    /// What a chat may choose, `off` first unless it must think.
    #[must_use]
    pub fn choices(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.mandatory {
            out.push("off".to_string());
        }
        match self.style {
            ThinkingStyle::Toggle => out.push("on".to_string()),
            ThinkingStyle::Effort => out.extend(self.levels.iter().cloned()),
            ThinkingStyle::Budget => out.extend(self.budgets.iter().map(ToString::to_string)),
        }
        out
    }

    /// What a request asks for, when a chat chose `choice` (or nothing):
    /// a choice this model does not offer gives way to its default.
    #[must_use]
    pub fn resolve(&self, choice: Option<&str>) -> crate::llm_proxy::Reasoning {
        use crate::llm_proxy::Reasoning;
        let choices = self.choices();
        let pick = choice
            .filter(|c| choices.iter().any(|x| x == c))
            .or_else(|| {
                self.default
                    .as_deref()
                    .filter(|d| choices.iter().any(|x| x == d))
            })
            .or_else(|| choices.first().map(String::as_str))
            .unwrap_or("off");
        match (pick, self.style) {
            ("off", _) => Reasoning::Off,
            ("on", _) => Reasoning::On,
            (level, ThinkingStyle::Effort) => Reasoning::Effort(level.to_string()),
            (budget, ThinkingStyle::Budget) => {
                budget.parse().map_or(Reasoning::On, Reasoning::Budget)
            }
            (_, ThinkingStyle::Toggle) => Reasoning::On,
        }
    }

    /// `api` is the API the model's provider speaks.
    fn validate(&self, id: &str, api: Option<&str>) -> anyhow::Result<()> {
        match self.style {
            ThinkingStyle::Effort => {
                if self.levels.is_empty() || self.levels.len() > MAX_CHOICES {
                    anyhow::bail!(
                        "[[llm.models]] {id:?}: thinking by effort lists 1 to {MAX_CHOICES} levels"
                    );
                }
                for level in &self.levels {
                    let ok = !level.is_empty()
                        && level.len() <= MAX_LEVEL_LEN
                        && level
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                        && level != "off"
                        && level != "on";
                    if !ok {
                        anyhow::bail!(
                            "[[llm.models]] {id:?}: thinking level {level:?} is not a short lowercase word (nor off or on)"
                        );
                    }
                }
            }
            ThinkingStyle::Budget => {
                if api == Some("openai") {
                    anyhow::bail!(
                        "[[llm.models]] {id:?}: an OpenAI-compatible endpoint takes thinking by effort or on/off, not by budget"
                    );
                }
                if self.budgets.is_empty()
                    || self.budgets.len() > MAX_CHOICES
                    || self.budgets.iter().any(|b| *b < 128)
                {
                    anyhow::bail!(
                        "[[llm.models]] {id:?}: thinking by budget lists 1 to {MAX_CHOICES} budgets of at least 128 tokens"
                    );
                }
            }
            ThinkingStyle::Toggle => {}
        }
        if let Some(default) = &self.default
            && !self.choices().contains(default)
        {
            anyhow::bail!(
                "[[llm.models]] {id:?}: thinking default {default:?} is not one of {:?}",
                self.choices()
            );
        }
        Ok(())
    }

    /// As an inline table of config.toml.
    fn to_toml(&self) -> InlineTable {
        let mut t = InlineTable::new();
        t.insert(
            "style",
            match self.style {
                ThinkingStyle::Toggle => "toggle",
                ThinkingStyle::Effort => "effort",
                ThinkingStyle::Budget => "budget",
            }
            .into(),
        );
        if !self.levels.is_empty() {
            t.insert(
                "levels",
                self.levels
                    .iter()
                    .map(String::as_str)
                    .collect::<toml_edit::Array>()
                    .into(),
            );
        }
        if !self.budgets.is_empty() {
            t.insert(
                "budgets",
                self.budgets
                    .iter()
                    .map(|b| i64::from(*b))
                    .collect::<toml_edit::Array>()
                    .into(),
            );
        }
        if let Some(default) = &self.default {
            t.insert("default", default.as_str().into());
        }
        if self.mandatory {
            t.insert("mandatory", true.into());
        }
        t
    }
}

/// What model families take in a request, by the start of the model's
/// name (after any `vendor/`), the longest that matches; [`DEFAULT_CONTEXT`]
/// for the rest.
const KNOWN_CONTEXTS: &[(&str, u32)] = &[
    ("claude", 200_000),
    ("gemini", 1_000_000),
    ("gpt-4-1", 1_000_000),
    ("gpt-4-1106", 128_000),
    ("gpt-4-0125", 128_000),
    ("gpt-4o", 128_000),
    ("gpt-4-turbo", 128_000),
    ("gpt-5", 400_000),
    ("gpt-oss", 128_000),
    ("o1", 200_000),
    ("o3", 200_000),
    ("o4", 200_000),
    ("deepseek", 128_000),
    ("kimi", 256_000),
    ("grok", 256_000),
    ("mistral", 128_000),
    ("codestral", 256_000),
    ("devstral", 128_000),
    ("magistral", 128_000),
    ("ministral", 128_000),
    ("pixtral", 128_000),
    ("minimax", 200_000),
    ("qwen", 128_000),
    ("glm", 128_000),
    ("llama", 128_000),
];
/// What a model of no known family is taken to take: little, so a local
/// model with a small context is not overfilled.
pub const DEFAULT_CONTEXT: u32 = 32_000;

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn a_model_takes_its_set_window_else_its_family_s() {
        let entry = |id: &str, window: Option<u32>| ModelEntry {
            id: id.into(),
            label: None,
            max_output_tokens: None,
            vision: None,
            thinking: None,
            context_window: window,
            fallback: Vec::new(),
            price: None,
        };
        assert_eq!(
            entry("anthropic:claude-sonnet-4-6", None).context(),
            200_000
        );
        assert_eq!(
            entry("openrouter:google/gemini-3.8-flash", None).context(),
            1_000_000
        );
        assert_eq!(
            entry("openrouter:deepseek/deepseek-v4.1-flash-1017", None).context(),
            128_000
        );
        assert_eq!(
            entry("ollama:some-local-model", None).context(),
            DEFAULT_CONTEXT
        );
        assert_eq!(entry("openai:gpt-4.1-mini", None).context(), 1_000_000);
        assert_eq!(
            entry("openrouter:qwen/qwen3-coder", None).context(),
            128_000
        );
        assert_eq!(entry("openai:o3-mini", None).context(), 200_000);
        // A name that only holds a family's name elsewhere is not of it.
        assert_eq!(entry("openai:olmo3", None).context(), DEFAULT_CONTEXT);
        assert_eq!(
            entry("anthropic:claude-sonnet-4-6", Some(64_000)).context(),
            64_000
        );
    }
}

impl ModelEntry {
    /// Tokens it takes in one request: `context_window`, else what its
    /// family takes, else [`DEFAULT_CONTEXT`].
    #[must_use]
    pub fn context(&self) -> u32 {
        self.context_window.unwrap_or_else(|| {
            let model = self.model();
            let bare = model.rsplit('/').next().unwrap_or(model);
            let name = bare.to_ascii_lowercase().replace('.', "-");
            KNOWN_CONTEXTS
                .iter()
                .filter(|(prefix, _)| {
                    name.strip_prefix(prefix).is_some_and(|rest| {
                        rest.is_empty()
                            || rest.starts_with(['-', ':', '@'])
                            || rest.starts_with(|c: char| c.is_ascii_digit())
                    })
                })
                .max_by_key(|(prefix, _)| prefix.len())
                .map_or(DEFAULT_CONTEXT, |(_, tokens)| *tokens)
        })
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        split_model_ref(&self.id).map_or("", |(provider, _)| provider)
    }

    #[must_use]
    pub fn model(&self) -> &str {
        split_model_ref(&self.id).map_or("", |(_, model)| model)
    }
}

/// `provider:model`, split at the first colon, with a provider that is a
/// built-in one or has the shape of an `[llm.endpoints]` name: whether
/// that endpoint is configured is [`ModelCatalog::validate`]'s to check.
#[must_use]
pub fn split_model_ref(id: &str) -> Option<(&str, &str)> {
    if id.len() > MAX_ID_LEN || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (provider, model) = id.split_once(':')?;
    let named =
        crate::llm_proxy::is_provider(provider) || crate::config::is_endpoint_name(provider);
    (!model.is_empty() && named).then_some((provider, model))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub default_model: Option<String>,
    /// The model that writes chats' titles (issue #179), unless a team's
    /// policy names another; `None` for each agent's own model.
    #[serde(default)]
    pub title_model: Option<String>,
    pub models: Vec<ModelEntry>,
}

impl ModelCatalog {
    /// `endpoints` are the `[llm.endpoints]` its models may name.
    pub fn validate(&self, endpoints: &crate::config::Endpoints) -> anyhow::Result<()> {
        let mut seen = HashSet::new();
        for entry in &self.models {
            let split = split_model_ref(&entry.id);
            let api = split.and_then(|(p, _)| endpoints.api_of(p));
            if let Some((provider, _)) = split
                && api.is_none()
            {
                anyhow::bail!(
                    "[[llm.models]] {:?} names the endpoint {provider:?}, which is not one of \
                     [llm.endpoints] (the gateway takes an endpoint added to config.toml, and \
                     drops one removed, on `havn gateway restart`)",
                    entry.id
                );
            }
            if api.is_none() {
                let providers: Vec<&str> = crate::llm_proxy::PROVIDERS
                    .iter()
                    .copied()
                    .chain(endpoints.0.keys().map(String::as_str))
                    .collect();
                anyhow::bail!(
                    "[[llm.models]] id {:?} must be provider:model, without spaces, with a \
                     provider of {} (an endpoint of [llm.endpoints] is a provider too)",
                    entry.id,
                    providers.join(", ")
                );
            }
            if !seen.insert(entry.id.as_str()) {
                anyhow::bail!("[[llm.models]] lists {:?} more than once", entry.id);
            }
            // A fallback that is not listed (removed since) is passed over
            // when one is picked; one naming its own model is a mistake.
            if entry.fallback.contains(&entry.id) {
                anyhow::bail!("[[llm.models]] {:?} names itself as its fallback", entry.id);
            }
            if entry.max_output_tokens == Some(0) {
                anyhow::bail!(
                    "[[llm.models]] {:?}: max_output_tokens must be at least 1",
                    entry.id
                );
            }
            if let Some(price) = &entry.price {
                price.validate(&entry.id)?;
            }
            if let Some(thinking) = &entry.thinking {
                thinking.validate(&entry.id, api)?;
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
        if let Some(title) = &self.title_model
            && !seen.contains(title.as_str())
        {
            anyhow::bail!("[llm] title_model {title:?} is not one of the [[llm.models]] ids");
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
    title_model: Option<String>,
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
        title_model: file.llm.title_model,
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
// none. A missing file is created. Nothing is written when the gateway
// could not load the file with the list ([`Unloadable`]): its models must
// name the file's own `[llm.endpoints]`.
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

/// Why a model list was not written: config.toml with it is not one the
/// gateway could load.
#[derive(Debug, thiserror::Error)]
#[error("config.toml would not load with this model list: {0}")]
pub struct Unloadable(pub String);

fn write_checked(
    path: &Path,
    catalog: &ModelCatalog,
    expected: Option<&str>,
) -> anyhow::Result<bool> {
    use anyhow::Context as _;

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
    match &catalog.title_model {
        Some(id) => llm["title_model"] = value(id.as_str()),
        None => {
            llm.remove("title_model");
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
            match entry.vision {
                Some(sees) => table["vision"] = value(sees),
                None => {
                    table.remove("vision");
                }
            }
            match &entry.thinking {
                Some(thinking) => table["thinking"] = value(thinking.to_toml()),
                None => {
                    table.remove("thinking");
                }
            }
            match entry.context_window {
                Some(n) => table["context_window"] = value(i64::from(n)),
                None => {
                    table.remove("context_window");
                }
            }
            if entry.fallback.is_empty() {
                table.remove("fallback");
            } else {
                let mut list = toml_edit::Array::new();
                for id in &entry.fallback {
                    list.push(id.as_str());
                }
                table["fallback"] = value(list);
            }
            match &entry.price {
                Some(price) => {
                    let mut inline = toml_edit::InlineTable::new();
                    inline.insert("input", price.input.into());
                    inline.insert("output", price.output.into());
                    if let Some(p) = price.cache_read {
                        inline.insert("cache_read", p.into());
                    }
                    if let Some(p) = price.cache_write {
                        inline.insert("cache_write", p.into());
                    }
                    table["price"] = value(inline);
                }
                None => {
                    table.remove("price");
                }
            }
            models.push(table);
        }
        llm.insert("models", Item::ArrayOfTables(models));
    }

    // Checked as the gateway loads it at its next start.
    let written = doc.to_string();
    if let Err(e) = crate::config::GatewayConfig::from_toml(&written) {
        return Err(Unloadable(format!("{e:#}")).into());
    }
    replace_file(&target, written.as_bytes())?;
    Ok(true)
}

// Write a sibling file, flush it, give it the original's owner and mode,
// and rename it over `target`.
pub(crate) fn replace_file(target: &Path, contents: &[u8]) -> anyhow::Result<()> {
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
            let builtin =
                split_model_ref(&model).is_some_and(|(p, _)| crate::llm_proxy::is_provider(p));
            if !seeded.contains(&model) && builtin {
                seeded.push(model);
            }
        }
        if !seeded.is_empty() {
            let catalog = ModelCatalog {
                default_model: None,
                title_model: None,
                models: seeded
                    .into_iter()
                    .map(|id| ModelEntry {
                        id,
                        label: None,
                        max_output_tokens: None,
                        vision: None,
                        thinking: None,
                        context_window: None,
                        fallback: Vec::new(),
                        price: None,
                    })
                    .collect(),
            };
            let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
            // Of the built-in providers only, as the agents named them.
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
    use crate::config::{Endpoint, EndpointApi, Endpoints};

    const NO_ENDPOINTS: Endpoints = Endpoints(std::collections::BTreeMap::new());

    #[test]
    fn a_models_thinking_resolves_within_its_choices() {
        use crate::llm_proxy::Reasoning;
        let effort = ThinkingSpec {
            style: ThinkingStyle::Effort,
            levels: vec!["low".into(), "high".into()],
            budgets: Vec::new(),
            default: Some("low".into()),
            mandatory: false,
        };
        assert_eq!(effort.choices(), ["off", "low", "high"]);
        assert_eq!(effort.resolve(None), Reasoning::Effort("low".into()));
        assert_eq!(
            effort.resolve(Some("high")),
            Reasoning::Effort("high".into())
        );
        assert_eq!(effort.resolve(Some("off")), Reasoning::Off);
        assert_eq!(
            effort.resolve(Some("4096")),
            Reasoning::Effort("low".into()),
            "not its choice"
        );
        let mandatory = ThinkingSpec {
            style: ThinkingStyle::Budget,
            levels: Vec::new(),
            budgets: vec![1024, 8192],
            default: None,
            mandatory: true,
        };
        assert_eq!(mandatory.choices(), ["1024", "8192"]);
        assert_eq!(
            mandatory.resolve(Some("off")),
            Reasoning::Budget(1024),
            "it must think"
        );
        assert_eq!(mandatory.resolve(Some("8192")), Reasoning::Budget(8192));
        let toggle = ThinkingSpec {
            style: ThinkingStyle::Toggle,
            levels: Vec::new(),
            budgets: Vec::new(),
            default: None,
            mandatory: false,
        };
        assert_eq!(
            toggle.resolve(None),
            Reasoning::Off,
            "off unless the default says on"
        );
        assert_eq!(toggle.resolve(Some("on")), Reasoning::On);
    }

    #[test]
    fn each_vision_setting_is_written_and_read_back() {
        let entry = |id: &str, vision: Option<bool>| ModelEntry {
            id: id.into(),
            label: None,
            max_output_tokens: None,
            vision,
            thinking: None,
            context_window: None,
            fallback: Vec::new(),
            price: None,
        };
        let mut c = ModelCatalog::default();
        c.models = vec![
            entry("openrouter:a/seeing", Some(true)),
            entry("openrouter:a/blind", Some(false)),
            entry("openrouter:a/tried", None),
        ];
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("write");
        write_to_file(&path, &c).expect("written");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("vision = true"), "{text}");
        assert!(text.contains("vision = false"), "{text}");
        assert_eq!(text.matches("vision").count(), 2, "{text}");
        assert_eq!(read_from_file(&path).expect("read back"), c);
        // Unset again, the line goes.
        c.models[1].vision = None;
        write_to_file(&path, &c).expect("written");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.matches("vision").count(), 1, "{text}");
        // A context window set is written and read back (issue #214).
        c.models[2].context_window = Some(20_000);
        c.models[2].fallback = vec!["openrouter:a/seeing".into()];
        write_to_file(&path, &c).expect("written");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("context_window = 20000"), "{text}");
        assert!(
            text.contains("fallback = [\"openrouter:a/seeing\"]"),
            "{text}"
        );
        assert_eq!(read_from_file(&path).expect("read back"), c);
    }

    #[test]
    fn a_fallback_must_be_another_model() {
        let mut c = ModelCatalog::default();
        c.models = vec![entry("openrouter:a/main"), entry("openrouter:a/spare")];
        c.models[0].fallback = vec!["openrouter:a/spare".into()];
        assert!(c.validate(&NO_ENDPOINTS).is_ok());
        // One removed since is passed over, not refused: the model it was
        // can be removed.
        c.models[0].fallback = vec!["openrouter:a/gone".into()];
        assert!(c.validate(&NO_ENDPOINTS).is_ok());
        c.models[0].fallback = vec!["openrouter:a/main".into()];
        assert!(c.validate(&NO_ENDPOINTS).is_err());
    }

    #[test]
    fn thinking_is_checked_and_written_to_config_toml() {
        let mut c = ModelCatalog::default();
        c.models.push(ModelEntry {
            id: "openrouter:deepseek/deepseek-v4.1-flash".into(),
            label: None,
            max_output_tokens: None,
            vision: None,
            thinking: Some(ThinkingSpec {
                style: ThinkingStyle::Effort,
                levels: vec!["low".into(), "medium".into(), "high".into()],
                budgets: Vec::new(),
                default: Some("medium".into()),
                mandatory: false,
            }),
            context_window: None,
            fallback: Vec::new(),
            price: None,
        });
        c.validate(&NO_ENDPOINTS).expect("valid");
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("write");
        write_to_file(&path, &c).expect("written");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("thinking = { style = \"effort\""), "{text}");
        assert_eq!(read_from_file(&path).expect("read back"), c);

        let mut bad = c.clone();
        bad.models[0].thinking.as_mut().expect("thinking").default = Some("max".into());
        assert!(
            bad.validate(&NO_ENDPOINTS).is_err(),
            "a default it does not offer"
        );
        let mut bad = c.clone();
        bad.models[0].id = "openai:gpt-5".into();
        let spec = bad.models[0].thinking.as_mut().expect("thinking");
        spec.style = ThinkingStyle::Budget;
        spec.budgets = vec![1024];
        spec.default = None;
        assert!(
            bad.validate(&NO_ENDPOINTS).is_err(),
            "an OpenAI endpoint takes no budget"
        );
    }

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            label: None,
            max_output_tokens: None,
            vision: None,
            thinking: None,
            context_window: None,
            fallback: Vec::new(),
            price: None,
        }
    }

    fn catalog(ids: &[&str], default: Option<&str>) -> ModelCatalog {
        ModelCatalog {
            default_model: default.map(str::to_string),
            title_model: None,
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
        assert_eq!(split_model_ref("Nobody:model"), None);
        assert_eq!(split_model_ref("openai:"), None);
        // An endpoint's name splits off; whether it is configured is
        // checked by the catalog.
        assert_eq!(
            split_model_ref("ollama:qwen3:8b"),
            Some(("ollama", "qwen3:8b"))
        );
    }

    #[test]
    fn a_model_may_name_an_endpoint_only_when_it_is_configured() {
        let mut endpoints = Endpoints::default();
        endpoints.0.insert(
            "ollama".into(),
            Endpoint {
                api: EndpointApi::Openai,
                base_url: "http://127.0.0.1:11434/v1".into(),
            },
        );
        endpoints.0.insert(
            "lan".into(),
            Endpoint {
                api: EndpointApi::Anthropic,
                base_url: "http://10.0.0.5:8000".into(),
            },
        );
        let c = catalog(&["ollama:qwen3", "anthropic:claude-opus-4-7"], None);
        c.validate(&endpoints).expect("a configured endpoint");
        let msg = format!(
            "{}",
            c.validate(&NO_ENDPOINTS).expect_err("no such endpoint")
        );
        assert!(msg.contains("ollama:qwen3"), "{msg}");
        assert!(
            catalog(&["nobody:model"], None)
                .validate(&endpoints)
                .is_err()
        );
        // The endpoint's API decides how it thinks: by budget on an
        // Anthropic-compatible one, not on an OpenAI-compatible one.
        let budget = |id: &str| {
            let mut c = catalog(&[id], None);
            c.models[0].thinking = Some(ThinkingSpec {
                style: ThinkingStyle::Budget,
                levels: Vec::new(),
                budgets: vec![1024],
                default: None,
                mandatory: false,
            });
            c.validate(&endpoints)
        };
        assert!(budget("lan:claude-opus-4-7").is_ok());
        assert!(budget("ollama:qwen3").is_err());
    }

    #[test]
    fn validation_rejects_bad_ids_duplicates_and_an_unlisted_default() {
        assert!(
            catalog(&["anthropic:claude-opus-4-7"], None)
                .validate(&NO_ENDPOINTS)
                .is_ok()
        );
        assert!(
            catalog(&["claude-opus-4-7"], None)
                .validate(&NO_ENDPOINTS)
                .is_err()
        );
        assert!(
            catalog(&["openai:gpt-5", "openai:gpt-5"], None)
                .validate(&NO_ENDPOINTS)
                .is_err()
        );
        assert!(
            catalog(&["openai:gpt-5"], Some("openai:gpt-4o"))
                .validate(&NO_ENDPOINTS)
                .is_err()
        );
        let mut zero = catalog(&["openai:gpt-5"], None);
        zero.models[0].max_output_tokens = Some(0);
        assert!(zero.validate(&NO_ENDPOINTS).is_err());
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
        assert!(c.validate(&NO_ENDPOINTS).is_err());
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

    #[test]
    fn a_list_is_written_only_with_the_endpoints_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let c = catalog(&["ollama:qwen3"], None);
        let err = write_to_file(&path, &c).expect_err("no such endpoint in the file");
        assert!(err.downcast_ref::<Unloadable>().is_some(), "{err:#}");
        assert!(
            format!("{err:#}").contains("endpoint \"ollama\""),
            "{err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "nothing written"
        );
        std::fs::write(
            &path,
            "[llm.endpoints.ollama]\napi = \"openai\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n",
        )
        .unwrap();
        write_to_file(&path, &c).expect("written");
        assert_eq!(read_from_file(&path).unwrap(), c);
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

    #[test]
    fn a_price_gives_what_tokens_cost_with_cache_reads_and_writes_apart() {
        let price = ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: None,
        };
        // 1M input (200k read from cache, 100k written), 100k output.
        let cost = price.cost(1_000_000, 200_000, 100_000, 100_000);
        let wanted = 0.7 * 3.0 + 0.2 * 0.3 + 0.1 * 3.0 + 0.1 * 15.0;
        assert!((cost - wanted).abs() < 1e-9, "{cost} {wanted}");
        assert!((price.cost(0, 0, 0, 0)).abs() < f64::EPSILON);

        let entry: ModelEntry = toml::from_str(
            "id = \"anthropic:claude\"\nprice = { input = 3.0, output = 15.0, cache_read = 0.3 }",
        )
        .expect("entry");
        assert_eq!(entry.price, Some(price));
        let negative: ModelEntry =
            toml::from_str("id = \"a:b\"\nprice = { input = -1.0, output = 1.0 }").expect("entry");
        let catalog = ModelCatalog {
            default_model: None,
            title_model: None,
            models: vec![negative],
        };
        assert!(catalog.validate(&NO_ENDPOINTS).is_err());
        assert!(
            toml::from_str::<ModelEntry>("id = \"a:b\"\nprice = { input = 1.0 }").is_err(),
            "output is needed"
        );
    }

    #[test]
    fn a_price_is_written_to_config_toml_changed_and_cleared() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[llm]\n[[llm.models]]\nid = \"anthropic:a\"\n").expect("write");
        let read = || -> ModelCatalog {
            let cfg: crate::config::GatewayConfig =
                toml::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
            cfg.llm.catalog()
        };
        let mut catalog = read();
        let priced = ModelPrice {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: None,
        };
        for price in [
            Some(priced),
            Some(ModelPrice {
                input: 1.5,
                ..priced
            }),
            None,
        ] {
            let expected = catalog.version();
            catalog.models[0].price = price;
            assert!(write_if_unchanged(&path, &expected, &catalog).expect("write"));
            assert_eq!(read().models[0].price, price);
            assert_eq!(read().version(), catalog.version(), "the file is what runs");
        }
        let negative = ModelPrice {
            input: -0.0,
            ..priced
        };
        catalog.models[0].price = Some(negative);
        assert!(
            catalog.validate(&NO_ENDPOINTS).is_err(),
            "-0 is refused too"
        );
    }
}
