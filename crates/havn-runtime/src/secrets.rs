//! The agent's secrets (issue #233), as the gateway sends them in
//! `Welcome` and again whenever the owner changes them.
//!
//! - `secret:<name>` in an MCP server's `env` or in the shell environment
//!   resolves to the value when the process starts ([`resolve`],
//!   [`apply_shell_env`]).
//! - Every value is replaced with `[secret:<name>]` in tool output before
//!   the model sees it or it is kept ([`redact`]), and in this process's
//!   log ([`LogWriter`]). Only exact values are: one encoded or split by a
//!   tool goes through, and values shorter than
//!   [`secret_ref::MIN_REDACTED_CHARS`] are not replaced.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::{RwLock, RwLockReadGuard};

use havn_core::secret_ref::{self, SecretRef};
use havn_proto::Secrets;

struct Current {
    values: BTreeMap<String, String>,
    shell_env: BTreeMap<String, String>,
    /// From [`secret_ref::redaction_list`].
    redaction: Vec<(String, String)>,
}

static CURRENT: RwLock<Current> = RwLock::new(Current {
    values: BTreeMap::new(),
    shell_env: BTreeMap::new(),
    redaction: Vec::new(),
});

fn read() -> RwLockReadGuard<'static, Current> {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Use `secrets` from now on, in place of what came before.
pub fn set(secrets: Secrets) {
    let redaction = secret_ref::redaction_list(&secrets.values);
    let next = Current {
        values: secrets.values,
        shell_env: secrets.shell_env,
        redaction,
    };
    // Nothing logs while the lock is held: the log writer reads it.
    let previous = std::mem::replace(
        &mut *CURRENT
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        next,
    );
    drop(previous);
}

/// `text` with every secret's value replaced with `[secret:<name>]`.
pub fn redact(text: &str) -> String {
    secret_ref::redact(text, &read().redaction).into_owned()
}

/// `value`, or the secret it names when it is `secret:<name>`. Fails when
/// that secret is not set, or the reference names something else.
pub fn resolve(value: &str) -> Result<String, String> {
    resolve_in(&read().values, value)
}

fn resolve_in(values: &BTreeMap<String, String>, value: &str) -> Result<String, String> {
    if !value.starts_with(secret_ref::PREFIX) {
        return Ok(value.to_string());
    }
    match secret_ref::parse(value) {
        Ok(SecretRef::Agent { name }) => values.get(&name).cloned().ok_or_else(|| {
            format!("the secret `{name}` is not set (secret_request asks the owner for it)")
        }),
        Ok(SecretRef::Credential { .. }) => Err(format!(
            "{value:?} names a gateway credential; only the agent's own secrets \
             (`secret:<name>`) are available to its tools"
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// Give a shell command the agent's shell environment. A variable whose
/// secret is not set is left out.
pub fn apply_shell_env(command: &mut tokio::process::Command) {
    apply_in(&read(), command);
}

fn apply_in(current: &Current, command: &mut tokio::process::Command) {
    for (name, value) in &current.shell_env {
        match resolve_in(&current.values, value) {
            Ok(value) => {
                command.env(name, value);
            }
            Err(_) => {
                command.env_remove(name);
            }
        }
    }
}

/// The runtime's log output, each event with the secrets' values redacted
/// before it is written to stdout.
#[derive(Debug, Clone, Copy)]
pub struct LogWriter;

/// One event of [`LogWriter`], written when dropped.
#[derive(Debug)]
pub struct LogEvent(Vec<u8>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogEvent;

    fn make_writer(&'a self) -> LogEvent {
        LogEvent(Vec::new())
    }
}

impl std::io::Write for LogEvent {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogEvent {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.0);
        let _ = std::io::stdout().lock().write_all(redact(&text).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_resolve_to_the_agents_own_secrets_only() {
        let values = BTreeMap::from([("github".to_string(), "ghp_abcdefgh".to_string())]);
        assert_eq!(resolve_in(&values, "plain").as_deref(), Ok("plain"));
        assert_eq!(
            resolve_in(&values, "secret:github").as_deref(),
            Ok("ghp_abcdefgh")
        );
        assert!(
            resolve_in(&values, "secret:gitlab")
                .expect_err("not set")
                .contains("not set")
        );
        assert!(
            resolve_in(&values, "secret:channel:telegram:bot")
                .expect_err("a credential")
                .contains("credential")
        );
    }

    // The only test that sets the process's secrets.
    #[test]
    fn the_secrets_set_are_resolved_and_redacted_until_replaced() {
        set(Secrets {
            values: BTreeMap::from([("api".to_string(), "sk-live-123456".to_string())]),
            shell_env: BTreeMap::new(),
        });
        assert_eq!(redact("key=sk-live-123456"), "key=[secret:api]");
        assert_eq!(resolve("secret:api").as_deref(), Ok("sk-live-123456"));
        set(Secrets::default());
        assert_eq!(redact("key=sk-live-123456"), "key=sk-live-123456");
        assert!(resolve("secret:api").is_err());
    }

    #[test]
    fn shell_commands_get_the_variables_whose_secrets_are_set() {
        let mut command = tokio::process::Command::new("true");
        let values = BTreeMap::from([("github".to_string(), "ghp_abcdefgh".to_string())]);
        let shell_env = BTreeMap::from([
            ("GH_TOKEN".to_string(), "secret:github".to_string()),
            ("REGION".to_string(), "eu".to_string()),
            ("MISSING".to_string(), "secret:none".to_string()),
        ]);
        let current = Current {
            redaction: secret_ref::redaction_list(&values),
            values,
            shell_env,
        };
        apply_in(&current, &mut command);
        let envs: BTreeMap<String, Option<String>> = command
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs["GH_TOKEN"].as_deref(), Some("ghp_abcdefgh"));
        assert_eq!(envs["REGION"].as_deref(), Some("eu"));
        assert_eq!(envs["MISSING"], None);
        assert_eq!(
            secret_ref::redact("token ghp_abcdefgh", &current.redaction),
            "token [secret:github]"
        );
    }
}
