//! The agent's secrets (issue #233), as the gateway sends them in
//! `Welcome` and again whenever the owner changes them.
//!
//! - `secret:<name>` in an MCP server's `env` or in the shell environment
//!   resolves to the value when the process starts ([`resolve`],
//!   [`apply_shell_env`]).
//! - Every value is replaced with `[secret:<name>]` in tool output before
//!   the model sees it or it is kept ([`redact`]), and in this process's
//!   log ([`LogWriter`], [`Stream`]), as it is and escaped as log fields
//!   and JSON print it. A value replaced or removed stays redacted for the
//!   life of the process: a tool may have kept it. Output read in pieces
//!   never ends a piece with the start of a value ([`emit_len`],
//!   [`cut_end`], [`cut_start`]). Only exact values are redacted: one a tool
//!   encodes goes through, and values shorter than
//!   [`secret_ref::MIN_REDACTED_CHARS`] are not replaced.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::{RwLock, RwLockReadGuard};

use havn_core::secret_ref::{self, SecretRef};
use havn_proto::Secrets;

struct Current {
    values: BTreeMap<String, String>,
    shell_env: BTreeMap<String, String>,
    /// Every `(name, value)` this process was given, the current ones and
    /// those replaced or removed since.
    seen: Vec<(String, String)>,
    /// From [`secret_ref::redaction_list`] over `seen`.
    redaction: Vec<(String, String)>,
}

static CURRENT: RwLock<Current> = RwLock::new(Current {
    values: BTreeMap::new(),
    shell_env: BTreeMap::new(),
    seen: Vec::new(),
    redaction: Vec::new(),
});

fn read() -> RwLockReadGuard<'static, Current> {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Use `secrets` from now on, in place of what came before; the values
/// before stay redacted.
pub fn set(secrets: Secrets) {
    let mut seen = read().seen.clone();
    for (name, value) in &secrets.values {
        if !seen.iter().any(|(n, v)| n == name && v == value) {
            seen.push((name.clone(), value.clone()));
        }
    }
    let redaction = secret_ref::redaction_list(seen.iter().map(|(n, v)| (n, v)));
    let next = Current {
        values: secrets.values,
        shell_env: secrets.shell_env,
        seen,
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

/// The name of a secret whose marker `[secret:<name>]` is in `text`.
pub fn marker_in(text: &str) -> Option<String> {
    read()
        .seen
        .iter()
        .map(|(name, _)| name)
        .find(|name| text.contains(&secret_ref::marker(name)))
        .cloned()
}

/// How many of `bytes`, output still coming, can be shown now: not a
/// trailing start of a value (the rest may follow), not a value cut in two
/// by that, nor an incomplete UTF-8 character.
pub fn emit_len(bytes: &[u8]) -> usize {
    let current = read();
    let end = bytes.len() - secret_ref::partial_end(bytes, &current.redaction);
    let end = secret_ref::whole_end(bytes, end, &current.redaction);
    end - incomplete_char(&bytes[..end])
}

/// How many of `bytes`, output cut off after them, can be shown: not a
/// trailing start of a value whose rest was cut.
pub fn cut_end(bytes: &[u8]) -> usize {
    bytes.len() - secret_ref::partial_end(bytes, &read().redaction)
}

/// How many leading bytes of `bytes`, output whose start was cut off, to
/// drop: the end of a value whose start was cut.
pub fn cut_start(bytes: &[u8]) -> usize {
    secret_ref::partial_start(bytes, &read().redaction)
}

/// Bytes at the end of `bytes` that start a UTF-8 character not complete
/// in them.
fn incomplete_char(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let needs = match byte {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if needs > back { back } else { 0 };
    }
    0
}

/// Characters of a line of an MCP server's stderr kept in the log.
const MAX_LINE_CHARS: usize = 2000;
/// Bytes a [`Stream`] holds back at most, waiting for the rest of a value.
const MAX_HELD_BYTES: usize = 256 * 1024;

/// Output written to the log as it comes (an MCP server's stderr), with the
/// secrets' values redacted, also ones that span lines or arrive in
/// pieces, and lines cut at [`MAX_LINE_CHARS`].
#[derive(Debug, Default)]
pub struct Stream {
    pending: Vec<u8>,
    line_chars: usize,
}

impl Stream {
    /// What can be written now of what came before and `bytes`.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut end = emit_len(&self.pending);
        if self.pending.len() - end > MAX_HELD_BYTES {
            end = self.pending.len();
        }
        self.take(end)
    }

    /// The rest, once the output ended.
    pub fn finish(&mut self) -> String {
        self.take(self.pending.len())
    }

    fn take(&mut self, end: usize) -> String {
        let shown: Vec<u8> = self.pending.drain(..end).collect();
        let text = redact(&String::from_utf8_lossy(&shown));
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if c == '\n' {
                self.line_chars = 0;
                out.push(c);
                continue;
            }
            self.line_chars += 1;
            if self.line_chars <= MAX_LINE_CHARS {
                out.push(c);
            }
            if self.line_chars == MAX_LINE_CHARS {
                out.push_str(" [line cut]");
            }
        }
        out
    }
}

/// Write `text` to the runtime's stderr; a failed write is dropped.
pub fn write_stderr(text: &str) {
    if !text.is_empty() {
        let _ = std::io::stderr().lock().write_all(text.as_bytes());
    }
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
pub(crate) mod tests {
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

    /// Held by each test that sets the process's secrets.
    pub static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_secrets_set_are_resolved_and_stay_redacted_once_replaced() {
        let _one = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set(Secrets {
            values: BTreeMap::from([("api".to_string(), "sk-live-123456".to_string())]),
            shell_env: BTreeMap::new(),
        });
        assert_eq!(redact("key=sk-live-123456"), "key=[secret:api]");
        assert_eq!(resolve("secret:api").as_deref(), Ok("sk-live-123456"));
        set(Secrets {
            values: BTreeMap::from([
                ("api".to_string(), "sk-live-654321".to_string()),
                (
                    "pem".to_string(),
                    "-----BEGIN KEY-----\nAAAA\n-----END KEY-----".to_string(),
                ),
            ]),
            shell_env: BTreeMap::new(),
        });
        assert_eq!(resolve("secret:api").as_deref(), Ok("sk-live-654321"));
        assert_eq!(
            redact("old sk-live-123456 new sk-live-654321"),
            "old [secret:api] new [secret:api]"
        );
        set(Secrets::default());
        assert!(resolve("secret:api").is_err());
        assert_eq!(redact("key=sk-live-654321"), "key=[secret:api]");
        assert_eq!(marker_in("x [secret:api] y").as_deref(), Some("api"));
        assert_eq!(marker_in("x [secret:other] y"), None);

        // Pieces of output: the start of a value is held back until the
        // rest comes, and a value over several lines is redacted whole.
        let mut stream = Stream::default();
        let first = stream.push(b"log: -----BEGIN KEY-----\nAA");
        assert_eq!(first, "log: ");
        let rest = stream.push(b"AA\n-----END KEY----- done\n");
        assert_eq!(rest, "[secret:pem] done\n");
        assert_eq!(stream.push(b"sk-live"), "");
        assert_eq!(stream.finish(), "sk-live");
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let cut = Stream::default().push(format!("{long}\nnext\n").as_bytes());
        assert!(cut.contains("[line cut]") && cut.ends_with("\nnext\n"));

        assert_eq!(emit_len(b"done sk-live-6"), 5);
        assert_eq!(emit_len(b"done sk-live-654321"), 19);
        assert_eq!(emit_len("caf\u{e9}".as_bytes()), 5);
        assert_eq!(emit_len(&"caf\u{e9}".as_bytes()[..4]), 3);
        assert_eq!(cut_end(b"head sk-li"), 5);
        assert_eq!(cut_start(b"654321 tail"), 6);
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
            seen: Vec::new(),
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
