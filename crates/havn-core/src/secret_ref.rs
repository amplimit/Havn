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

/// Secrets `(name, value)` as [`redact`] takes them: `(form, name)` for
/// each form a value is printed in (as it is, trimmed; escaped as Rust's
/// `Debug` and as JSON escape it, which log fields and JSON output use),
/// without the values too short to redact, longest first, then by name.
#[must_use]
pub fn redaction_list<'a>(
    secrets: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = Vec::new();
    for (name, value) in secrets {
        let value = value.trim();
        if value.chars().count() < MIN_REDACTED_CHARS {
            continue;
        }
        let debug = format!("{value:?}");
        let json = serde_json::to_string(value).unwrap_or_default();
        for form in [
            value,
            debug.get(1..debug.len() - 1).unwrap_or_default(),
            json.get(1..json.len().saturating_sub(1))
                .unwrap_or_default(),
        ] {
            if !form.is_empty() && !list.iter().any(|(f, n)| f == form && n == name) {
                list.push((form.to_string(), name.clone()));
            }
        }
    }
    list.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(&b.1)));
    list
}

/// `text` with every occurrence of a form in `list` (from
/// [`redaction_list`]) replaced with [`marker`] of its name, in one pass:
/// occurrences that overlap are replaced together, so no part of either
/// stays, with the marker of each that reaches past the ones before it (one
/// inside another adds none).
#[must_use]
pub fn redact<'a>(text: &'a str, list: &[(String, String)]) -> Cow<'a, str> {
    let mut found: Vec<(usize, usize, &str)> = Vec::new();
    for (form, name) in list {
        let mut from = 0;
        while let Some(at) = text.get(from..).and_then(|rest| rest.find(form.as_str())) {
            let start = from + at;
            found.push((start, start + form.len(), name));
            from = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if found.is_empty() {
        return Cow::Borrowed(text);
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    let mut at = 0;
    while at < found.len() {
        let (start, mut end, name) = found[at];
        let mut names = vec![name];
        at += 1;
        while at < found.len() && found[at].0 < end {
            // One inside what is replaced already adds no marker; one that
            // reaches past it does.
            if found[at].1 > end {
                end = found[at].1;
                if !names.contains(&found[at].2) {
                    names.push(found[at].2);
                }
            }
            at += 1;
        }
        out.push_str(&text[done..start]);
        for name in names {
            out.push_str(&marker(name));
        }
        done = end;
    }
    out.push_str(&text[done..]);
    Cow::Owned(out)
}

/// How many bytes at the end of `bytes` could be the start of a form in
/// `list`: the longest suffix that is a proper prefix of one. Output cut
/// or read in pieces holds them back, or drops them at a cut, so a piece
/// never shows the start of a value whose rest is elsewhere.
#[must_use]
pub fn partial_end(bytes: &[u8], list: &[(String, String)]) -> usize {
    let mut longest = 0;
    for (form, _) in list {
        let form = form.as_bytes();
        for k in (longest + 1..form.len().min(bytes.len() + 1)).rev() {
            if bytes.ends_with(&form[..k]) {
                longest = k;
                break;
            }
        }
    }
    longest
}

/// How many bytes at the start of `bytes` could be the end of a form in
/// `list` whose start was cut off: the longest prefix that is a proper
/// suffix of one.
#[must_use]
pub fn partial_start(bytes: &[u8], list: &[(String, String)]) -> usize {
    let mut longest = 0;
    for (form, _) in list {
        let form = form.as_bytes();
        for k in (longest + 1..form.len().min(bytes.len() + 1)).rev() {
            if bytes.starts_with(&form[form.len() - k..]) {
                longest = k;
                break;
            }
        }
    }
    longest
}

/// `end`, or further when a form in `list` starts before it and ends past
/// it (within `bytes`): where `bytes` can be split without splitting one.
#[must_use]
pub fn whole_end(bytes: &[u8], mut end: usize, list: &[(String, String)]) -> usize {
    loop {
        let mut moved = end;
        for (form, _) in list {
            let form = form.as_bytes();
            let first = end.saturating_sub(form.len() - 1);
            for start in first..end {
                let stop = start + form.len();
                if stop > end && stop <= bytes.len() && &bytes[start..stop] == form {
                    moved = moved.max(stop);
                }
            }
        }
        if moved == end {
            return end;
        }
        end = moved;
    }
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

    #[test]
    fn overlapping_values_leave_nothing_of_either() {
        let secrets = BTreeMap::from([
            ("a".to_string(), "AAAAAAAABBBB".to_string()),
            ("b".to_string(), "BBBBCCCCCCCC".to_string()),
        ]);
        let list = redaction_list(&secrets);
        assert_eq!(
            redact("x AAAAAAAABBBBCCCCCCCC y", &list),
            "x [secret:a][secret:b] y"
        );
        assert_eq!(
            redact("AAAAAAAABBBB BBBBCCCCCCCC", &list),
            "[secret:a] [secret:b]"
        );
    }

    #[test]
    fn escaped_forms_of_a_value_are_redacted() {
        let secrets = BTreeMap::from([(
            "pem".to_string(),
            "-----BEGIN KEY-----\nab\"c\\d\n-----END KEY-----".to_string(),
        )]);
        let list = redaction_list(&secrets);
        let value = &secrets["pem"];
        let debug = format!("field={value:?}");
        let json = serde_json::json!({ "v": value }).to_string();
        for text in [value.clone(), debug, json] {
            let out = redact(&text, &list);
            assert!(out.contains("[secret:pem]"), "{out}");
            assert!(!out.contains("BEGIN KEY"), "{out}");
        }
    }

    #[test]
    fn pieces_never_split_a_value() {
        let secrets = BTreeMap::from([("t".to_string(), "tok-0123456789".to_string())]);
        let list = redaction_list(&secrets);
        assert_eq!(partial_end(b"output tok-012", &list), 7);
        assert_eq!(partial_end(b"done.", &list), 0);
        assert_eq!(partial_end(b"output", &list), 1, "a `t` may start it");
        assert_eq!(
            partial_end(b"tok-0123456789", &list),
            0,
            "a whole value is not held"
        );
        assert_eq!(partial_start(b"3456789 more", &list), 7);
        assert_eq!(partial_start(b"more", &list), 0);
        let bytes = b"a tok-0123456789 b";
        assert_eq!(whole_end(bytes, 5, &list), 16);
        assert_eq!(whole_end(bytes, 1, &list), 1);
    }
}
