//! Parser for the operator-facing `secret:<provider>:<name>` credential
//! reference syntax (spec §7.3).
//!
//! Config blocks like `[[channels.telegram.accounts]] adapter_token_ref =
//! "secret:channel:telegram:alice-tg-bot"` resolve at gateway startup or
//! at WS-upgrade time to a credentials-table row by `(provider, name)`.
//! Spec §7.3 specifies the split semantics:
//!
//! > Reference parsing splits the string on the FIRST colon (strip
//! > `secret:` prefix) and the LAST colon (separate provider from name);
//! > intermediate colons stay with the provider. So
//! > `secret:channel:telegram:alice-tg-bot` resolves to
//! > `(provider = "channel:telegram", name = "alice-tg-bot")`
//! > unambiguously.
//!
//! This is the parser. Pure function, no I/O. Lookup against the
//! `credentials` table is the caller's job.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub provider: String,
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretRefError {
    #[error("secret reference must start with `secret:` prefix; got {0:?}")]
    MissingPrefix(String),
    #[error("secret reference must contain at least one provider segment and a name; got {0:?}")]
    MissingFields(String),
    #[error("secret reference provider must not be empty; got {0:?}")]
    EmptyProvider(String),
    #[error("secret reference name must not be empty; got {0:?}")]
    EmptyName(String),
}

/// Parse `secret:<provider>:<name>` per spec §7.3. Provider may itself
/// contain colons (for the `channel:telegram` / `saas:microsoft-graph`
/// namespaced forms); the LAST colon separates provider from name.
///
/// Examples:
/// - `secret:llm:anthropic:default` → (`llm:anthropic`, `default`)
/// - `secret:channel:telegram:alice-tg-bot` → (`channel:telegram`, `alice-tg-bot`)
/// - `secret:saas:microsoft-graph:alice-m365` → (`saas:microsoft-graph`, `alice-m365`)
pub fn parse(s: &str) -> Result<SecretRef, SecretRefError> {
    let body = s
        .strip_prefix("secret:")
        .ok_or_else(|| SecretRefError::MissingPrefix(s.to_string()))?;
    let (provider, name) = body
        .rsplit_once(':')
        .ok_or_else(|| SecretRefError::MissingFields(s.to_string()))?;
    if provider.is_empty() {
        return Err(SecretRefError::EmptyProvider(s.to_string()));
    }
    if name.is_empty() {
        return Err(SecretRefError::EmptyName(s.to_string()));
    }
    Ok(SecretRef {
        provider: provider.to_string(),
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaced_channel_provider() {
        let r = parse("secret:channel:telegram:alice-tg-bot").expect("ok");
        assert_eq!(r.provider, "channel:telegram");
        assert_eq!(r.name, "alice-tg-bot");
    }

    #[test]
    fn parses_namespaced_saas_provider() {
        let r = parse("secret:saas:microsoft-graph:alice-m365").expect("ok");
        assert_eq!(r.provider, "saas:microsoft-graph");
        assert_eq!(r.name, "alice-m365");
    }

    #[test]
    fn parses_namespaced_llm_provider() {
        let r = parse("secret:llm:anthropic:prod").expect("ok");
        assert_eq!(r.provider, "llm:anthropic");
        assert_eq!(r.name, "prod");
    }

    #[test]
    fn rejects_missing_secret_prefix() {
        assert!(matches!(
            parse("channel:telegram:alice"),
            Err(SecretRefError::MissingPrefix(_))
        ));
    }

    #[test]
    fn rejects_no_separator_after_prefix() {
        // "secret:onlyone" has no `:` after the prefix-strip body.
        assert!(matches!(
            parse("secret:onlyone"),
            Err(SecretRefError::MissingFields(_))
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
        assert!(parse("secret:").is_err());
        assert!(parse("secret:").is_err());
    }

    #[test]
    fn name_can_contain_dashes_and_dots() {
        let r = parse("secret:channel:telegram:alice.bob-test_42").expect("ok");
        assert_eq!(r.name, "alice.bob-test_42");
    }
}
