//! `havn credential ...` — credential CRUD via the gateway REST API.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::api::Gateway;

#[derive(Debug, Deserialize)]
struct CredentialView {
    id: String,
    provider: String,
    priority: i32,
    enabled: bool,
    created_at: DateTime<Utc>,
}

pub async fn list() -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let creds: Vec<CredentialView> = gw.get_json("/credentials").await?;
    if creds.is_empty() {
        println!("no credentials — `havn credential add <provider> <key>` to add one");
        return Ok(());
    }
    println!(
        "{:36}  {:12}  {:>8}  {:<8}  CREATED",
        "ID", "PROVIDER", "PRIORITY", "ENABLED"
    );
    for c in creds {
        println!(
            "{:36}  {:12}  {:>8}  {:<8}  {}",
            c.id,
            c.provider,
            c.priority,
            if c.enabled { "yes" } else { "no" },
            c.created_at.format("%Y-%m-%d %H:%M")
        );
    }
    Ok(())
}

pub async fn add(
    provider: &str,
    api_key: &str,
    priority: i32,
    name: Option<&str>,
) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    let mut body = json!({
        "provider": provider,
        "api_key": api_key,
        "priority": priority,
        "limits": {},
    });
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    let cred: CredentialView = gw.post_json("/credentials", &body).await?;
    let suffix = match name {
        Some(n) => format!(", name {n:?}"),
        None => String::new(),
    };
    println!(
        "✓ added {} credential {} (priority {}{})",
        cred.provider, cred.id, cred.priority, suffix
    );
    Ok(())
}

pub async fn delete(id: &str) -> anyhow::Result<()> {
    let gw = Gateway::from_env()?;
    gw.delete(&format!("/credentials/{id}")).await?;
    println!("✓ deleted credential {id}");
    Ok(())
}
