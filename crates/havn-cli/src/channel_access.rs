//! `havn invite {create,list,revoke}` and `havn access {list,revoke}` —
//! invite-code pairing for channel DMs (issue #25).
//!
//! Direct-DB like `havn user`. The gateway reads `channel_allowlist` on
//! every inbound message, so admissions and revocations apply without a
//! restart. `allow_from` entries from config are shown alongside but can
//! only be changed in config.

use std::str::FromStr as _;

use anyhow::Context as _;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use havn_db::DbError;
use havn_db::repo::channel_access::{self, AllowedSender, CreatedInvite, Invite};
use uuid::Uuid;

use crate::paths::{self, CliConfig};
use crate::user::open_db_pub;

const MAX_TTL: Duration = Duration::days(30);

/// `--expires` values: a positive integer with an `s`, `m`, `h` or `d`
/// suffix, at most 30 days.
pub fn parse_ttl(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.len().saturating_sub(1);
    let (n, unit) = (s.get(..split).unwrap_or(""), s.get(split..).unwrap_or(""));
    let n: i64 = n
        .parse()
        .map_err(|_| format!("expected a duration like 30m, 24h or 7d (got {s:?})"))?;
    let ttl = match unit {
        "s" => Duration::try_seconds(n),
        "m" => Duration::try_minutes(n),
        "h" => Duration::try_hours(n),
        "d" => Duration::try_days(n),
        _ => return Err(format!("unknown unit in {s:?}; use s, m, h or d")),
    }
    .ok_or_else(|| format!("{s:?} is out of range"))?;
    if ttl <= Duration::zero() || ttl > MAX_TTL {
        return Err(format!("{s:?} must be between 1s and 30d"));
    }
    Ok(ttl)
}

fn check_part(what: &str, value: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() || value.contains('|') {
        anyhow::bail!("{what} must be non-empty and must not contain '|' (got {value:?})");
    }
    Ok(())
}

/// Problems with the config that would keep an invite from working.
/// Empty when no config file is found: the CLI cannot tell then.
fn account_warnings(cfg: &CliConfig, channel: &str, account: &str) -> Vec<String> {
    if cfg.channels.is_empty() {
        return Vec::new();
    }
    match cfg.account(channel, account) {
        None => vec![format!(
            "no [[channels.{channel}.accounts]] entry with id {account:?} in config; \
             the gateway will not accept messages for it"
        )],
        Some(acct) if !acct.is_allowlist() => vec![format!(
            "{channel}/{account} has dm_policy = {:?}: anyone can already message it, \
             and the code will be passed to the agent as an ordinary message",
            acct.dm_policy.as_deref().unwrap_or_default()
        )],
        Some(_) => Vec::new(),
    }
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

async fn create_with(
    pool: &sqlx::SqlitePool,
    channel: &str,
    account: &str,
    ttl: Duration,
) -> anyhow::Result<CreatedInvite> {
    check_part("--channel", channel)?;
    check_part("--account", account)?;
    channel_access::create_invite(pool, channel, account, ttl)
        .await
        .context("creating invite")
}

pub async fn invite_create(channel: &str, account: &str, ttl: Duration) -> anyhow::Result<()> {
    let cfg = paths::load_config()?;
    for w in account_warnings(&cfg, channel, account) {
        eprintln!("warning: {w}");
    }
    let pool = open_db_pub().await?;
    let created = create_with(&pool, channel, account, ttl).await?;
    println!("{}", created.code);
    println!(
        "invite {} for {channel}/{account}, single use, expires {}",
        created.invite.id,
        ts(created.invite.expires_at)
    );
    println!(
        "The code is shown only once (havn stores its hash). The sender messages it to the bot."
    );
    Ok(())
}

fn invite_lines(invites: &[Invite], now: DateTime<Utc>) -> Vec<String> {
    invites
        .iter()
        .map(|i| {
            format!(
                "{}  {:8}  {}/{}  created={}  expires={}  used_by={}",
                i.id,
                i.status(now).as_str(),
                i.channel,
                i.account_id,
                ts(i.created_at),
                ts(i.expires_at),
                i.used_by_sender.as_deref().unwrap_or("-"),
            )
        })
        .collect()
}

pub async fn invite_list(channel: Option<&str>, account: Option<&str>) -> anyhow::Result<()> {
    let pool = open_db_pub().await?;
    let invites = channel_access::list_invites(&pool, channel, account)
        .await
        .context("listing invites")?;
    if invites.is_empty() {
        println!("(no invites)");
    }
    for line in invite_lines(&invites, Utc::now()) {
        println!("{line}");
    }
    Ok(())
}

async fn revoke_invite_with(pool: &sqlx::SqlitePool, id: &str) -> anyhow::Result<()> {
    let id =
        Uuid::from_str(id).map_err(|_| anyhow::anyhow!("invite id must be a UUID (got {id:?})"))?;
    match channel_access::revoke_invite(pool, id).await {
        Ok(()) => Ok(()),
        Err(DbError::NotFound) => anyhow::bail!("no invite with id {id}"),
        Err(DbError::Conflict(_)) => anyhow::bail!(
            "invite {id} was already used; remove the sender it admitted with `havn access revoke`"
        ),
        Err(e) => Err(e).context("revoking invite"),
    }
}

pub async fn invite_revoke(id: &str) -> anyhow::Result<()> {
    let pool = open_db_pub().await?;
    revoke_invite_with(&pool, id).await?;
    println!("revoked invite {id}");
    Ok(())
}

/// Whether config admits `sender` through `allow_from`.
fn in_allow_from(cfg: &CliConfig, channel: &str, account: &str, sender: &str) -> bool {
    cfg.account(channel, account)
        .is_some_and(|a| a.is_allowlist() && a.allow_from.iter().any(|s| s == sender))
}

/// Why the gateway ignores invite grants for this account, mirroring its
/// start-up warning. Nothing to say without a config file.
fn grant_note(
    cfg: &CliConfig,
    config_found: bool,
    channel: &str,
    account: &str,
) -> Option<&'static str> {
    if !config_found {
        return None;
    }
    match cfg.account(channel, account) {
        None => Some("not in effect: account is not configured"),
        Some(a) if !a.is_allowlist() => Some("not in effect: dm_policy is not allowlist"),
        Some(_) => None,
    }
}

fn access_lines(
    cfg: &CliConfig,
    config_found: bool,
    allowed: &[AllowedSender],
    channel: Option<&str>,
    account: Option<&str>,
) -> Vec<String> {
    let wanted =
        |ch: &str, acct: &str| channel.is_none_or(|c| c == ch) && account.is_none_or(|a| a == acct);
    let mut lines = Vec::new();
    for (ch, entry) in &cfg.channels {
        for acct in entry.accounts.iter().filter(|a| a.is_allowlist()) {
            if !wanted(ch, &acct.id) {
                continue;
            }
            for sender in &acct.allow_from {
                lines.push(format!("{ch}/{}  {sender}  source=config", acct.id));
            }
        }
    }
    for a in allowed.iter().filter(|a| wanted(&a.channel, &a.account_id)) {
        let invite = a
            .invite_id
            .map_or_else(|| "-".to_string(), |id| id.to_string());
        let mut line = format!(
            "{}/{}  {}  source=invite  invite={invite}  registered={}",
            a.channel,
            a.account_id,
            a.sender_id,
            ts(a.registered_at)
        );
        if let Some(note) = grant_note(cfg, config_found, &a.channel, &a.account_id) {
            line.push_str(&format!("  note=\"{note}\""));
        }
        lines.push(line);
    }
    lines
}

pub async fn access_list(channel: Option<&str>, account: Option<&str>) -> anyhow::Result<()> {
    let cfg = paths::load_config()?;
    let pool = open_db_pub().await?;
    let allowed = channel_access::list_allowed(&pool)
        .await
        .context("listing allowed senders")?;
    let lines = access_lines(
        &cfg,
        paths::config_path().is_some(),
        &allowed,
        channel,
        account,
    );
    if lines.is_empty() {
        println!("(no allowed senders)");
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// Returns a warning when the sender stays admitted through `allow_from`.
async fn revoke_sender_with(
    pool: &sqlx::SqlitePool,
    cfg: &CliConfig,
    channel: &str,
    account: &str,
    sender: &str,
) -> anyhow::Result<Option<String>> {
    let in_config = in_allow_from(cfg, channel, account, sender);
    match channel_access::revoke_sender(pool, channel, account, sender).await {
        Ok(()) => Ok(in_config.then(|| {
            format!(
                "{sender} is also listed in allow_from for {channel}/{account} in config and \
                 can still message the bot; remove it there and restart the gateway"
            )
        })),
        Err(DbError::NotFound) => {
            if in_config {
                anyhow::bail!(
                    "{sender} is listed in allow_from for {channel}/{account} in config; \
                     remove it there and restart the gateway"
                );
            }
            anyhow::bail!("{sender} was not admitted by invite to {channel}/{account}")
        }
        Err(e) => Err(e).context("revoking access"),
    }
}

pub async fn access_revoke(channel: &str, account: &str, sender: &str) -> anyhow::Result<()> {
    let cfg = paths::load_config()?;
    let pool = open_db_pub().await?;
    let warning = revoke_sender_with(&pool, &cfg, channel, account, sender).await?;
    println!(
        "revoked {sender}'s invite access on {channel}/{account}; applies to their next message"
    );
    if let Some(w) = warning {
        eprintln!("warning: {w}");
    }
    Ok(())
}

async fn revoke_all_with(
    pool: &sqlx::SqlitePool,
    channel: Option<&str>,
    account: &str,
) -> anyhow::Result<u64> {
    if let Some(ch) = channel {
        check_part("--channel", ch)?;
    }
    check_part("--account", account)?;
    channel_access::revoke_account(pool, channel, account)
        .await
        .context("revoking access")
}

pub async fn access_revoke_all(channel: Option<&str>, account: &str) -> anyhow::Result<()> {
    let pool = open_db_pub().await?;
    let n = revoke_all_with(&pool, channel, account).await?;
    let scope = channel.map_or_else(
        || format!("{account} on every channel"),
        |ch| format!("{ch}/{account}"),
    );
    println!(
        "revoked invite access of {n} sender(s) on {scope}; allow_from entries in config are unchanged"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn config(toml: &str) -> CliConfig {
        toml::from_str(toml).expect("config")
    }

    const CONFIG: &str = r#"
        [[channels.telegram.accounts]]
        id = "bot"
        adapter_token_ref = "secret:channel:telegram:bot"
        allow_from = ["tg:toml"]

        [[channels.telegram.accounts]]
        id = "public-bot"
        adapter_token_ref = "secret:channel:telegram:public-bot"
        dm_policy = "open"
        allow_from = ["tg:ignored"]
    "#;

    #[test]
    fn parse_ttl_accepts_units_and_bounds() {
        assert_eq!(parse_ttl("24h"), Ok(Duration::hours(24)));
        assert_eq!(parse_ttl("30m"), Ok(Duration::minutes(30)));
        assert_eq!(parse_ttl("7d"), Ok(Duration::days(7)));
        assert_eq!(parse_ttl("90s"), Ok(Duration::seconds(90)));
        for bad in [
            "",
            "h",
            "24",
            "0h",
            "-1h",
            "31d",
            "1w",
            "1.5h",
            "99999999999999999d",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn warnings_flag_unknown_and_open_accounts() {
        let cfg = config(CONFIG);
        assert!(account_warnings(&cfg, "telegram", "bot").is_empty());
        assert_eq!(account_warnings(&cfg, "telegram", "nope").len(), 1);
        assert!(account_warnings(&cfg, "telegram", "public-bot")[0].contains("open"));
        assert!(account_warnings(&CliConfig::default(), "telegram", "nope").is_empty());
    }

    #[tokio::test]
    async fn create_list_revoke_round_trip() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        assert!(
            create_with(&pool, "tele|gram", "bot", Duration::hours(1))
                .await
                .is_err()
        );
        let created = create_with(&pool, "telegram", "bot", Duration::hours(1))
            .await
            .expect("create");
        let invites = channel_access::list_invites(&pool, None, None)
            .await
            .unwrap();
        let lines = invite_lines(&invites, Utc::now());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("active"), "{}", lines[0]);
        assert!(
            !lines[0].contains(&created.code),
            "list must not show the code"
        );

        revoke_invite_with(&pool, &created.invite.id.to_string())
            .await
            .expect("revoke");
        let invites = channel_access::list_invites(&pool, Some("telegram"), Some("bot"))
            .await
            .unwrap();
        assert!(invite_lines(&invites, Utc::now())[0].contains("revoked"));
        assert!(revoke_invite_with(&pool, "not-a-uuid").await.is_err());
        assert!(
            revoke_invite_with(&pool, &Uuid::now_v7().to_string())
                .await
                .is_err()
        );
    }

    async fn admit(pool: &sqlx::SqlitePool, channel: &str, account: &str, sender: &str) {
        let created = create_with(pool, channel, account, Duration::hours(1))
            .await
            .unwrap();
        channel_access::redeem(pool, channel, account, sender, &created.code, Utc::now())
            .await
            .unwrap()
            .expect("valid");
    }

    #[tokio::test]
    async fn revoking_a_sender_also_in_allow_from_warns() {
        let cfg = config(CONFIG);
        let pool = havn_db::connect_in_memory().await.expect("db");
        admit(&pool, "telegram", "bot", "tg:toml").await;
        let warning = revoke_sender_with(&pool, &cfg, "telegram", "bot", "tg:toml")
            .await
            .expect("the invite grant is removed");
        assert!(
            warning
                .as_deref()
                .is_some_and(|w| w.contains("allow_from") && w.contains("still")),
            "{warning:?}"
        );
        assert!(
            !channel_access::is_allowed(&pool, "telegram", "bot", "tg:toml")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn access_list_flags_grants_the_gateway_ignores() {
        let cfg = config(CONFIG);
        let pool = havn_db::connect_in_memory().await.expect("db");
        admit(&pool, "telegram", "public-bot", "tg:1").await;
        admit(&pool, "telegram", "gone-bot", "tg:2").await;
        let allowed = channel_access::list_allowed(&pool).await.unwrap();
        let lines = access_lines(&cfg, true, &allowed, None, Some("public-bot"));
        assert_eq!(
            lines.len(),
            1,
            "allow_from of an open account is not listed"
        );
        assert!(
            lines[0].contains("dm_policy is not allowlist"),
            "{}",
            lines[0]
        );
        let lines = access_lines(&cfg, true, &allowed, None, Some("gone-bot"));
        assert!(
            lines[0].contains("account is not configured"),
            "{}",
            lines[0]
        );
        let lines = access_lines(&CliConfig::default(), false, &allowed, None, None);
        assert!(lines.iter().all(|l| !l.contains("note=")), "{lines:?}");
    }

    #[tokio::test]
    async fn revoke_all_clears_an_account() {
        let pool = havn_db::connect_in_memory().await.expect("db");
        admit(&pool, "telegram", "bot", "tg:1").await;
        admit(&pool, "telegram", "bot", "tg:2").await;
        admit(&pool, "slack", "bot", "slack:1").await;
        admit(&pool, "telegram", "other-bot", "tg:3").await;
        assert_eq!(
            revoke_all_with(&pool, Some("telegram"), "bot")
                .await
                .unwrap(),
            2
        );
        assert_eq!(revoke_all_with(&pool, None, "bot").await.unwrap(), 1);
        assert_eq!(revoke_all_with(&pool, None, "bot").await.unwrap(), 0);
        assert!(revoke_all_with(&pool, None, "").await.is_err());
        assert_eq!(channel_access::list_allowed(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn access_list_and_revoke_cover_both_sources() {
        let cfg = config(CONFIG);
        let pool = havn_db::connect_in_memory().await.expect("db");
        let created = create_with(&pool, "telegram", "bot", Duration::hours(1))
            .await
            .unwrap();
        channel_access::redeem(&pool, "telegram", "bot", "tg:1", &created.code, Utc::now())
            .await
            .unwrap()
            .expect("valid");
        let allowed = channel_access::list_allowed(&pool).await.unwrap();
        let lines = access_lines(&cfg, true, &allowed, None, None);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("tg:toml") && lines[0].contains("source=config"));
        assert!(lines[1].contains("tg:1") && lines[1].contains("source=invite"));
        assert!(!lines[1].contains("note="), "{}", lines[1]);
        assert!(access_lines(&cfg, true, &allowed, Some("slack"), None).is_empty());
        assert_eq!(
            access_lines(&cfg, true, &allowed, None, Some("bot")).len(),
            2
        );

        let err = revoke_sender_with(&pool, &cfg, "telegram", "bot", "tg:toml")
            .await
            .expect_err("config entry");
        assert!(err.to_string().contains("allow_from"), "{err}");
        let warning = revoke_sender_with(&pool, &cfg, "telegram", "bot", "tg:1")
            .await
            .expect("revoke");
        assert_eq!(warning, None);
        assert!(
            !channel_access::is_allowed(&pool, "telegram", "bot", "tg:1")
                .await
                .unwrap()
        );
        assert!(
            revoke_sender_with(&pool, &cfg, "telegram", "bot", "tg:1")
                .await
                .is_err()
        );
    }
}
