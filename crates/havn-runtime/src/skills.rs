//! Skill loader, indexer, and retriever (spec §9.3).
//!
//! ## File format
//!
//! Each skill is a directory with at least `SKILL.md` — YAML frontmatter
//! followed by a markdown body. Required fields are `name` and
//! `description`; everything else is optional. Vendor-specific metadata
//! goes under `metadata.<vendor>` so files round-trip cleanly between
//! havn, `OpenClaw`, and Hermes.
//!
//! ```text
//! ---
//! name: code-review
//! description: Reviews diffs against a quality rubric.
//! version: 1.0.0
//! user-invocable: true
//! triggers: [review, PR, diff]
//! metadata:
//!   havn: { tags: [engineering] }
//!   openclaw: { requires: { bins: [git] } }
//!   hermes: { tags: [code, qa] }
//! ---
//!
//! # Body — markdown, ≤ 100 KB
//! ```
//!
//! ## Loading
//!
//! One source at runtime startup: `<workspace>/skills/<name>/SKILL.md`,
//! user-uploaded or agent-created via [`crate::tools::skill_manage`].
//!
//! Earlier drafts shipped a bundled set of demo skills compiled into
//! the runtime via `include_dir!`. Removed in v0.6 (spec §9.3): they
//! were placeholder content nobody used in production, and "what skills
//! should every agent have" turns out to be a deeply project-specific
//! question. Operators write their own SKILL.md or let the agent
//! create them via `skill_manage`.
//!
//! Loaded skills are written to the per-agent `skills_index` table
//! (spec §5.2 layer 3) which has FTS5 mirror `skills_fts` for retrieval.
//!
//! ## Retrieval
//!
//! At each user turn, [`relevant_for`] runs an FTS5 query over
//! name/description/body and returns the top-K matches. The runtime
//! prepends their bodies to the system prompt for *that* LLM call only —
//! the frozen-prompt invariant (spec §9.4) is unaffected.

use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Hermes-derived per-skill body cap (spec §9.3, §1.5 compatibility row).
pub const MAX_BODY_BYTES: usize = 100 * 1024;

/// Top-K skills retrieved per user turn. Tuned tight so context overhead
/// stays bounded.
pub const RELEVANT_LIMIT: u32 = 3;

/// Skill source. v0.6 has only `Workspace` — bundled skills were cut.
/// Kept as an enum (not `()`) so a future re-introduction of bundled or
/// team-shared sources doesn't ripple through every call site. The
/// `skills_index.source` column's CHECK constraint still allows
/// `'bundled'` for backward compatibility with rows from older installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Workspace,
}

/// Parsed skill (one `SKILL.md`).
///
/// Several fields (`version`, `user_invocable`, `triggers`, `source`) round-trip
/// through the parser but are not yet plumbed into runtime behaviour — they
/// will drive the dashboard's skill manager and the Phase 2 `skill_manage`
/// tool. Allowing `dead_code` on the struct rather than dropping the fields
/// keeps the on-disk format stable.
#[allow(
    dead_code,
    reason = "fields surface to dashboard / skill_manage in the next vertical"
)]
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub user_invocable: bool,
    pub triggers: Vec<String>,
    pub source: SkillSource,
    pub body: String,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SkillError {
    #[error("missing frontmatter: SKILL.md must start with `---`")]
    MissingFrontmatter,
    #[error("unterminated frontmatter: missing closing `---`")]
    UnterminatedFrontmatter,
    #[error("invalid frontmatter YAML: {0}")]
    InvalidYaml(String),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("body exceeds {MAX_BODY_BYTES} bytes (got {0})")]
    BodyTooLarge(usize),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("db: {0}")]
    Db(#[from] havn_db::DbError),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Parse a SKILL.md byte slice into a `LoadedSkill`.
///
/// The body is split off the first `---` block. We accept either CRLF or LF
/// line endings; parser is permissive about indentation in metadata blocks
/// (the YAML library handles JSON-in-YAML naturally).
pub fn parse_skill(content: &str, source: SkillSource) -> Result<LoadedSkill, SkillError> {
    let trimmed = content.trim_start_matches('\u{feff}');
    let after_open = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
        .ok_or(SkillError::MissingFrontmatter)?;

    let close_idx = find_close_marker(after_open).ok_or(SkillError::UnterminatedFrontmatter)?;
    let frontmatter = &after_open[..close_idx];
    // Skip the closing marker line (and any blank line right after).
    let after_close = after_open[close_idx..]
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    let body = after_close.trim_start().to_string();

    if body.len() > MAX_BODY_BYTES {
        return Err(SkillError::BodyTooLarge(body.len()));
    }

    let parsed: FrontMatter =
        serde_norway::from_str(frontmatter).map_err(|e| SkillError::InvalidYaml(e.to_string()))?;

    let name = parsed.name.ok_or(SkillError::MissingField("name"))?;
    let description = parsed
        .description
        .ok_or(SkillError::MissingField("description"))?;
    if name.trim().is_empty() {
        return Err(SkillError::MissingField("name"));
    }
    if description.trim().is_empty() {
        return Err(SkillError::MissingField("description"));
    }

    Ok(LoadedSkill {
        name,
        description,
        version: parsed.version,
        user_invocable: parsed.user_invocable.unwrap_or(false),
        triggers: parsed.triggers.unwrap_or_default(),
        source,
        body,
    })
}

fn find_close_marker(after_open: &str) -> Option<usize> {
    let mut idx = 0;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Some(idx);
        }
        idx += line.len();
    }
    None
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    #[serde(rename = "user-invocable", default)]
    user_invocable: Option<bool>,
    #[serde(default)]
    triggers: Option<Vec<String>>,
}

/// Load every `<name>/SKILL.md` from `<workspace>/skills/`.
/// Returns an empty vec when the directory doesn't exist (no skills installed yet).
pub async fn load_workspace(workspace: &Path) -> Result<Vec<LoadedSkill>, SkillError> {
    let dir = workspace.join("skills");
    if !tokio::fs::try_exists(&dir).await? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = entry.metadata().await?;
        if !metadata.is_dir() {
            continue;
        }
        let skill_path = path.join("SKILL.md");
        if !tokio::fs::try_exists(&skill_path).await? {
            continue;
        }
        let content = tokio::fs::read_to_string(&skill_path).await?;
        match parse_skill(&content, SkillSource::Workspace) {
            Ok(skill) => out.push(skill),
            Err(e) => {
                warn!(path = %skill_path.display(), error = %e, "workspace skill failed to parse");
            }
        }
    }
    debug!(count = out.len(), "workspace skills loaded");
    Ok(out)
}

/// Index loaded skills into the agent's `skills_index` (and FTS mirror via triggers).
/// Idempotent — upserts by `name`, so re-running on each startup keeps
/// the index fresh **without** wiping `last_used_at` / `use_count` /
/// `pinned` (those columns survive restarts; that's what makes the
/// curator's "unused for 90 days" check meaningful across sessions).
///
/// `archived_at` of an existing row is intentionally not cleared on
/// re-index — once the curator archives a skill it stays archived
/// across restarts unless explicitly un-archived (or the SKILL.md is
/// edited / recreated, which triggers `skill_manage::reindex_one`'s
/// own upsert path).
pub async fn index_into(skills: &[LoadedSkill], pool: &SqlitePool) -> Result<(), SkillError> {
    // v0.6: workspace is the only source; bundled was cut. The
    // `source` column stays in the schema (with a CHECK that still
    // accepts 'bundled' for backward compat with existing rows from
    // older installs) so the curator's "what's eligible to consolidate"
    // filter keeps the same shape.
    for skill in skills {
        let source = match skill.source {
            SkillSource::Workspace => "workspace",
        };
        sqlx::query(
            "INSERT INTO skills_index (id, name, description, body, source) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(name) DO UPDATE SET \
               description = excluded.description, \
               body        = excluded.body, \
               source      = excluded.source",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&skill.name)
        .bind(&skill.description)
        .bind(&skill.body)
        .bind(source)
        .execute(pool)
        .await?;
    }
    info!(count = skills.len(), "skills indexed");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RetrievedSkill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// How many of the top-K returned rows get their `use_count` /
/// `last_used_at` bumped by [`relevant_for`]. Mirrors memory's
/// recall-tracking design — only the strongest hits count as "this
/// skill was actually load-bearing" so the curator's aging check
/// doesn't get confused by long-tail FTS noise.
const USE_BUMP_TOP_K: usize = 3;

/// Top-K skills relevant to `user_text`. Returns empty vec when no match
/// (or when the index is empty). Active rows only — archived skills are
/// invisible to retrieval so the agent can't unintentionally lean on
/// stale workflows.
///
/// As a side effect, the top [`USE_BUMP_TOP_K`] rows have `use_count`
/// incremented and `last_used_at` set to now. The curator (§9.5) reads
/// these columns to decide what's safe to archive.
///
/// **Hybrid retrieval (spec §13 Phase 3):** when `embedder` is `Some`
/// the routing goes through `embedding::hybrid::search` — vector +
/// BM25 weighted, MMR-diversified — which lets queries with no shared
/// keywords still surface the right skill (e.g. "evaluate my changes"
/// hitting a `code-review` skill). When `embedder` is `None`,
/// degrades to FTS5-only, byte-for-byte the v0.6 behaviour.
pub async fn relevant_for(
    pool: &SqlitePool,
    embedder: &crate::embedding::EmbedderHandle,
    user_text: &str,
    limit: u32,
) -> Result<Vec<RetrievedSkill>, SkillError> {
    if user_text.trim().is_empty() {
        return Ok(Vec::new());
    }

    let source = SkillsSource { pool };
    let params = crate::embedding::hybrid::HybridParams {
        recall_bump_top_k: USE_BUMP_TOP_K,
        ..crate::embedding::hybrid::HybridParams::default()
    };
    let curatable =
        match crate::embedding::hybrid::search(&source, embedder, user_text, &(), limit, params)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "skills hybrid search failed");
                return Ok(Vec::new());
            }
        };
    Ok(curatable
        .into_iter()
        .map(|s| RetrievedSkill {
            name: s.name,
            description: s.description,
            body: s.body,
        })
        .collect())
}

/// `HybridSource` impl for the per-agent `skills_index` table
/// (spec §13 Phase 3). Filter is `()` (skills don't have a
/// kind-style discriminator in v1); Row is `CuratableSkill` —
/// the same shape `list_curatable` returns, so callers can bridge
/// to a slimmer `RetrievedSkill` if they prefer.
struct SkillsSource<'a> {
    pool: &'a SqlitePool,
}

#[async_trait::async_trait]
impl crate::embedding::hybrid::HybridSource for SkillsSource<'_> {
    type Filter = ();
    type Row = havn_db::agent::skills_index::CuratableSkill;

    async fn fts_hits(
        &self,
        query: &str,
        _filter: &(),
        limit: u32,
    ) -> Result<Vec<havn_db::agent::hybrid_common::FtsHit>, havn_db::DbError> {
        havn_db::agent::skills_index::fts_hits(self.pool, query, limit).await
    }

    async fn embedded_candidates(
        &self,
        expected_dim: usize,
        _filter: &(),
    ) -> Result<Vec<havn_db::agent::hybrid_common::EmbeddedCandidate>, havn_db::DbError> {
        havn_db::agent::skills_index::embedded_candidates(self.pool, expected_dim).await
    }

    async fn fetch_by_id(&self, id: &str) -> Result<Option<Self::Row>, havn_db::DbError> {
        havn_db::agent::skills_index::fetch_by_id(self.pool, id).await
    }

    async fn bump_top_k(&self, ids: &[String]) {
        if let Err(e) = havn_db::agent::skills_index::bump_use_for(self.pool, ids).await {
            warn!(error = %e, "skills hybrid: bump_use_for failed");
        }
    }
}

/// Render top-K skills as a markdown section ready to append to a system
/// prompt. Returns `None` when no skills matched (caller should send the
/// system prompt unchanged).
pub fn render_for_prompt(skills: &[RetrievedSkill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from("## Skills available right now\n\n");
    for skill in skills {
        writeln!(
            out,
            "### {}\n\n_{}_\n\n{}\n",
            skill.name, skill.description, skill.body
        )
        .ok();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_db::agent::connect_in_memory;

    fn skill(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}")
    }

    #[test]
    fn parse_minimum_required_fields() {
        let s = skill("hello", "say hi", "hi.");
        let parsed = parse_skill(&s, SkillSource::Workspace).expect("parse");
        assert_eq!(parsed.name, "hello");
        assert_eq!(parsed.description, "say hi");
        assert_eq!(parsed.body, "hi.");
        assert!(!parsed.user_invocable);
        assert!(parsed.triggers.is_empty());
        assert_eq!(parsed.source, SkillSource::Workspace);
    }

    #[test]
    fn parse_full_frontmatter_round_trip() {
        let raw = "---
name: code-review
description: review code
version: 1.2.3
user-invocable: true
triggers:
  - review
  - PR
---

body content
";
        let parsed = parse_skill(raw, SkillSource::Workspace).expect("parse");
        assert_eq!(parsed.name, "code-review");
        assert_eq!(parsed.version.as_deref(), Some("1.2.3"));
        assert!(parsed.user_invocable);
        assert_eq!(parsed.triggers, vec!["review", "PR"]);
    }

    #[test]
    fn missing_frontmatter_rejected() {
        let err = parse_skill("no frontmatter here", SkillSource::Workspace).expect_err("bad");
        assert!(matches!(err, SkillError::MissingFrontmatter));
    }

    #[test]
    fn missing_close_marker_rejected() {
        let err = parse_skill(
            "---\nname: x\ndescription: y\nbody no close",
            SkillSource::Workspace,
        )
        .expect_err("bad");
        assert!(matches!(err, SkillError::UnterminatedFrontmatter));
    }

    #[test]
    fn missing_required_fields_rejected() {
        let err = parse_skill("---\nname: x\n---\nbody", SkillSource::Workspace).expect_err("bad");
        assert!(matches!(err, SkillError::MissingField("description")));
    }

    #[test]
    fn empty_required_fields_rejected() {
        let err = parse_skill(
            "---\nname: \"\"\ndescription: y\n---\n",
            SkillSource::Workspace,
        )
        .expect_err("bad");
        assert!(matches!(err, SkillError::MissingField("name")));
    }

    #[test]
    fn body_oversize_rejected() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let raw = skill("x", "y", &big);
        let err = parse_skill(&raw, SkillSource::Workspace).expect_err("bad");
        assert!(matches!(err, SkillError::BodyTooLarge(_)));
    }

    #[test]
    fn vendor_metadata_round_trips_in_frontmatter() {
        let raw = "---
name: x
description: y
metadata:
  havn:
    tags: [tag1]
  openclaw:
    requires:
      bins: [git]
---
body
";
        // We don't currently surface metadata into LoadedSkill — but parse must
        // not fail on it (vendor-namespace fields are optional).
        let parsed = parse_skill(raw, SkillSource::Workspace).expect("parse");
        assert_eq!(parsed.name, "x");
    }

    #[tokio::test]
    async fn index_then_relevant_for_round_trip() {
        let pool = connect_in_memory().await.expect("db");
        let skills = vec![
            LoadedSkill {
                name: "code-review".into(),
                description: "review pull requests against a checklist".into(),
                version: None,
                user_invocable: false,
                triggers: vec![],
                source: SkillSource::Workspace,
                body: "Look at the diff. Check for tests.".into(),
            },
            LoadedSkill {
                name: "weather-fetcher".into(),
                description: "look up the current weather for a city".into(),
                version: None,
                user_invocable: false,
                triggers: vec![],
                source: SkillSource::Workspace,
                body: "Use web_fetch on the weather API.".into(),
            },
        ];
        index_into(&skills, &pool).await.expect("index");

        // OR semantics: queries with chatty filler ("the") match both rows on
        // the common word, but rank must surface the keyword-match first.
        // FTS5-only path (embedder = None) — preserves the v0.6 behaviour
        // for operators who haven't opted into hybrid retrieval.
        let no_embedder: crate::embedding::EmbedderHandle = None;
        let hits = relevant_for(&pool, &no_embedder, "review the PR", 3)
            .await
            .expect("search");
        assert!(!hits.is_empty(), "expected at least one match");
        assert_eq!(
            hits[0].name, "code-review",
            "rank must put code-review first"
        );

        // A query without stopwords narrows to the single best match.
        let only_weather = relevant_for(&pool, &no_embedder, "weather city", 3)
            .await
            .expect("search");
        assert_eq!(only_weather.len(), 1);
        assert_eq!(only_weather[0].name, "weather-fetcher");
    }

    #[tokio::test]
    async fn relevant_for_handles_empty_query() {
        let pool = connect_in_memory().await.expect("db");
        let no_embedder: crate::embedding::EmbedderHandle = None;
        let hits = relevant_for(&pool, &no_embedder, "", 3)
            .await
            .expect("search");
        assert!(hits.is_empty());
    }

    /// End-to-end with the deterministic HRR embedder + the skills
    /// hybrid path. Mirrors the memory e2e in
    /// `embedding::hybrid::tests::end_to_end_remember_backfill_search_recall`:
    /// load skills, run skills backfill, hybrid-search a token-overlapping
    /// query, assert the matching skill ranks first AND its `use_count` /
    /// `last_used_at` bump.
    #[tokio::test]
    async fn skills_hybrid_search_ranks_overlapping_skill_first_and_bumps_use_count() {
        use crate::embedding::EmbeddingProvider;
        use crate::embedding::backfill;
        use crate::embedding::hrr::{HrrConfig, HrrEmbedder};
        use std::sync::Arc;

        let pool = connect_in_memory().await.expect("connect");
        let embedder: Arc<dyn EmbeddingProvider> =
            Arc::new(HrrEmbedder::new(HrrConfig::default()).expect("hrr"));

        let skills = vec![
            LoadedSkill {
                name: "code-review".into(),
                description: "review pull requests against a checklist".into(),
                version: None,
                user_invocable: false,
                triggers: vec![],
                source: SkillSource::Workspace,
                body: "Look at the diff. Check for tests.".into(),
            },
            LoadedSkill {
                name: "weather-fetcher".into(),
                description: "look up the current weather for a city".into(),
                version: None,
                user_invocable: false,
                triggers: vec![],
                source: SkillSource::Workspace,
                body: "Use web_fetch on the weather API.".into(),
            },
            LoadedSkill {
                name: "deploy".into(),
                description: "ship build artefacts to production".into(),
                version: None,
                user_invocable: false,
                triggers: vec![],
                source: SkillSource::Workspace,
                body: "Run the release script.".into(),
            },
        ];
        index_into(&skills, &pool).await.expect("index");

        backfill::skills_run_to_completion(pool.clone(), embedder.clone()).await;

        // Confirm vectors landed for every active skill.
        let candidates =
            havn_db::agent::skills_index::embedded_candidates(&pool, embedder.dimensions())
                .await
                .expect("embedded_candidates");
        assert_eq!(
            candidates.len(),
            3,
            "all skills should have vectors after backfill"
        );
        for c in &candidates {
            assert_eq!(c.embedding.len(), embedder.dimensions());
        }

        // Query overlaps strongly with code-review (review + PR + check).
        let handle: crate::embedding::EmbedderHandle = Some(embedder.clone());
        let hits = relevant_for(&pool, &handle, "please review my PR checklist", 3)
            .await
            .expect("search");
        assert!(!hits.is_empty(), "expected at least one match");
        assert_eq!(
            hits[0].name,
            "code-review",
            "code-review should rank first; got: {:?}",
            hits.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
        );

        // use_count should have bumped for the top hit.
        let curatable = havn_db::agent::skills_index::list_all_active(&pool, 50)
            .await
            .expect("list_all_active");
        let cr = curatable
            .iter()
            .find(|s| s.name == "code-review")
            .expect("code-review row");
        assert_eq!(cr.use_count, 1, "top hit should have use_count = 1");
        assert!(
            cr.last_used_at.is_some(),
            "top hit should have last_used_at set"
        );
    }

    #[test]
    fn render_for_prompt_emits_section_header_and_bodies() {
        let skills = vec![
            RetrievedSkill {
                name: "a".into(),
                description: "alpha".into(),
                body: "do A".into(),
            },
            RetrievedSkill {
                name: "b".into(),
                description: "beta".into(),
                body: "do B".into(),
            },
        ];
        let rendered = render_for_prompt(&skills).expect("some");
        assert!(rendered.contains("## Skills available right now"));
        assert!(rendered.contains("### a"));
        assert!(rendered.contains("### b"));
        assert!(rendered.contains("do A"));
        assert!(rendered.contains("do B"));
    }

    #[test]
    fn render_for_prompt_empty_returns_none() {
        assert!(render_for_prompt(&[]).is_none());
    }

    /// Real-world compatibility test: parse OC and Hermes SKILL.md files from
    /// `tests/fixtures/`. These are verbatim copies (see audit notes).
    /// All five MUST parse successfully — `name` and `description` are present
    /// in every one. Vendor metadata, `allowed-tools`, `homepage`, and inline
    /// JSON-flow YAML must all round-trip without erroring.
    #[test]
    fn parses_real_oc_and_hermes_skill_files() {
        const CASES: &[(&str, &str)] = &[
            ("oc_gh_issues.md", "gh-issues"),
            ("oc_discord.md", "discord"),
            ("oc_github.md", "github"),
            ("oc_apple_notes.md", "apple-notes"),
            ("hermes_dogfood.md", "dogfood"),
        ];

        let fixtures_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");

        for (file, expected_name) in CASES {
            let path = fixtures_dir.join(file);
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {}: {}", path.display(), e));
            let parsed = parse_skill(&content, SkillSource::Workspace)
                .unwrap_or_else(|e| panic!("parse failed for {file}: {e}"));
            assert_eq!(parsed.name, *expected_name, "{file}");
            assert!(!parsed.description.is_empty(), "{file}");
            assert!(!parsed.body.is_empty(), "{file}");
        }
    }
}
