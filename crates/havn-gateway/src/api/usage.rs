//! `GET /usage` (issue #235): what a user's model calls used, by day, agent
//! and model, in tokens, and in US dollars for the models whose price
//! `[[llm.models]]` gives. The gateway's owner sees everyone's.

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

#[derive(Debug, Serialize)]
pub struct UsageRow {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub user_id: String,
    pub display_name: String,
    /// `None` for calls made for no agent (a model check, say).
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    /// As `[[llm.models]]` lists it: `provider:model`.
    pub model: String,
    pub calls: i64,
    pub tokens_in: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub tokens_out: i64,
    /// In US dollars, at the price the model has now; `None` for a model
    /// without one.
    pub cost: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct UsageView {
    pub since: DateTime<Utc>,
    pub days: u32,
    pub currency: &'static str,
    /// Whose this is: a user's id, or `all`.
    pub user: String,
    /// Whether the caller may see everyone's (the gateway's owner).
    pub everyone: bool,
    /// The users the caller may pick, for the gateway's owner.
    pub users: Vec<UserChoice>,
    pub rows: Vec<UsageRow>,
    /// The cost of the calls of priced models.
    pub cost: f64,
    /// Tokens of models without a price, not in `cost`.
    pub unpriced_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct UserChoice {
    pub user_id: String,
    pub display_name: String,
}

/// `GET /usage?days=30&user=<id|all>`.
pub async fn usage(
    State(state): State<AppState>,
    user: AuthedUser,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsageView>, ApiError> {
    let owner = user.id == state.bootstrap_user_id;
    let whose: Option<UserId> = match q.user.as_deref() {
        None => Some(user.id),
        Some("all") if owner => None,
        Some(id) => {
            let id =
                UserId::from_str(id).map_err(|_| ApiError::BadRequest("not a user id".into()))?;
            if id != user.id && !owner {
                return Err(ApiError::Forbidden(
                    "only the gateway's owner sees others' usage".into(),
                ));
            }
            Some(id)
        }
    };
    let days = q.days.unwrap_or(DAYS).clamp(1, MAX_DAYS);
    let since = Utc::now() - ChronoDuration::days(days.into());
    let since_rfc = since.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let found = havn_db::repo::credential_usage::usage_by_day(&state.db, whose, &since_rfc).await?;
    let catalog = state.models.load();
    let mut cost = 0.0;
    let mut unpriced_tokens = 0;
    let rows = found
        .into_iter()
        .map(|d| {
            let model = format!("{}:{}", d.provider, d.model);
            let price = catalog
                .models
                .iter()
                .find(|m| m.id == model)
                .and_then(|m| m.price);
            let row_cost =
                price.map(|p| p.cost(d.tokens_in, d.cache_read, d.cache_write, d.tokens_out));
            match row_cost {
                Some(c) => cost += c,
                None => unpriced_tokens += d.tokens_in + d.tokens_out,
            }
            UsageRow {
                day: d.day,
                user_id: d.user_id,
                display_name: d.display_name,
                agent_id: d.agent_id,
                agent_name: d.agent_name,
                model,
                calls: d.calls,
                tokens_in: d.tokens_in,
                cache_read: d.cache_read,
                cache_write: d.cache_write,
                tokens_out: d.tokens_out,
                cost: row_cost,
            }
        })
        .collect();
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
        rows,
        cost,
        unpriced_tokens,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    async fn spend(state: &AppState, user: UserId, model: &str, tokens_in: i64, tokens_out: i64) {
        use havn_db::repo::credential_usage::{Caps, Reservation, Used, reserve, settle};
        let scope_id = user.to_string();
        let cred = havn_db::repo::credentials::create(
            &state.db,
            havn_db::repo::credentials::NewCredential {
                scope: havn_db::repo::credentials::CredentialScope::User,
                scope_id: &scope_id,
                provider: "anthropic",
                name: None,
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
                provider: "anthropic",
                model,
                tokens_in,
                max_tokens_out: tokens_out,
                min_tokens_out: 0,
                caps: Caps::default(),
                day_start: "1970-01-01T00:00:00.000Z",
                minute_start: "1970-01-01T00:00:00.000Z",
            },
        )
        .await
        .expect("reserve")
        .expect("room");
        settle(
            &state.db,
            &reserved.id,
            "answered-as",
            Used {
                tokens_in,
                tokens_out,
                ..Used::default()
            },
        )
        .await
        .expect("settle");
    }

    fn authed(id: UserId) -> AuthedUser {
        AuthedUser {
            id,
            display_name: "u".into(),
        }
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
        spend(&state, owner, "priced", 1_000_000, 100_000).await;
        spend(&state, owner, "free", 500, 50).await;
        spend(&state, other, "priced", 10, 10).await;

        let Json(mine) = usage(
            State(state.clone()),
            authed(owner),
            Query(UsageQuery {
                days: None,
                user: None,
            }),
        )
        .await
        .expect("mine");
        assert_eq!(mine.rows.len(), 2);
        assert!((mine.cost - 3.0).abs() < 1e-9, "{}", mine.cost);
        assert_eq!(mine.unpriced_tokens, 550);
        assert!(
            mine.rows
                .iter()
                .any(|r| r.model == "anthropic:priced" && r.cost.is_some())
        );
        assert!(
            mine.rows
                .iter()
                .any(|r| r.model == "anthropic:free" && r.cost.is_none())
        );
        assert!(mine.everyone && mine.users.len() >= 2);

        let Json(all) = usage(
            State(state.clone()),
            authed(owner),
            Query(UsageQuery {
                days: Some(7),
                user: Some("all".into()),
            }),
        )
        .await
        .expect("all");
        assert_eq!(all.rows.len(), 3);

        // Someone else sees theirs only.
        let Json(theirs) = usage(
            State(state.clone()),
            authed(other),
            Query(UsageQuery {
                days: None,
                user: None,
            }),
        )
        .await
        .expect("theirs");
        assert_eq!(theirs.rows.len(), 1);
        assert!(!theirs.everyone && theirs.users.is_empty());
        for asked in ["all".to_string(), owner.to_string()] {
            let refused = usage(
                State(state.clone()),
                authed(other),
                Query(UsageQuery {
                    days: None,
                    user: Some(asked),
                }),
            )
            .await;
            assert!(refused.is_err());
        }
    }
}
