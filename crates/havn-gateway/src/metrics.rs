//! Prometheus metrics (issue #236): counters the gateway keeps in memory
//! since it started, and readings taken when they are asked for, served as
//! text on `metrics_listen`.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Instant;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use crate::AppState;
use crate::llm_proxy::{AnthropicResponse, ProxyError};

/// Models (per provider) or channels a counter keeps at most, so names
/// from outside (a model an agent asks for) cannot grow it without end.
/// Past it, a new one is counted under [`OTHER`].
const NAMES_MAX: usize = 1000;

/// What a name past [`NAMES_MAX`] is counted as: no model or channel is
/// called this.
const OTHER: &str = "__other__";

#[derive(Default)]
struct Counts {
    /// `(provider, model, outcome)`.
    llm_calls: BTreeMap<(String, String, &'static str), u64>,
    /// `(provider, model)`: input and output tokens.
    llm_tokens: BTreeMap<(String, String), (u64, u64)>,
    /// `(channel, direction)`.
    channel_messages: BTreeMap<(String, &'static str), u64>,
    /// By outcome.
    cron_runs: BTreeMap<&'static str, u64>,
    /// Per `channel|account`, the last reply counted as sent: a reply
    /// written again (replayed to an adapter that came back) is not
    /// counted again.
    sent_up_to: std::collections::HashMap<String, u64>,
}

static COUNTS: LazyLock<Mutex<Counts>> = LazyLock::new(Default::default);
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

fn counts() -> std::sync::MutexGuard<'static, Counts> {
    COUNTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `name` when it is one of `known` names or there is room for another,
/// else [`OTHER`].
fn kept<'a>(name: &str, mut known: impl Iterator<Item = &'a String>, len: usize) -> String {
    if len < NAMES_MAX || known.any(|k| k == name) {
        name.to_string()
    } else {
        OTHER.to_string()
    }
}

/// Start the uptime clock.
pub fn start() {
    LazyLock::force(&STARTED);
}

/// What one LLM call came to: an outcome and, when it answered, its tokens.
pub fn llm_call(provider: &str, model: &str, result: &Result<AnthropicResponse, ProxyError>) {
    let outcome = match result {
        Ok(_) => "ok",
        Err(ProxyError::NoCredential(_)) => "no_credential",
        Err(ProxyError::Unavailable { .. }) => "unavailable",
        Err(ProxyError::Upstream { .. }) => "upstream_error",
        Err(ProxyError::Transport(_)) => "transport_error",
        Err(ProxyError::BudgetExhausted(_)) => "budget_exhausted",
        Err(_) => "other_error",
    };
    let mut counts = counts();
    let models: std::collections::BTreeSet<&String> = counts
        .llm_calls
        .keys()
        .filter(|(p, _, _)| p == provider)
        .map(|(_, m, _)| m)
        .collect();
    let len = models.len();
    let model = kept(model, models.into_iter(), len);
    *counts
        .llm_calls
        .entry((provider.to_string(), model.clone(), outcome))
        .or_default() += 1;
    if let Ok(resp) = result {
        let tokens = counts
            .llm_tokens
            .entry((provider.to_string(), model))
            .or_default();
        tokens.0 = tokens.0.saturating_add(resp.usage.total_input_tokens());
        tokens.1 = tokens.1.saturating_add(resp.usage.output_tokens);
    }
}

/// A message of `channel` came in (`in`) or went out (`out`).
pub fn channel_message(channel: &str, direction: &'static str) {
    let mut counts = counts();
    let channels: std::collections::BTreeSet<&String> =
        counts.channel_messages.keys().map(|(c, _)| c).collect();
    let len = channels.len();
    let channel = kept(channel, channels.into_iter(), len);
    *counts
        .channel_messages
        .entry((channel, direction))
        .or_default() += 1;
}

/// Reply `seq` of `channel`'s account `account_id` was written to its
/// adapter: counted as `out` unless it was counted already.
pub fn channel_sent(channel: &str, account_id: &str, seq: u64) {
    {
        let mut counts = counts();
        let last = counts
            .sent_up_to
            .entry(format!("{channel}|{account_id}"))
            .or_default();
        if seq <= *last {
            return;
        }
        *last = seq;
    }
    channel_message(channel, "out");
}

/// A scheduled task ran: `success`, `failure` or `skipped` (its wake check
/// said not to).
pub fn cron_run(outcome: &'static str) {
    *counts().cron_runs.entry(outcome).or_default() += 1;
}

/// A label value as the text format writes it.
fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// The metrics as Prometheus's text format, with `agents` agents and
/// `adapters` channel adapters connected now.
#[must_use]
pub fn render(agents: usize, adapters: usize) -> String {
    let gauge = |name: &str, help: &str, sample: String| {
        (
            format!(
                "# HELP {name} {help}
# TYPE {name} gauge
"
            ),
            format!(
                "{name}{sample}
"
            ),
        )
    };
    let mut sections = vec![
        gauge(
            "havn_build_info",
            "The gateway's version.",
            format!("{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION")),
        ),
        gauge(
            "havn_uptime_seconds",
            "Seconds since the gateway started.",
            format!(" {}", STARTED.elapsed().as_secs()),
        ),
        gauge(
            "havn_agents_connected",
            "Agents connected to the gateway now.",
            format!(" {agents}"),
        ),
        gauge(
            "havn_channel_adapters_connected",
            "Channel adapters connected to the gateway now.",
            format!(" {adapters}"),
        ),
    ];

    let counts = counts();
    let mut section = |name: &str, kind: &str, help: &str, lines: Vec<String>| {
        let head = format!("# HELP {name} {help}\n# TYPE {name} {kind}\n");
        let body: String = lines
            .into_iter()
            .map(|line| format!("{name}{line}\n"))
            .collect();
        sections.push((head, body));
    };
    section(
        "havn_llm_calls_total",
        "counter",
        "LLM calls the gateway made for agents, by provider, model and outcome.",
        counts
            .llm_calls
            .iter()
            .map(|((p, m, o), n)| {
                format!(
                    "{{provider=\"{}\",model=\"{}\",outcome=\"{o}\"}} {n}",
                    label(p),
                    label(m)
                )
            })
            .collect(),
    );
    section(
        "havn_llm_tokens_total",
        "counter",
        "Tokens of the LLM calls that answered, by provider, model and direction.",
        counts
            .llm_tokens
            .iter()
            .flat_map(|((p, m), (input, output))| {
                let (p, m) = (label(p), label(m));
                [
                    format!("{{provider=\"{p}\",model=\"{m}\",direction=\"input\"}} {input}"),
                    format!("{{provider=\"{p}\",model=\"{m}\",direction=\"output\"}} {output}"),
                ]
            })
            .collect(),
    );
    section(
        "havn_channel_messages_total",
        "counter",
        "Messages through channel adapters, by channel and direction.",
        counts
            .channel_messages
            .iter()
            .map(|((c, d), n)| format!("{{channel=\"{}\",direction=\"{d}\"}} {n}", label(c)))
            .collect(),
    );
    section(
        "havn_cron_runs_total",
        "counter",
        "Runs of scheduled tasks, by outcome.",
        counts
            .cron_runs
            .iter()
            .map(|(o, n)| format!("{{outcome=\"{o}\"}} {n}"))
            .collect(),
    );
    sections
        .into_iter()
        .map(|(head, body)| head + &body)
        .collect()
}

/// `GET /metrics` on `metrics_listen`.
pub async fn handler(State(state): State<AppState>) -> impl IntoResponse {
    let agents = state
        .registry
        .list()
        .await
        .iter()
        .filter(|a| a.connected)
        .count();
    let adapters = state.channel_router.connected().await;
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        render(agents, adapters),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_counters_are_written_in_the_text_format() {
        channel_message("tg\"x", "in");
        cron_run("success");
        let text = render(2, 1);
        assert!(
            text.contains("# TYPE havn_agents_connected gauge\nhavn_agents_connected 2\n"),
            "{text}"
        );
        assert!(
            text.contains("havn_channel_adapters_connected 1\n"),
            "{text}"
        );
        assert!(
            text.contains("havn_channel_messages_total{channel=\"tg\\\"x\",direction=\"in\"} "),
            "{text}"
        );
        assert!(
            text.contains("havn_cron_runs_total{outcome=\"success\"} "),
            "{text}"
        );
        assert!(text.contains(&format!(
            "havn_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
        // Every sample follows its metric's TYPE line.
        let mut typed = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                typed.insert(rest.split(' ').next().unwrap().to_string());
            } else if !line.starts_with('#') {
                let name = line.split(['{', ' ']).next().unwrap();
                assert!(typed.contains(name), "{line}");
            }
        }
    }

    #[tokio::test]
    async fn the_endpoint_serves_the_text_format() {
        let (state, _) = crate::auth::tests::state_with(false).await;
        let response = handler(State(state)).await.into_response();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf-8");
        assert!(
            text.contains(
                "havn_agents_connected 0
"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "havn_channel_adapters_connected 0
"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_counter_takes_so_many_names_and_counts_the_rest_as_other() {
        let known = ["a".to_string()];
        assert_eq!(kept("m", known.iter(), NAMES_MAX - 1), "m");
        assert_eq!(kept("a", known.iter(), NAMES_MAX), "a");
        assert_eq!(kept("m", known.iter(), NAMES_MAX), OTHER);
    }

    #[test]
    fn an_llm_call_is_counted_with_its_outcome_and_tokens_and_a_model_past_the_cap_as_other() {
        let provider = "metrics-test";
        let answered: Result<AnthropicResponse, ProxyError> =
            Ok(serde_json::from_value(serde_json::json!({
                "id": "m", "type": "message", "role": "assistant", "model": "x",
                "content": [], "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 4}
            }))
            .expect("response"));
        llm_call(provider, "model-0", &answered);
        llm_call(
            provider,
            "model-0",
            &Err(ProxyError::NoCredential(provider.into())),
        );
        for n in 1..NAMES_MAX {
            llm_call(
                provider,
                &format!("model-{n}"),
                &Err(ProxyError::NoCredential(provider.into())),
            );
        }
        // A known model takes a new outcome; a new one goes to other.
        llm_call(provider, "model-1", &answered);
        llm_call(provider, "one-too-many", &answered);
        let text = render(0, 0);
        assert!(
            text.contains(&format!(
                "havn_llm_calls_total{{provider=\"{provider}\",model=\"model-0\",outcome=\"ok\"}} 1"
            )),
            "{text}"
        );
        assert!(text.contains(&format!("havn_llm_calls_total{{provider=\"{provider}\",model=\"model-0\",outcome=\"no_credential\"}} 1")));
        assert!(text.contains(&format!("havn_llm_tokens_total{{provider=\"{provider}\",model=\"model-0\",direction=\"input\"}} 3")));
        assert!(text.contains(&format!("havn_llm_tokens_total{{provider=\"{provider}\",model=\"model-0\",direction=\"output\"}} 4")));
        assert!(text.contains(&format!(
            "havn_llm_calls_total{{provider=\"{provider}\",model=\"model-1\",outcome=\"ok\"}} 1"
        )));
        assert!(text.contains(&format!(
            "havn_llm_calls_total{{provider=\"{provider}\",model=\"{OTHER}\",outcome=\"ok\"}} 1"
        )));
        assert!(!text.contains("one-too-many"));
    }

    #[test]
    fn a_reply_written_again_is_counted_once() {
        let out = |text: &str| {
            text.lines()
                .find(|l| {
                    l.starts_with(
                        "havn_channel_messages_total{channel=\"metrics-sent\",direction=\"out\"}",
                    )
                })
                .map(|l| l.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
        };
        channel_sent("metrics-sent", "bot", 5);
        channel_sent("metrics-sent", "bot", 6);
        channel_sent("metrics-sent", "bot", 5);
        channel_sent("metrics-sent", "bot", 6);
        assert_eq!(out(&render(0, 0)), Some(2));
    }
}
