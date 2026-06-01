//! Curator — periodic consolidation pass over the agent's skill library
//! (spec §9.5). Two phases:
//!
//! 1. **Rule-based aging**: archive any workspace skill whose
//!    `last_used_at` is older than [`STALE_THRESHOLD_DAYS`] *and* whose
//!    `use_count` is below a low-water mark. Pinned and bundled skills
//!    are immune. Archives via `skills_index::archive` + moves the
//!    SKILL.md dir to `workspace/.archive/<name>/`. Never deletes.
//!
//! 2. **LLM-driven consolidation**: send the surviving active skills
//!    to the LLM with a structured prompt asking for an umbrella
//!    consolidation plan. The model returns YAML; the curator parses,
//!    creates the umbrella skill on disk, and archives the absorbed
//!    skills. Failure modes (malformed YAML, missing names, body cap)
//!    fail-safe: the report records the error and no destructive
//!    action is taken.
//!
//! Both phases write a Markdown report to
//! `workspace/.curator/<utc-timestamp>.md` so the user can inspect
//! exactly what happened (audit trail; never deletes the source data).
//!
//! Inspired by Hermes's CURATOR_REVIEW_PROMPT and OpenClaw's
//! short-term-promotion scoring; the rule-based aging is the OpenClaw
//! pattern (recall-fresh wins) and the LLM phase is the Hermes pattern.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use havn_db::agent::skills_index::{self, CuratableSkill};
use havn_proto::{AgentToGateway, LlmOutcome, LlmRequest, LlmResponse};
use serde::Deserialize;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::tool_loop::Pending;

/// Skills idle for more than this number of days are eligible for
/// rule-based archive. Spec §9.5 gives 30d for "stale" / 90d for
/// "archive"; we use the archive threshold directly because Phase 2
/// doesn't expose a separate "stale" UI state — once unused that long
/// the safe move is to archive.
pub const STALE_THRESHOLD_DAYS: i64 = 90;

/// Use-count floor below which the rule-based archive applies. A skill
/// that was used 50+ times once and then went idle for 90 days is
/// probably still load-bearing; let the LLM consolidation phase decide
/// instead of auto-archiving on time alone.
pub const LOW_USE_FLOOR: i64 = 5;

/// Per-pass cap on the number of skills the curator considers. Bounds
/// the LLM prompt size and keeps any single pass cheap.
pub const PASS_CAP: u32 = 100;

/// Max age the LLM consolidation prompt body can grow to. Hermes uses
/// ~600 lines for its CURATOR_REVIEW_PROMPT; we keep an effective
/// per-skill cap so that 50 skills × 100 KB of body don't blow past
/// the LLM context window.
pub const PER_SKILL_PROMPT_BUDGET_BYTES: usize = 4_096;

/// Top-level result of a curator pass. Persisted to `.curator/` and
/// (optionally, in a future version) reported to the gateway for
/// dashboard surfacing.
#[derive(Debug, Default)]
pub struct CuratorReport {
    pub ran_at: DateTime<Utc>,
    pub considered: usize,
    pub archived_by_age: Vec<String>,
    pub consolidations: Vec<Consolidation>,
    pub prunings: Vec<Pruning>,
    pub errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Consolidation {
    pub into: String,
    #[serde(default)]
    pub umbrella_description: String,
    #[serde(default)]
    pub umbrella_body: String,
    #[serde(default)]
    pub from: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Pruning {
    pub skill: String,
    #[serde(default)]
    pub reason: String,
}

/// Plumbing the curator needs. Cloned cheaply per pass.
#[derive(Clone)]
pub struct Bridge {
    pub pool: SqlitePool,
    pub workspace_dir: Arc<PathBuf>,
    pub writer_tx: mpsc::Sender<AgentToGateway>,
    pub pending: Pending,
    /// Model the curator's LLM call uses. Pulled from runtime config so
    /// the operator can pin curator to a cheap model.
    pub model: String,
}

/// Run a single curator pass. Walks both phases, writes the report,
/// returns it to the caller (the scheduler logs counts; tests inspect
/// the structure).
pub async fn run_pass(bridge: &Bridge) -> anyhow::Result<CuratorReport> {
    let mut report = CuratorReport {
        ran_at: Utc::now(),
        ..Default::default()
    };

    let candidates = skills_index::list_curatable(&bridge.pool, PASS_CAP)
        .await
        .context("listing curatable skills")?;
    report.considered = candidates.len();
    info!(considered = candidates.len(), "curator pass starting");

    // Phase 1: rule-based aging.
    let cutoff = Utc::now() - ChronoDuration::days(STALE_THRESHOLD_DAYS);
    let mut survivors: Vec<CuratableSkill> = Vec::with_capacity(candidates.len());
    for skill in candidates {
        let stale = skill.last_used_at.is_none_or(|t| t < cutoff);
        if stale && skill.use_count < LOW_USE_FLOOR {
            match archive_skill(&bridge.pool, &bridge.workspace_dir, &skill.name).await {
                Ok(()) => {
                    info!(name = %skill.name, "curator archived stale skill");
                    report.archived_by_age.push(skill.name);
                }
                Err(e) => {
                    let msg = format!("archiving {}: {e}", skill.name);
                    warn!(error = %e, name = %skill.name, "skill archive failed");
                    report.errors.push(msg);
                }
            }
        } else {
            survivors.push(skill);
        }
    }

    // Phase 2: LLM consolidation. Skip when there's nothing useful to
    // ask about (≤1 surviving skill — nothing to consolidate against).
    if survivors.len() >= 2 {
        match run_consolidation(bridge, &survivors).await {
            Ok((consolidations, prunings)) => {
                apply_consolidations(bridge, &survivors, consolidations, prunings, &mut report)
                    .await;
            }
            Err(e) => {
                warn!(error = %e, "curator LLM consolidation failed");
                report.errors.push(format!("LLM consolidation: {e}"));
            }
        }
    } else {
        debug!(
            survivors = survivors.len(),
            "skipping LLM phase: nothing to consolidate"
        );
    }

    if let Err(e) = write_report(&bridge.workspace_dir, &report).await {
        warn!(error = %e, "writing curator report failed");
    }
    info!(
        archived = report.archived_by_age.len(),
        consolidated = report.consolidations.len(),
        pruned = report.prunings.len(),
        errors = report.errors.len(),
        "curator pass done"
    );
    Ok(report)
}

/// Soft-archive in DB + move SKILL.md dir to `workspace/.archive/<name>/`.
async fn archive_skill(pool: &SqlitePool, workspace: &Path, name: &str) -> anyhow::Result<()> {
    skills_index::archive(pool, name)
        .await
        .with_context(|| format!("DB archive {name}"))?;
    let src = workspace.join("skills").join(name);
    if !tokio::fs::try_exists(&src).await.unwrap_or(false) {
        // Skills may exist in DB without an on-disk dir if the row was
        // synthesised by some test path — that's fine, DB archive alone
        // is enough.
        return Ok(());
    }
    let archive_root = workspace.join(".archive");
    tokio::fs::create_dir_all(&archive_root)
        .await
        .context("mkdir .archive")?;
    let dst = archive_root.join(format!("{}-{}", name, Utc::now().format("%Y%m%dT%H%M%S")));
    tokio::fs::rename(&src, &dst)
        .await
        .with_context(|| format!("mv {} → {}", src.display(), dst.display()))?;
    Ok(())
}

/// Build the LLM prompt, dispatch, parse YAML response.
async fn run_consolidation(
    bridge: &Bridge,
    skills: &[CuratableSkill],
) -> anyhow::Result<(Vec<Consolidation>, Vec<Pruning>)> {
    let prompt = build_consolidation_prompt(skills);
    let request_id = Uuid::now_v7().to_string();

    let llm_req = LlmRequest {
        request_id: request_id.clone(),
        model: bridge.model.clone(),
        messages: serde_json::json!([
            { "role": "user", "content": prompt }
        ]),
        options: serde_json::json!({
            "max_tokens": 4096,
            "system": SYSTEM_PROMPT,
        }),
        billing_user_id: None,
    };

    let (resp_tx, resp_rx) = oneshot::channel();
    bridge
        .pending
        .lock()
        .await
        .insert(request_id.clone(), resp_tx);
    bridge
        .writer_tx
        .send(AgentToGateway::LlmRequest(llm_req))
        .await
        .map_err(|e| {
            // Best-effort cleanup of the pending entry.
            anyhow!("enqueue LlmRequest: {e}")
        })?;

    let response: LlmResponse = resp_rx
        .await
        .map_err(|_| anyhow!("LlmResponse channel closed for curator pass"))?;

    let body = match response.outcome {
        LlmOutcome::Ok => extract_text(&response.provider_response)
            .ok_or_else(|| anyhow!("LLM response had no text content"))?,
        LlmOutcome::Error { message } => {
            return Err(anyhow!("LLM error: {message}"));
        }
    };

    parse_consolidation_yaml(&body)
}

const SYSTEM_PROMPT: &str = "\
You are the curator for an autonomous agent's skill library. You read \
the agent's user-created skills and produce a consolidation plan in \
YAML. Be conservative: only propose merging skills that genuinely \
overlap or that would be more useful as one umbrella skill. Be \
suspicious of merging skills that look superficially similar but \
serve different intents. When in doubt, leave a skill alone.\n\
\n\
Your output is parsed by code. Output ONLY a YAML block, no other \
text. Empty lists are OK.";

fn build_consolidation_prompt(skills: &[CuratableSkill]) -> String {
    let mut prompt = String::with_capacity(8192);
    prompt.push_str(
        "Below are the agent's currently-active workspace skills. Each \
         entry shows the skill name, description, recent use count, \
         and body. Decide which (if any) should be:\n\
         \n\
         - **consolidated** into umbrella skills (multiple narrow \
           skills merged under a broader one with sub-sections); the \
           originals will be archived and the umbrella becomes the \
           agent's go-to.\n\
         - **pruned** (archived without replacement) because they're \
           obsolete, redundant, or low-quality.\n\
         \n\
         Respond with a YAML block in exactly this shape:\n\
         \n\
         ```yaml\n\
         consolidations:\n\
           - into: umbrella-name-kebab-case\n\
             umbrella_description: one-liner for the umbrella skill\n\
             umbrella_body: |\n\
               full markdown body for the umbrella, including\n\
               all the sub-section content from the absorbed skills.\n\
             from: [skill-a, skill-b, skill-c]\n\
         prunings:\n\
           - skill: name-of-skill-to-archive\n\
             reason: one-liner explaining why it's safe to archive\n\
         ```\n\
         \n\
         If nothing needs consolidation or pruning, return both lists \
         empty. Skill names in `from` and `skill` MUST exactly match \
         the names below.\n\
         \n\
         === SKILLS ===\n\
         \n",
    );
    for s in skills {
        prompt.push_str(&format!(
            "## {}\n\n_description_: {}\n_use_count_: {}\n_last_used_at_: {}\n\n",
            s.name,
            s.description,
            s.use_count,
            s.last_used_at
                .map_or_else(|| "never".into(), |t| t.format("%Y-%m-%d").to_string())
        ));
        let body_excerpt = if s.body.len() > PER_SKILL_PROMPT_BUDGET_BYTES {
            let head: String = s
                .body
                .chars()
                .take(PER_SKILL_PROMPT_BUDGET_BYTES.saturating_sub(64))
                .collect();
            format!("{head}\n…(body truncated for curator prompt)")
        } else {
            s.body.clone()
        };
        prompt.push_str(&body_excerpt);
        prompt.push_str("\n\n---\n\n");
    }
    prompt
}

#[derive(Debug, Deserialize)]
struct Plan {
    #[serde(default)]
    consolidations: Vec<Consolidation>,
    #[serde(default)]
    prunings: Vec<Pruning>,
}

/// Pull a YAML plan out of an LLM response. Tolerant of code-fence
/// wrapping (```yaml ... ``` or ``` ... ```).
fn parse_consolidation_yaml(text: &str) -> anyhow::Result<(Vec<Consolidation>, Vec<Pruning>)> {
    let stripped = strip_code_fences(text.trim());
    let plan: Plan = serde_norway::from_str(stripped).with_context(|| {
        format!(
            "parsing consolidation YAML; first 200 chars: {:?}",
            stripped.chars().take(200).collect::<String>()
        )
    })?;
    Ok((plan.consolidations, plan.prunings))
}

fn strip_code_fences(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed
        .strip_prefix("```yaml")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
    }
    trimmed
}

/// Apply consolidation/pruning decisions to disk + DB. Validates
/// every name reference against the survivor set so a hallucinated
/// skill name can't trick the curator into writing junk.
async fn apply_consolidations(
    bridge: &Bridge,
    survivors: &[CuratableSkill],
    consolidations: Vec<Consolidation>,
    prunings: Vec<Pruning>,
    report: &mut CuratorReport,
) {
    let known: std::collections::HashSet<&str> =
        survivors.iter().map(|s| s.name.as_str()).collect();

    for c in consolidations {
        if c.umbrella_body.trim().is_empty() || c.umbrella_description.trim().is_empty() {
            report.errors.push(format!(
                "consolidation {:?}: empty umbrella body or description",
                c.into
            ));
            continue;
        }
        if !validate_kebab_name(&c.into) {
            report.errors.push(format!(
                "consolidation skipped: {:?} is not a valid kebab-case name",
                c.into
            ));
            continue;
        }
        // Reject if any `from` name isn't in the active set — guards
        // against the model hallucinating skill names.
        let unknown: Vec<&String> = c
            .from
            .iter()
            .filter(|n| !known.contains(n.as_str()))
            .collect();
        if !unknown.is_empty() {
            report.errors.push(format!(
                "consolidation {:?}: unknown source skills {unknown:?}",
                c.into
            ));
            continue;
        }
        if c.from.len() < 2 {
            report.errors.push(format!(
                "consolidation {:?}: needs ≥2 source skills",
                c.into
            ));
            continue;
        }
        // Write the umbrella skill, then archive the originals.
        if let Err(e) = write_umbrella(
            &bridge.pool,
            &bridge.workspace_dir,
            &c.into,
            &c.umbrella_description,
            &c.umbrella_body,
            &c.from,
        )
        .await
        {
            report
                .errors
                .push(format!("writing umbrella {:?}: {e}", c.into));
            continue;
        }
        for src in &c.from {
            if let Err(e) = archive_skill(&bridge.pool, &bridge.workspace_dir, src).await {
                report
                    .errors
                    .push(format!("archiving absorbed {src:?}: {e}"));
            }
        }
        report.consolidations.push(c);
    }

    for p in prunings {
        if !known.contains(p.skill.as_str()) {
            report
                .errors
                .push(format!("pruning skipped: unknown skill {:?}", p.skill));
            continue;
        }
        if let Err(e) = archive_skill(&bridge.pool, &bridge.workspace_dir, &p.skill).await {
            report.errors.push(format!("pruning {:?}: {e}", p.skill));
            continue;
        }
        report.prunings.push(p);
    }
}

fn validate_kebab_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

async fn write_umbrella(
    pool: &SqlitePool,
    workspace: &Path,
    name: &str,
    description: &str,
    body: &str,
    absorbed: &[String],
) -> anyhow::Result<()> {
    let dir = workspace.join("skills").join(name);
    let path = dir.join("SKILL.md");
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(anyhow!("umbrella name {name:?} already exists"));
    }
    tokio::fs::create_dir_all(&dir).await?;
    let mut content = String::with_capacity(body.len() + 256);
    content.push_str("---\n");
    content.push_str(&format!("name: {name}\n"));
    content.push_str(&format!("description: {description}\n"));
    content.push_str("metadata:\n");
    content.push_str("  havn:\n");
    content.push_str("    source: agent_created\n");
    content.push_str("    consolidated_from:\n");
    for s in absorbed {
        content.push_str(&format!("      - {s}\n"));
    }
    content.push_str("---\n\n");
    content.push_str(body.trim_end());
    content.push('\n');
    tokio::fs::write(&path, &content).await?;

    // Index the new skill so it's immediately retrievable.
    sqlx::query(
        "INSERT INTO skills_index (id, name, description, body, source) \
         VALUES (?1, ?2, ?3, ?4, 'workspace') \
         ON CONFLICT(name) DO UPDATE SET \
           description = excluded.description, \
           body        = excluded.body",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(name)
    .bind(description)
    .bind(body)
    .execute(pool)
    .await?;
    Ok(())
}

async fn write_report(workspace: &Path, report: &CuratorReport) -> anyhow::Result<()> {
    let dir = workspace.join(".curator");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.md", report.ran_at.format("%Y%m%dT%H%M%SZ")));
    let mut out = String::new();
    out.push_str(&format!(
        "# Curator pass — {}\n\n",
        report.ran_at.format("%Y-%m-%d %H:%M UTC")
    ));
    out.push_str(&format!("- skills considered: {}\n", report.considered));
    out.push_str(&format!(
        "- archived by age (>{STALE_THRESHOLD_DAYS}d unused, <{LOW_USE_FLOOR} uses): {}\n",
        report.archived_by_age.len()
    ));
    out.push_str(&format!(
        "- consolidated by LLM: {}\n",
        report.consolidations.len()
    ));
    out.push_str(&format!("- pruned by LLM: {}\n", report.prunings.len()));
    out.push_str(&format!("- errors: {}\n\n", report.errors.len()));

    if !report.archived_by_age.is_empty() {
        out.push_str("## Archived by age\n\n");
        for n in &report.archived_by_age {
            out.push_str(&format!("- `{n}`\n"));
        }
        out.push('\n');
    }
    if !report.consolidations.is_empty() {
        out.push_str("## Consolidations\n\n");
        for c in &report.consolidations {
            out.push_str(&format!(
                "- **{}** ← {}\n",
                c.into,
                c.from
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push('\n');
    }
    if !report.prunings.is_empty() {
        out.push_str("## Prunings\n\n");
        for p in &report.prunings {
            out.push_str(&format!("- `{}` — {}\n", p.skill, p.reason));
        }
        out.push('\n');
    }
    if !report.errors.is_empty() {
        out.push_str("## Errors\n\n");
        for e in &report.errors {
            out.push_str(&format!("- {e}\n"));
        }
    }
    tokio::fs::write(&path, out).await?;
    Ok(())
}

/// Concatenate `text` content blocks from an Anthropic Messages API response.
fn extract_text(provider_response: &serde_json::Value) -> Option<String> {
    let content = provider_response.get("content")?.as_array()?;
    let mut out = String::new();
    for block in content {
        if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
            if let Some(t) = block.get("text").and_then(serde_json::Value::as_str) {
                out.push_str(t);
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

#[allow(dead_code, reason = "scheduler helper used in main.rs / future tests")]
pub fn pass_cadence() -> Duration {
    // 24 h check; the scheduler decides per-tick whether to actually fire.
    Duration::from_secs(24 * 60 * 60)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parse_consolidation_yaml_handles_fenced_block() {
        let raw = "```yaml\nconsolidations:\n  - into: x\n    umbrella_description: hi\n    umbrella_body: |\n      body\n    from: [a, b]\nprunings: []\n```";
        let (cons, pru) = parse_consolidation_yaml(raw).expect("parse");
        assert_eq!(cons.len(), 1);
        assert_eq!(cons[0].into, "x");
        assert_eq!(cons[0].from, vec!["a", "b"]);
        assert!(pru.is_empty());
    }

    #[test]
    fn parse_consolidation_yaml_handles_unfenced() {
        let raw = "consolidations: []\nprunings:\n  - skill: drop\n    reason: redundant\n";
        let (cons, pru) = parse_consolidation_yaml(raw).expect("parse");
        assert!(cons.is_empty());
        assert_eq!(pru.len(), 1);
        assert_eq!(pru[0].skill, "drop");
    }

    #[test]
    fn parse_consolidation_yaml_rejects_garbage() {
        let raw = "this is not yaml at all, just prose";
        // serde_norway can sometimes parse free text as a string; the
        // expected failure mode is "missing required structure".
        let result = parse_consolidation_yaml(raw);
        assert!(
            result.is_err()
                || (result
                    .as_ref()
                    .map(|(c, p)| c.is_empty() && p.is_empty())
                    .unwrap_or(false)),
            "expected error or empty plan, got {:?}",
            result.map(|_| "ok")
        );
    }

    #[test]
    fn validate_kebab_name_rejects_path_traversal() {
        assert!(!validate_kebab_name("../etc"));
        assert!(!validate_kebab_name("a/b"));
        assert!(!validate_kebab_name("UPPER"));
        assert!(!validate_kebab_name(""));
        assert!(validate_kebab_name("good-name"));
        assert!(validate_kebab_name("snake_case_2"));
    }

    #[test]
    fn build_prompt_truncates_oversize_body() {
        let big_body = "x".repeat(20_000);
        let skills = vec![CuratableSkill {
            id: "id".into(),
            name: "fat".into(),
            description: "d".into(),
            body: big_body,
            source: skills_index::Source::Workspace,
            pinned: false,
            last_used_at: None,
            use_count: 0,
        }];
        let prompt = build_consolidation_prompt(&skills);
        assert!(prompt.contains("body truncated"));
        assert!(prompt.len() < 30_000);
    }
}
