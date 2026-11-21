//! Webhooks (issue #236): an address outside systems call to have an agent
//! act on what they send, as for someone other than its owner.
//!
//! The owner makes, changes and deletes them on the agent's settings page
//! (`/agents/{id}/webhooks`); a caller posts to `/hooks/{id}` with the
//! webhook's secret, as a bearer token or `X-Havn-Token`, or by signing
//! the body with it (`X-Hub-Signature-256: sha256=<hex>`, GitHub's form).

use std::collections::{HashMap, VecDeque};
use std::str::FromStr as _;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use havn_db::repo::webhooks::{self, NewWebhook, Webhook};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tracing::warn;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;

/// Bytes of a call's body the gateway takes.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// Characters of a call's body the agent is given.
const MAX_BODY_CHARS: usize = 32_000;

/// Calls one webhook takes a minute; more are answered 429.
const CALLS_PER_MINUTE: usize = 30;

/// Characters of a webhook's name and of its instructions.
const MAX_NAME_CHARS: usize = 80;
const MAX_INSTRUCTIONS_CHARS: usize = 4000;

#[derive(Debug, Serialize)]
pub struct WebhookView {
    pub id: String,
    pub name: String,
    /// Where it is called: under the gateway's `listen` address.
    pub path: String,
    pub instructions: String,
    pub deliver: String,
    pub created_at: DateTime<Utc>,
    pub last_called_at: Option<DateTime<Utc>>,
    pub calls: i64,
    /// Given only when it is made: it is not shown again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl WebhookView {
    fn of(hook: Webhook, secret: bool) -> Self {
        Self {
            id: hook.id.to_string(),
            path: format!("/hooks/{}", hook.id),
            name: hook.name,
            instructions: hook.instructions,
            deliver: hook.deliver,
            created_at: hook.created_at,
            last_called_at: hook.last_called_at,
            calls: hook.calls,
            secret: secret.then_some(hook.secret),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookCreate {
    pub name: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub deliver: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookChange {
    pub instructions: String,
    pub deliver: String,
}

/// Where a webhook's results go: `web`, `last`, or the name of a channel
/// one of the agent's bots is on.
fn deliver_of(
    state: &AppState,
    agent: &havn_db::repo::agents::Agent,
    deliver: &str,
) -> Result<String, ApiError> {
    let deliver = deliver.trim();
    let agent_id = agent.id.to_string();
    let on_channel = state
        .bindings
        .iter()
        .any(|b| b.agent_id == agent_id && b.channel == deliver);
    if deliver == crate::heartbeat::DELIVER_WEB
        || deliver == crate::heartbeat::DELIVER_LAST
        || on_channel
    {
        Ok(deliver.to_string())
    } else {
        Err(ApiError::BadRequest(
            "deliver is web, last, or the channel of one of the agent's bots".into(),
        ))
    }
}

fn instructions_of(text: &str) -> Result<String, ApiError> {
    if text.chars().count() > MAX_INSTRUCTIONS_CHARS {
        return Err(ApiError::BadRequest(format!(
            "instructions are at most {MAX_INSTRUCTIONS_CHARS} characters"
        )));
    }
    Ok(text.trim().to_string())
}

fn webhook_id(id: &str) -> Result<Uuid, ApiError> {
    Uuid::from_str(id).map_err(|_| ApiError::NotFound)
}

/// The webhook `hook` of `agent`, or `NotFound`.
async fn agents_webhook(
    state: &AppState,
    agent: &havn_db::repo::agents::Agent,
    hook: &str,
) -> Result<Webhook, ApiError> {
    let found = webhooks::find(&state.db, webhook_id(hook)?)
        .await?
        .ok_or(ApiError::NotFound)?;
    if found.agent_id != agent.id {
        return Err(ApiError::NotFound);
    }
    Ok(found)
}

/// `GET /agents/{id}/webhooks`.
pub async fn list(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<WebhookView>>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let hooks = webhooks::list_for_agent(&state.db, agent.id).await?;
    Ok(Json(
        hooks
            .into_iter()
            .map(|h| WebhookView::of(h, false))
            .collect(),
    ))
}

/// `POST /agents/{id}/webhooks`: a new webhook, with its secret, shown
/// this once.
pub async fn create(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<WebhookCreate>,
) -> Result<(StatusCode, Json<WebhookView>), ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let name = req.name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::BadRequest(format!(
            "a name of 1 to {MAX_NAME_CHARS} characters, on one line"
        )));
    }
    let instructions = instructions_of(&req.instructions)?;
    let deliver = deliver_of(
        &state,
        &agent,
        req.deliver
            .as_deref()
            .unwrap_or(crate::heartbeat::DELIVER_WEB),
    )?;
    let hook = webhooks::create(
        &state.db,
        NewWebhook {
            agent_id: agent.id,
            name,
            instructions: &instructions,
            deliver: &deliver,
        },
    )
    .await?;
    crate::audit::record(
        &state.db,
        user.id,
        agent.team_id,
        Some(agent.id),
        "webhook.created",
        serde_json::json!({ "webhook_id": hook.id.to_string(), "name": hook.name }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(WebhookView::of(hook, true))))
}

/// `PATCH /agents/{id}/webhooks/{hook}`: what it does with a call and
/// where its results go.
pub async fn update(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, hook)): Path<(String, String)>,
    Json(req): Json<WebhookChange>,
) -> Result<Json<WebhookView>, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let found = agents_webhook(&state, &agent, &hook).await?;
    let instructions = instructions_of(&req.instructions)?;
    let deliver = deliver_of(&state, &agent, &req.deliver)?;
    webhooks::update(&state.db, found.id, &instructions, &deliver).await?;
    crate::audit::record(
        &state.db,
        user.id,
        agent.team_id,
        Some(agent.id),
        "webhook.updated",
        serde_json::json!({
            "webhook_id": found.id.to_string(),
            "deliver": deliver,
            "instructions_changed": instructions != found.instructions,
        }),
    )
    .await;
    let updated = webhooks::find(&state.db, found.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(WebhookView::of(updated, false)))
}

/// `DELETE /agents/{id}/webhooks/{hook}`: calls to it are refused from
/// now on.
pub async fn delete(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((id, hook)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let agent = crate::api::agents::resolve_agent(&id, &user, &state).await?;
    let found = agents_webhook(&state, &agent, &hook).await?;
    webhooks::delete(&state.db, found.id).await?;
    crate::audit::record(
        &state.db,
        user.id,
        agent.team_id,
        Some(agent.id),
        "webhook.deleted",
        serde_json::json!({ "webhook_id": found.id.to_string(), "name": found.name }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Whether `a` and `b` are equal, in time that does not depend on where
/// they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA256 of `body` under `key`.
fn hmac_sha256(key: &[u8], body: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(body)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a call with `headers` and `body` holds `secret`: as a bearer
/// token, as `X-Havn-Token`, or as the key of the body's HMAC-SHA256 in
/// `X-Hub-Signature-256` (or `X-Havn-Signature-256`), `sha256=<hex>`.
fn holds_secret(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    if secret.is_empty() {
        return false;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let bearer = header("authorization")
        .and_then(|v| v.trim().split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token);
    if let Some(token) = bearer {
        return same(token.trim().as_bytes(), secret.as_bytes());
    }
    if let Some(token) = header("x-havn-token") {
        return same(token.trim().as_bytes(), secret.as_bytes());
    }
    let signature = header("x-hub-signature-256").or_else(|| header("x-havn-signature-256"));
    if let Some(given) = signature.and_then(|v| v.trim().strip_prefix("sha256=")) {
        let wanted = hex(&hmac_sha256(secret.as_bytes(), body));
        return same(given.to_ascii_lowercase().as_bytes(), wanted.as_bytes());
    }
    false
}

/// When each webhook was called in the last minute.
static CALLS: LazyLock<Mutex<HashMap<Uuid, VecDeque<Instant>>>> = LazyLock::new(Default::default);

/// Whether webhook `id` may be called again now, counting this call.
fn may_call(id: Uuid, now: Instant) -> bool {
    let mut calls = CALLS.lock().unwrap_or_else(PoisonError::into_inner);
    let minute = Duration::from_secs(60);
    calls.retain(|_, times| {
        times
            .back()
            .is_some_and(|t| now.duration_since(*t) < minute)
    });
    let times = calls.entry(id).or_default();
    while times
        .front()
        .is_some_and(|t| now.duration_since(*t) >= minute)
    {
        times.pop_front();
    }
    if times.len() >= CALLS_PER_MINUTE {
        return false;
    }
    times.push_back(now);
    true
}

/// The text of a call's `body`, as the agent is given it: UTF-8 cut to
/// [`MAX_BODY_CHARS`], or `None` for a body that is not text.
fn body_text(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let mut out: String = text.chars().take(MAX_BODY_CHARS).collect();
    if out.len() < text.len() {
        out.push_str("\n[…cut]");
    }
    Some(out)
}

/// `POST /hooks/{id}`: a call of a webhook. Answered 202 once it is
/// checked; the agent, started when it is not running, then acts on it.
pub async fn call(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // One answer for a webhook that is not there and a wrong secret, so
    // ids cannot be found out.
    let refused = || ApiError::Unauthorized;
    let Ok(id) = Uuid::from_str(&id) else {
        return Err(refused());
    };
    let Some(hook) = webhooks::find(&state.db, id).await? else {
        return Err(refused());
    };
    if !holds_secret(&hook.secret, &headers, &body) {
        return Err(refused());
    }
    // After the secret, so a restart does not tell which ids are there.
    if state.supervisor.shutting_down() {
        return Err(ApiError::Unavailable(
            "the gateway is restarting; call again once it is back".into(),
        ));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(ApiError::BadRequest(format!(
            "a body of at most {MAX_BODY_BYTES} bytes"
        )));
    }
    let Some(text) = body_text(&body) else {
        return Err(ApiError::BadRequest("the body is not UTF-8 text".into()));
    };
    if !may_call(hook.id, Instant::now()) {
        return Err(ApiError::TooManyRequests(format!(
            "at most {CALLS_PER_MINUTE} calls a minute"
        )));
    }
    let agent = havn_db::repo::agents::find_by_id(&state.db, hook.agent_id)
        .await?
        .ok_or_else(refused)?;
    let now = Utc::now();
    if let Err(e) = webhooks::called(&state.db, hook.id, now).await {
        warn!(webhook = %hook.id, error = %e, "could not note a webhook call");
    }
    let run_id = Uuid::now_v7().to_string();
    crate::audit::record(
        &state.db,
        agent.owner_id,
        agent.team_id,
        Some(agent.id),
        "webhook.called",
        serde_json::json!({
            "webhook_id": hook.id.to_string(),
            "run_id": run_id,
            "bytes": body.len(),
        }),
    )
    .await;
    let run = run_id.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::api::agents::ensure_running(&state, agent.owner_id, agent.id).await {
            warn!(webhook = %hook.id, error = %e, "could not start the agent for a webhook call");
            return;
        }
        let channels = state.channels.load_full();
        let targets = crate::heartbeat::targets_for(
            &state.db,
            &state.bindings,
            &channels,
            &agent,
            &hook.deliver,
        )
        .await;
        let frame = havn_proto::GatewayToAgent::WebhookCall(havn_proto::WebhookCall {
            webhook_id: hook.id.to_string(),
            run_id: run,
            name: hook.name,
            instructions: hook.instructions,
            body: text,
            web: targets.web,
            to: targets.to,
        });
        if let Err(e) = state.registry.send(agent.id, frame).await {
            warn!(webhook = %hook.id, error = %e, "could not hand a webhook call to its agent");
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "run_id": run_id })),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        // RFC 4231, test cases 2 and 6 (a key longer than a block).
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_call_holds_the_secret_as_a_token_or_a_signature() {
        let secret = "s3cret";
        let body = br#"{"action":"opened"}"#;
        let with = |name: &str, value: String| {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            headers
        };
        assert!(holds_secret(
            secret,
            &with("authorization", "Bearer s3cret".into()),
            body
        ));
        assert!(holds_secret(
            secret,
            &with("authorization", "bearer s3cret".into()),
            body
        ));
        assert!(!holds_secret(
            "",
            &with("x-havn-token", String::new()),
            body
        ));
        assert!(holds_secret(
            secret,
            &with("x-havn-token", "s3cret".into()),
            body
        ));
        let signed = format!("sha256={}", hex(&hmac_sha256(secret.as_bytes(), body)));
        assert!(holds_secret(
            secret,
            &with("x-hub-signature-256", signed.clone()),
            body
        ));
        assert!(holds_secret(
            secret,
            &with(
                "x-havn-signature-256",
                signed.to_uppercase().replace("SHA256=", "sha256=")
            ),
            body
        ));

        assert!(!holds_secret(secret, &HeaderMap::new(), body));
        assert!(!holds_secret(
            secret,
            &with("authorization", "Bearer s3cre".into()),
            body
        ));
        assert!(!holds_secret(
            secret,
            &with("x-havn-token", "nope".into()),
            body
        ));
        assert!(!holds_secret(
            secret,
            &with("x-hub-signature-256", signed),
            b"{\"action\":\"closed\"}"
        ));
        assert!(!holds_secret(
            secret,
            &with("x-hub-signature-256", "sha1=abc".into()),
            body
        ));
    }

    #[test]
    fn a_webhook_takes_so_many_calls_a_minute() {
        let id = Uuid::now_v7();
        let start = Instant::now();
        for _ in 0..CALLS_PER_MINUTE {
            assert!(may_call(id, start));
        }
        assert!(!may_call(id, start + Duration::from_secs(1)));
        assert!(
            may_call(Uuid::now_v7(), start),
            "each webhook counts its own"
        );
        assert!(
            may_call(id, start + Duration::from_secs(61)),
            "a minute later"
        );
    }

    #[test]
    fn a_body_is_given_as_text_cut_short() {
        assert_eq!(body_text(b"hi").as_deref(), Some("hi"));
        assert!(body_text(&[0xff, 0xfe]).is_none());
        let long = "x".repeat(MAX_BODY_CHARS + 10);
        let given = body_text(long.as_bytes()).unwrap();
        assert!(given.ends_with("[…cut]") && given.chars().count() < long.len());
    }

    #[tokio::test]
    async fn a_call_with_the_secret_goes_to_the_agent_and_others_are_refused_alike() {
        let (state, owner) = crate::auth::tests::state_with(false).await;
        let agent = havn_db::repo::agents::create(
            &state.db,
            havn_db::repo::agents::NewAgent {
                owner_id: owner,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        let hook = webhooks::create(
            &state.db,
            NewWebhook {
                agent_id: agent.id,
                name: "alerts",
                instructions: "summarise",
                deliver: "web",
            },
        )
        .await
        .expect("hook");
        state
            .registry
            .register(havn_spawner::AgentHandle::new(agent.id, 1))
            .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        state.registry.attach(agent.id, tx).await.expect("attach");

        let bearer = |secret: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {secret}").parse().unwrap());
            headers
        };
        let refused = |r: Result<(StatusCode, Json<serde_json::Value>), ApiError>| {
            matches!(r, Err(ApiError::Unauthorized))
        };
        assert!(refused(
            call(
                State(state.clone()),
                Path(hook.id.to_string()),
                bearer("wrong"),
                Bytes::from_static(b"x")
            )
            .await
        ));
        assert!(refused(
            call(
                State(state.clone()),
                Path(Uuid::now_v7().to_string()),
                bearer(&hook.secret),
                Bytes::from_static(b"x")
            )
            .await
        ));
        assert!(refused(
            call(
                State(state.clone()),
                Path("not-an-id".into()),
                bearer(&hook.secret),
                Bytes::from_static(b"x")
            )
            .await
        ));
        assert!(matches!(
            call(
                State(state.clone()),
                Path(hook.id.to_string()),
                bearer(&hook.secret),
                Bytes::from_static(&[0xff])
            )
            .await,
            Err(ApiError::BadRequest(_))
        ));

        let (status, Json(answer)) = call(
            State(state.clone()),
            Path(hook.id.to_string()),
            bearer(&hook.secret),
            Bytes::from_static(b"disk full on db-1"),
        )
        .await
        .expect("called");
        assert_eq!(status, StatusCode::ACCEPTED);
        let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("in time")
            .expect("frame");
        let havn_proto::GatewayToAgent::WebhookCall(given) = frame else {
            panic!("{frame:?}");
        };
        assert_eq!(given.run_id, answer["run_id"].as_str().unwrap());
        assert_eq!(
            (
                given.name.as_str(),
                given.instructions.as_str(),
                given.body.as_str()
            ),
            ("alerts", "summarise", "disk full on db-1")
        );
        assert_eq!(
            given.web,
            crate::webchat::session_for(&owner.to_string(), agent.id).to_string()
        );
        assert!(given.to.is_empty(), "web: kept in the web chat only");
        let noted = webhooks::find(&state.db, hook.id)
            .await
            .expect("find")
            .expect("hook");
        assert_eq!(noted.calls, 1);
    }
}
