//! `GET /usage` (issue #235): what a user's model calls used, by day, agent,
//! model and user, in tokens, and in US dollars for the models whose price
//! `[[llm.models]]` gives. The gateway's owner sees everyone's.

use std::collections::BTreeMap;
use std::str::FromStr as _;

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use havn_core::UserId;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::ApiError;
use crate::auth::AuthedUser;

/// Days shown by default, and at most.
const DAYS: u32 = 30;
const MAX_DAYS: u32 = 366;

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub days: Option<u32>,
    /// Whose: the caller when absent; `all`, or another user's id, for the
    /// gateway's owner only.
    pub user: Option<String>,
}

/// One line of a table: a day, an agent, a model or a user.
#[derive(Debug, Serialize, PartialEq)]
pub struct Bucket {
    /// What it is: the day, an agent's or a user's id, or the model.
    pub key: String,
    /// What to call it.
    pub label: String,
    pub calls: i64,
    pub tokens: i64,
    /// US dollars at the prices set now, of its priced calls.
    pub cost: f64,
    /// Whether some of its calls are of a model without a price.
    pub unpriced: bool,
}

#[derive(Debug, Serialize)]
pub struct UsageView {
    /// The start of the first day shown, UTC.
    pub since: DateTime<Utc>,
    pub days: u32,
    pub currency: &'static str,
    /// Whose this is: a user's id, or `all`.
    pub user: String,
    /// Whether the caller may see others' (the gateway's owner).
    pub everyone: bool,
    /// The users the caller may pick, for the gateway's owner.
    pub users: Vec<UserChoice>,
    pub calls: i64,
    pub tokens: i64,
    /// The cost of the calls of priced models.
    pub cost: f64,
    /// Tokens of models without a price, not in `cost`.
    pub unpriced_tokens: i64,
    /// Calls not settled (still running, or whose provider reported no
    /// usage): counted at what was reserved for them, their most.
    pub unsettled_calls: i64,
    /// Searches through a search backend, counted apart.
    pub searches: i64,
    pub by_day: Vec<Bucket>,
    pub by_agent: Vec<Bucket>,
    pub by_model: Vec<Bucket>,
    /// When everyone's is shown.
    pub by_user: Vec<Bucket>,
}

#[derive(Debug, Serialize)]
pub struct UserChoice {
    pub user_id: String,
    pub display_name: String,
}

/// Sums by key.
#[derive(Default)]
struct Table(BTreeMap<String, Bucket>);

impl Table {
    fn add(&mut self, key: &str, label: &str, calls: i64, tokens: i64, cost: Option<f64>) {
        let bucket = self.0.entry(key.to_string()).or_insert_with(|| Bucket {
            key: key.to_string(),
            label: label.to_string(),
            calls: 0,
            tokens: 0,
            cost: 0.0,
            unpriced: false,
        });
        bucket.calls += calls;
        bucket.tokens += tokens;
        match cost {
            Some(c) => bucket.cost += c,
            None => bucket.unpriced = true,
        }
    }

    /// The most costly first, then the most used.
    fn by_cost(self) -> Vec<Bucket> {
        let mut out: Vec<Bucket> = self.0.into_values().collect();
        out.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.tokens.cmp(&a.tokens)));
        out
    }

    /// The latest first.
    fn by_key_down(self) -> Vec<Bucket> {
        self.0.into_values().rev().collect()
    }
}

/// `GET /usage?days=30&user=<id|all>`.
pub async fn usage(
    State(state): State<AppState>,
    user: AuthedUser,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsageView>, ApiError> {
    // With trust_header there is no one owner of the gateway: each sees
    // their own, as the gateway's settings are config.toml's there.
    let owner = user.id == state.bootstrap_user_id && state.trusted_proxies.is_none();
    let refused = || ApiError::Forbidden("only the gateway's owner sees others' usage".into());
    let whose: Option<UserId> = match q.user.as_deref() {
        None => Some(user.id),
        Some("all") if owner => None,
        Some("all") => return Err(refused()),
        Some(id) => {
            let id =
                UserId::from_str(id).map_err(|_| ApiError::BadRequest("not a user id".into()))?;
            if id != user.id && !owner {
                return Err(refused());
            }
            Some(id)
        }
    };
    let days = q.days.unwrap_or(DAYS).clamp(1, MAX_DAYS);
    // Whole UTC days, today the last.
    let first = Utc::now().date_naive() - ChronoDuration::days(i64::from(days) - 1);
    let since = first
        .and_hms_opt(0, 0, 0)
        .map_or_else(Utc::now, |t| t.and_utc());
    let since_rfc = since.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let found = havn_db::repo::credential_usage::usage_by_day(&state.db, whose, &since_rfc).await?;
    let catalog = state.models.load();

    let mut by_day = Table::default();
    let mut by_agent = Table::default();
    let mut by_model = Table::default();
    let mut by_user = Table::default();
    let (mut calls, mut tokens, mut cost) = (0, 0, 0.0);
    let (mut unpriced_tokens, mut unsettled_calls, mut searches) = (0, 0, 0);
    for d in found {
        // Calls of a provider the proxy does not dispatch to are of a web
        // search backend (Brave, Tavily, Exa): counted as searches.
        if crate::llm_proxy::dispatch_provider(&d.provider).is_err() {
            searches += d.calls;
            continue;
        }
        let model = format!("{}:{}", d.provider, d.model);
        let price = catalog
            .models
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.price);
        let row_cost =
            price.map(|p| p.cost(d.tokens_in, d.cache_read, d.cache_write, d.tokens_out));
        let row_tokens = d.tokens_in + d.tokens_out;
        calls += d.calls;
        tokens += row_tokens;
        unsettled_calls += d.unsettled;
        match row_cost {
            Some(c) => cost += c,
            None => unpriced_tokens += row_tokens,
        }
        by_day.add(&d.day, &d.day, d.calls, row_tokens, row_cost);
        let (agent_key, mut agent_label) = match (&d.agent_id, &d.agent_name) {
            (Some(id), Some(name)) => (id.clone(), name.clone()),
            (Some(id), None) => (id.clone(), "(a deleted agent)".to_string()),
            // Per user: everyone's calls with no agent are not one line.
            (None, _) => (format!("none:{}", d.user_id), "(no agent)".to_string()),
        };
        if whose.is_none() {
            agent_label = format!("{agent_label} ({})", d.display_name);
        }
        by_agent.add(&agent_key, &agent_label, d.calls, row_tokens, row_cost);
        by_model.add(&model, &model, d.calls, row_tokens, row_cost);
        if whose.is_none() {
            by_user.add(&d.user_id, &d.display_name, d.calls, row_tokens, row_cost);
        }
    }
    let users = if owner {
        havn_db::repo::users::list(&state.db)
            .await?
            .into_iter()
            .map(|u| UserChoice {
                user_id: u.id.to_string(),
                display_name: u.display_name,
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(Json(UsageView {
        since,
        days,
        currency: "USD",
        user: whose.map_or_else(|| "all".to_string(), |u| u.to_string()),
        everyone: owner,
        users,
        calls,
        tokens,
        cost,
        unpriced_tokens,
        unsettled_calls,
        searches,
        by_day: by_day.by_key_down(),
        by_agent: by_agent.by_cost(),
        by_model: by_model.by_cost(),
        by_user: by_user.by_cost(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    async fn spend(
        state: &AppState,
        user: UserId,
        provider: &str,
        model: &str,
        used: (i64, i64),
        settled: bool,
    ) {
        use havn_db::repo::credential_usage::{Caps, Reservation, Used, reserve, settle};
        let scope_id = user.to_string();
        let name = format!("k{}", uuid::Uuid::now_v7());
        let cred = havn_db::repo::credentials::create(
            &state.db,
            havn_db::repo::credentials::NewCredential {
                scope: havn_db::repo::credentials::CredentialScope::User,
                scope_id: &scope_id,
                provider,
                name: Some(&name),
                api_key_ciphertext: b"x",
                priority: 0,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("credential");
        let reserved = reserve(
            &state.db,
            Reservation {
                credential_id: cred.id,
                user_id: user,
                agent_id: None,
                provider,
                model,
                tokens_in: used.0,
                max_tokens_out: used.1,
                min_tokens_out: 0,
                caps: Caps::default(),
                day_start: "1970-01-01T00:00:00.000Z",
                minute_start: "1970-01-01T00:00:00.000Z",
            },
        )
        .await
        .expect("reserve")
        .expect("room");
        if settled {
            settle(
                &state.db,
                &reserved.id,
                "answered-as",
                Used {
                    tokens_in: used.0,
                    tokens_out: used.1,
                    ..Used::default()
                },
            )
            .await
            .expect("settle");
        }
    }

    fn authed(id: UserId) -> AuthedUser {
        AuthedUser {
            id,
            display_name: "u".into(),
        }
    }

    async fn ask(state: &AppState, who: UserId, user: Option<&str>) -> Result<UsageView, ApiError> {
        usage(
            State(state.clone()),
            authed(who),
            Query(UsageQuery {
                days: None,
                user: user.map(str::to_string),
            }),
        )
        .await
        .map(|Json(v)| v)
    }

    #[tokio::test]
    async fn usage_is_priced_by_the_model_asked_for_and_others_are_the_owner_s_to_see() {
        let (state, owner) = crate::auth::tests::state_with(false).await;
        let other = havn_db::repo::users::create(
            &state.db,
            havn_db::repo::users::NewUser { display_name: "o" },
        )
        .await
        .expect("user")
        .id;
        let mut catalog = (**state.models.load()).clone();
        catalog.models = vec![
            serde_json::from_value(serde_json::json!({
                "id": "anthropic:priced",
                "price": {"input": 2.0, "output": 10.0}
            }))
            .expect("entry"),
            serde_json::from_value(serde_json::json!({"id": "anthropic:free"})).expect("entry"),
        ];
        state.models.store(std::sync::Arc::new(catalog));
        spend(
            &state,
            owner,
            "anthropic",
            "priced",
            (1_000_000, 100_000),
            true,
        )
        .await;
        spend(&state, owner, "anthropic", "free", (500, 50), true).await;
        spend(&state, owner, "anthropic", "free", (100, 900), false).await;
        spend(&state, owner, "brave", "web_search", (1, 0), true).await;
        spend(&state, other, "anthropic", "priced", (10, 10), true).await;

        let mine = ask(&state, owner, None).await.expect("mine");
        assert!((mine.cost - 3.0).abs() < 1e-9, "{}", mine.cost);
        assert_eq!(mine.unpriced_tokens, 1550);
        assert_eq!((mine.calls, mine.unsettled_calls, mine.searches), (3, 1, 1));
        assert!(
            mine.by_model
                .iter()
                .any(|b| b.key == "anthropic:priced" && !b.unpriced)
        );
        assert!(
            mine.by_model
                .iter()
                .any(|b| b.key == "anthropic:free" && b.unpriced)
        );
        assert!(
            !mine.by_model.iter().any(|b| b.key.starts_with("brave")),
            "a search is not a model"
        );
        assert_eq!(mine.by_day.len(), 1);
        assert!(mine.by_user.is_empty() && mine.everyone && mine.users.len() >= 2);

        let all = ask(&state, owner, Some("all")).await.expect("all");
        assert_eq!(all.calls, 4);
        assert_eq!(all.by_user.len(), 2);
        assert_eq!(
            all.by_agent.len(),
            2,
            "each user's calls with no agent are their own line"
        );
        let theirs_by_owner = ask(&state, owner, Some(&other.to_string()))
            .await
            .expect("theirs");
        assert_eq!(theirs_by_owner.calls, 1);

        // Someone else sees theirs only.
        let theirs = ask(&state, other, None).await.expect("theirs");
        assert_eq!(theirs.calls, 1);
        assert!(!theirs.everyone && theirs.users.is_empty());
        assert!(matches!(
            ask(&state, other, Some("all")).await,
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            ask(&state, other, Some(&owner.to_string())).await,
            Err(ApiError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn with_trust_header_no_one_sees_others_usage() {
        let (state, owner) = crate::auth::tests::state_with(true).await;
        let mine = ask(&state, owner, None).await.expect("mine");
        assert!(!mine.everyone);
        assert!(matches!(
            ask(&state, owner, Some("all")).await,
            Err(ApiError::Forbidden(_))
        ));
    }
}
