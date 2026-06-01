//! `havn role {add,list,delete,show,set-policy}` — operator-side role
//! management (spec §6, §10.3).
//!
//! Same direct-DB pattern as `havn team` / `havn user`. Role policies
//! are JSON files on disk: `havn role set-policy <id> ./role.json`.
//! That keeps the CLI surface small while letting operators version
//! their role policies in their own dotfiles repo (spec §6.4 — "no
//! role marketplace").

use std::path::Path;
use std::str::FromStr as _;

use anyhow::Context as _;
use havn_core::{Policy, RoleId, TeamId};
use havn_db::repo::roles::{self, NewRole};

use crate::user::open_db_pub;

pub async fn add(team: &str, name: &str, policy_path: Option<&str>) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("--name must be non-empty");
    }
    let team_id = TeamId::from_str(team)
        .map_err(|_| anyhow::anyhow!("team id must be a UUID v7 (got {team:?})"))?;
    let policy = match policy_path {
        Some(p) => load_policy_file(p).await?,
        None => Policy::default(),
    };
    let pool = open_db_pub().await?;
    let role = roles::create(
        &pool,
        NewRole {
            team_id: Some(team_id),
            name: name.trim(),
            policy: &policy,
        },
    )
    .await
    .with_context(|| format!("creating role {name:?} in team {team_id}"))?;
    println!(
        "created role {} ({}) in team {}",
        role.id, role.name, team_id
    );
    Ok(())
}

pub async fn list(team: &str) -> anyhow::Result<()> {
    let team_id = TeamId::from_str(team)
        .map_err(|_| anyhow::anyhow!("team id must be a UUID v7 (got {team:?})"))?;
    let pool = open_db_pub().await?;
    let rows = roles::list_for_team(&pool, team_id)
        .await
        .with_context(|| format!("listing roles for team {team_id}"))?;
    if rows.is_empty() {
        println!("(no team-scoped roles — admin/member seeds live system-wide)");
        return Ok(());
    }
    for r in rows {
        println!("{}  {:16}  created={}", r.id, r.name, r.created_at);
    }
    Ok(())
}

pub async fn show(role: &str) -> anyhow::Result<()> {
    let role_id = RoleId::from_str(role)
        .map_err(|_| anyhow::anyhow!("role id must be a UUID v7 (got {role:?})"))?;
    let pool = open_db_pub().await?;
    let r = roles::find_by_id(&pool, role_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no role with id {role_id}"))?;
    println!("id:        {}", r.id);
    println!(
        "team_id:   {}",
        r.team_id.map_or("(system)".into(), |t| t.to_string())
    );
    println!("name:      {}", r.name);
    println!("created:   {}", r.created_at);
    println!("policy:");
    println!(
        "{}",
        serde_json::to_string_pretty(&r.policy).context("rendering policy")?
    );
    Ok(())
}

pub async fn set_policy(role: &str, policy_path: &str) -> anyhow::Result<()> {
    let role_id = RoleId::from_str(role)
        .map_err(|_| anyhow::anyhow!("role id must be a UUID v7 (got {role:?})"))?;
    let policy = load_policy_file(policy_path).await?;
    let pool = open_db_pub().await?;
    let updated = roles::set_policy(&pool, role_id, &policy)
        .await
        .with_context(|| format!("updating role {role_id}"))?;
    println!("updated role {} ({})", updated.id, updated.name);
    Ok(())
}

pub async fn delete(role: &str) -> anyhow::Result<()> {
    let role_id = RoleId::from_str(role)
        .map_err(|_| anyhow::anyhow!("role id must be a UUID v7 (got {role:?})"))?;
    let pool = open_db_pub().await?;
    roles::delete(&pool, role_id)
        .await
        .with_context(|| format!("deleting role {role_id}"))?;
    println!("deleted role {role_id}");
    Ok(())
}

async fn load_policy_file(path: &str) -> anyhow::Result<Policy> {
    let body = tokio::fs::read_to_string(Path::new(path))
        .await
        .with_context(|| format!("reading policy from {path}"))?;
    serde_json::from_str::<Policy>(&body).with_context(|| format!("parsing policy {path}"))
}
