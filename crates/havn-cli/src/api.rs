//! Shared HTTP client + URL helpers for the gateway-management CLI subcommands.

use std::time::Duration;

use anyhow::{Context as _, anyhow};
use reqwest::Client;
use serde::de::DeserializeOwned;

/// Override for tests + non-default deployments. Defaults to the same loopback
/// address the gateway binds to in single-user mode (spec §11 / §12.1).
const ENV_GATEWAY: &str = "HAVN_GATEWAY";
const DEFAULT_GATEWAY: &str = "http://127.0.0.1:8080";

#[derive(Debug, Clone)]
pub struct Gateway {
    pub base: String,
    pub http: Client,
}

impl Gateway {
    pub fn from_env() -> anyhow::Result<Self> {
        let base = std::env::var(ENV_GATEWAY).unwrap_or_else(|_| DEFAULT_GATEWAY.into());
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self { base, http })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    /// GET <base><path>, parse JSON response. Returns Err with the body text
    /// when status is non-2xx — those messages are operator-readable.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let resp = self
            .http
            .get(self.url(path))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        Self::parse(resp).await
    }

    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<T> {
        let resp = self
            .http
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        Self::parse(resp).await
    }

    #[allow(
        dead_code,
        reason = "exposed for upcoming stop/start convenience helpers"
    )]
    pub async fn post_empty(&self, path: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(self.url(path))
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        Self::parse_empty(resp).await
    }

    pub async fn delete(&self, path: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(self.url(path))
            .send()
            .await
            .with_context(|| format!("DELETE {path}"))?;
        Self::parse_empty(resp).await
    }

    async fn parse<T: DeserializeOwned>(resp: reqwest::Response) -> anyhow::Result<T> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("gateway returned {status}: {body}"));
        }
        resp.json::<T>().await.context("parsing response")
    }

    async fn parse_empty(resp: reqwest::Response) -> anyhow::Result<()> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("gateway returned {status}: {body}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn url_joins_paths_without_double_slash() {
        let gw = Gateway {
            base: "http://localhost:8080/".into(),
            http: Client::new(),
        };
        assert_eq!(gw.url("/agents"), "http://localhost:8080/agents");
    }

    #[test]
    fn url_joins_paths_when_base_has_no_trailing_slash() {
        let gw = Gateway {
            base: "http://localhost:8080".into(),
            http: Client::new(),
        };
        assert_eq!(gw.url("/agents"), "http://localhost:8080/agents");
    }
}
