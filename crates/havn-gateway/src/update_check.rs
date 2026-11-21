//! The gateway's daily look for a newer release (issue #236), shown beside
//! its version in the dashboard. Off with `update_check = false`.

use std::sync::{LazyLock, PoisonError, RwLock};
use std::time::Duration;

use tracing::{debug, info, warn};

/// How often the gateway looks.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long after starting it first looks.
const FIRST_AFTER: Duration = Duration::from_secs(60);

/// How long a look may take.
const WITHIN: Duration = Duration::from_secs(15);

/// The newer release found last, if any.
static NEWER: LazyLock<RwLock<Option<String>>> = LazyLock::new(Default::default);

/// The newer release the gateway knows of, as its tag.
#[must_use]
pub fn newer() -> Option<String> {
    NEWER.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Note the latest release's `tag`, kept when it is newer than this
/// gateway.
fn note(tag: &str) {
    let newer = havn_core::release::newer(tag, env!("CARGO_PKG_VERSION")).then(|| tag.to_string());
    let mut kept = NEWER.write().unwrap_or_else(PoisonError::into_inner);
    if newer.is_some() && *kept != newer {
        info!(
            release = tag,
            "a newer Havn release is out: `havn update` installs it"
        );
    }
    *kept = newer;
}

/// The latest release's tag, from GitHub.
async fn latest(http: &reqwest::Client) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let response = http
        .get(havn_core::release::LATEST_RELEASE_API)
        .header(reqwest::header::USER_AGENT, "havn-gateway")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(WITHIN)
        .send()
        .await?;
    // The gateway's client follows no redirect: a moved repository answers
    // 3xx, said like any other failure.
    if !response.status().is_success() {
        anyhow::bail!("GitHub answered {}", response.status());
    }
    let release: Release = response.json().await?;
    Ok(release.tag_name)
}

/// Look for a newer release now, and every day.
pub fn spawn(http: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_AFTER).await;
        let mut failed_before = false;
        loop {
            match latest(&http).await {
                Ok(tag) => {
                    note(&tag);
                    failed_before = false;
                }
                // Said once while it keeps failing, then quietly.
                Err(e) if !failed_before => {
                    warn!(error = %e, "could not look for a newer release");
                    failed_before = true;
                }
                Err(e) => debug!(error = %e, "could not look for a newer release"),
            }
            tokio::time::sleep(EVERY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_newer_release_is_kept() {
        note("v0.0.1");
        assert_eq!(newer(), None);
        note("v999.0.0");
        assert_eq!(newer().as_deref(), Some("v999.0.0"));
        note(&format!("v{}", env!("CARGO_PKG_VERSION")));
        assert_eq!(newer(), None, "once on it, nothing to say");
    }
}
