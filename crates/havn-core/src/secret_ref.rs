//! Parser for `secret:` references (spec §7.3, issue #233), and the
//! redaction of secret values.
//!
//! Two forms:
//!
//! - `secret:<name>` names one of an agent's own secrets, which its
//!   owner sets on the dashboard. MCP server `env` and the agent's shell
//!   environment use it; the runtime resolves it when it starts the
//!   process.
//! - `secret:<provider>:<name>` names a row of the gateway's credential
//!   table, such as a channel adapter's token
//!   (`adapter_token_ref = "secret:channel:telegram:alice-tg-bot"`).
//!   Spec §7.3 specifies the split semantics:
//!
//! > Reference parsing splits the string on the FIRST colon (strip
//! > `secret:` prefix) and the LAST colon (separate provider from name);
//! > intermediate colons stay with the provider. So
//! > `secret:channel:telegram:alice-tg-bot` resolves to
//! > `(provider = "channel:telegram", name = "alice-tg-bot")`
//! > unambiguously.
//!
//! This is the parser. Pure function, no I/O. Lookup is the caller's job.

use std::borrow::Cow;

/// What every reference starts with.
pub const PREFIX: &str = "secret:";

/// Longest name of an agent's secret.
pub const MAX_NAME_CHARS: usize = 64;

/// Values shorter than this (after trimming whitespace) are not redacted:
/// replacing every `1234` or `true` in what a tool prints would shred its
/// output more than it hides.
pub const MIN_REDACTED_CHARS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// `secret:<name>`: one of the agent's own secrets.
    Agent { name: String },
    /// `secret:<provider>:<name>`: a credential row by handle.
    Credential { provider: String, name: String },
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretRefError {
    #[error("secret reference must start with `secret:` prefix; got {0:?}")]
    MissingPrefix(String),
    #[error("secret reference provider must not be empty; got {0:?}")]
    EmptyProvider(String),
    #[error("secret reference name must not be empty; got {0:?}")]
    EmptyName(String),
    #[error("{1}; got {0:?}")]
    InvalidName(String, &'static str),
}

/// Parse a reference. Without a further colon after `secret:` it names an
/// agent secret, whose name must pass [`check_name`]; otherwise the LAST
/// colon separates a credential's provider (which may itself contain
/// colons) from its name.
///
/// Examples:
/// - `secret:github` → agent secret `github`
/// - `secret:llm:anthropic:default` → (`llm:anthropic`, `default`)
/// - `secret:channel:telegram:alice-tg-bot` → (`channel:telegram`, `alice-tg-bot`)
pub fn parse(s: &str) -> Result<SecretRef, SecretRefError> {
    let body = s
        .strip_prefix(PREFIX)
        .ok_or_else(|| SecretRefError::MissingPrefix(s.to_string()))?;
    let Some((provider, name)) = body.rsplit_once(':') else {
        if body.is_empty() {
            return Err(SecretRefError::EmptyName(s.to_string()));
        }
        check_name(body).map_err(|why| SecretRefError::InvalidName(s.to_string(), why))?;
        return Ok(SecretRef::Agent {
            name: body.to_string(),
        });
    };
    if provider.is_empty() {
        return Err(SecretRefError::EmptyProvider(s.to_string()));
    }
    if name.is_empty() {
        return Err(SecretRefError::EmptyName(s.to_string()));
    }
    Ok(SecretRef::Credential {
        provider: provider.to_string(),
        name: name.to_string(),
    })
}

/// The name of an agent secret: 1 to 64 letters, digits, `_`, `-` or `.`,
/// starting with a letter or digit.
pub fn check_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("a secret's name must not be empty");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("a secret's name is at most 64 characters");
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("a secret's name starts with a letter or a digit");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("a secret's name has only letters, digits, `_`, `-` and `.`");
    }
    Ok(())
}

/// What a redacted value is replaced with.
#[must_use]
pub fn marker(name: &str) -> String {
    format!("[{PREFIX}{name}]")
}

/// Secrets as [`redact`] takes them: `(value, name)`, trimmed, without the
/// values too short to redact, longest first (so a value that contains
/// another is replaced whole), then by name.
#[must_use]
pub fn redaction_list<'a>(
    secrets: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = secrets
        .into_iter()
        .map(|(name, value)| (value.trim().to_string(), name.clone()))
        .filter(|(value, _)| value.chars().count() >= MIN_REDACTED_CHARS)
        .collect();
    list.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(&b.1)));
    list
}

/// `text` with every exact occurrence of a value in `list` (from
/// [`redaction_list`]) replaced with [`marker`] of its name.
#[must_use]
pub fn redact<'a>(text: &'a str, list: &[(String, String)]) -> Cow<'a, str> {
    let mut out = Cow::Borrowed(text);
    for (value, name) in list {
        if out.contains(value.as_str()) {
            out = Cow::Owned(out.replace(value.as_str(), &marker(name)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn credential(s: &str) -> (String, String) {
        match parse(s).expect("ok") {
            SecretRef::Credential { provider, name } => (provider, name),
            other => panic!("expected a credential handle, got {other:?}"),
        }
    }

    #[test]
    fn parses_namespaced_channel_provider() {
        let (provider, name) = credential("secret:channel:telegram:alice-tg-bot");
        assert_eq!(provider, "channel:telegram");
        assert_eq!(name, "alice-tg-bot");
    }

    #[test]
    fn parses_namespaced_saas_provider() {
        let (provider, name) = credential("secret:saas:microsoft-graph:alice-m365");
        assert_eq!(provider, "saas:microsoft-graph");
        assert_eq!(name, "alice-m365");
    }

    #[test]
    fn parses_namespaced_llm_provider() {
        let (provider, name) = credential("secret:llm:anthropic:prod");
        assert_eq!(provider, "llm:anthropic");
        assert_eq!(name, "prod");
    }

    #[test]
    fn one_segment_names_an_agent_secret() {
        assert_eq!(
            parse("secret:github").expect("ok"),
            SecretRef::Agent {
                name: "github".into()
            }
        );
        assert!(matches!(
            parse("secret:-bad"),
            Err(SecretRefError::InvalidName(..))
        ));
        assert!(matches!(
            parse("secret:has space"),
            Err(SecretRefError::InvalidName(..))
        ));
    }

    #[test]
    fn rejects_missing_secret_prefix() {
        assert!(matches!(
            parse("channel:telegram:alice"),
            Err(SecretRefError::MissingPrefix(_))
        ));
    }

    #[test]
    fn rejects_empty_provider() {
        // body = ":name", rsplit on ':' gives ("", "name").
        assert!(matches!(
            parse("secret::name"),
            Err(SecretRefError::EmptyProvider(_))
        ));
    }

    #[test]
    fn rejects_empty_name() {
        // body = "channel:telegram:", rsplit gives ("channel:telegram", "").
        assert!(matches!(
            parse("secret:channel:telegram:"),
            Err(SecretRefError::EmptyName(_))
        ));
    }

    #[test]
    fn rejects_just_prefix() {
        assert!(matches!(
            parse("secret:"),
            Err(SecretRefError::EmptyName(_))
        ));
    }

    #[test]
    fn name_can_contain_dashes_and_dots() {
        let (_, name) = credential("secret:channel:telegram:alice.bob-test_42");
        assert_eq!(name, "alice.bob-test_42");
        assert!(check_name("gh.token-2_x").is_ok());
        assert!(check_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn exact_values_are_redacted_longest_first() {
        let secrets = BTreeMap::from([
            ("short".to_string(), "abc".to_string()),
            ("token".to_string(), "ghp_0123456789".to_string()),
            ("long".to_string(), "xx-ghp_0123456789-yy\n".to_string()),
        ]);
        let list = redaction_list(&secrets);
        assert_eq!(list.len(), 2, "a value under the minimum is left out");
        let out = redact("a xx-ghp_0123456789-yy b ghp_0123456789 abc", &list);
        assert_eq!(out, "a [secret:long] b [secret:token] abc");
        assert!(matches!(redact("nothing here", &list), Cow::Borrowed(_)));
    }
}
