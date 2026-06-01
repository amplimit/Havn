//! `havn agent ...` — agent CRUD via the gateway REST API.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::api::Gateway;

#[derive(Debug, Deserialize)]
struct AgentView {
    id: String,
    name: String,
    status: String,
    created_at: DateTime<Utc>,
}

pub async fn list() -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let agents: Vec<AgentView> = gw.get_json("/agents").await?;
    if agents.is_empty() {
        println!("no agents — `havn agent create <name>` to make one");
        return Ok(());
    }
    println!("{:36}  {:14}  {:20}  CREATED", "ID", "STATUS", "NAME");
    for a in agents {
        println!(
            "{:36}  {:14}  {:20}  {}",
            a.id,
            a.status,
            truncate(&a.name, 20),
            a.created_at.format("%Y-%m-%d %H:%M")
        );
    }
    Ok(())
}

pub async fn create(name: &str) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let agent: AgentView = gw.post_json("/agents", &json!({ "name": name })).await?;
    println!("✓ created agent {} ({})", agent.name, agent.id);
    println!("  next: havn agent start {}", agent.id);
    Ok(())
}

pub async fn start(id: &str) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let agent: AgentView = gw
        .post_json(&format!("/agents/{id}/start"), &json!({}))
        .await?;
    println!("✓ agent {} status: {}", agent.name, agent.status);
    Ok(())
}

pub async fn stop(id: &str) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let agent: AgentView = gw
        .post_json(&format!("/agents/{id}/stop"), &json!({}))
        .await?;
    println!("✓ agent {} status: {}", agent.name, agent.status);
    Ok(())
}

pub async fn delete(id: &str) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    gw.delete(&format!("/agents/{id}")).await?;
    println!("✓ deleted agent {id}");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hello", 20), "hello");
    }

    #[test]
    fn truncate_clips_long_strings() {
        let r = truncate("the-quick-brown-fox-jumps", 10);
        assert_eq!(r.chars().count(), 10);
        assert!(r.ends_with('…'));
    }
}
