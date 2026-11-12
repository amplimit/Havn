//! Credential resolution chain (spec §7.2) with team fall-through and
//! per-user caps for team-shared credentials.
//!
//! Resolution order:
//! ```text
//! 1. User-level credentials for `provider`, sorted by priority desc
//! 2. For each team the user belongs to, that team's credentials for
//!    `provider`, sorted by priority desc.
//! ```
//!
//! Earlier drafts had only step 1 — single-user mode. Phase 2 extends
//! to step 2 so a team admin can drop a shared API key once and have
//! every member's agents fall back to it. Per-user caps live in the
//! credential's `limits.per_user.{max_tokens_per_day,
//! max_requests_per_minute}` JSON path so admins can prevent one
//! heavy user from draining the team's daily quota — see [`credential_caps`],
//! which also reads the credential-wide daily token cap. The LLM proxy
//! enforces them by reserving each request's tokens before the call.

use havn_core::UserId;
use havn_db::DbError;
use havn_db::repo::credential_usage::Caps;
use havn_db::repo::credentials::{self as repo, Credential, CredentialScope};
use havn_db::repo::team_memberships;
use sqlx::SqlitePool;
use tracing::debug;

/// Return the full credential chain for `(user_id, provider)`. Order
/// matches spec §7.2: user-scoped first (member's own keys take
/// precedence), then team-scoped (admins drop a fallback for the
/// whole team).
///
/// Within each scope, `priority DESC` sorts; across scopes, the chain
/// concatenates. Disabled credentials are filtered at the SQL layer
/// by [`repo::list_active_for_provider`].
pub async fn resolve_for_user(
    db: &SqlitePool,
    user_id: UserId,
    provider: &str,
) -> Result<Vec<Credential>, DbError> {
    // v0.2 namespace bridge: callers pass a bare provider name like
    // `"anthropic"` (the LLM proxy's internal dispatch name); v0.2
    // credential rows live under `"llm:anthropic"`. Try the namespaced
    // form first (the canonical post-migration shape), fall through
    // to the bare form for v0.1 rows that haven't been migrated for
    // any reason. Two SELECTs maximum, both hitting the
    // (scope, scope_id, provider, priority) index.
    let candidates: [String; 2] = [format!("llm:{provider}"), provider.to_string()];

    // Step 1: user scope, both forms.
    let mut chain = Vec::new();
    for p in &candidates {
        let part =
            repo::list_active_for_provider(db, CredentialScope::User, &user_id.to_string(), p)
                .await?;
        chain.extend(part);
    }

    // Step 2: team scope, both forms. One DB hit to enumerate the
    // user's teams, then 2× per team. Bounded — most users belong
    // to ≤2 teams in practice.
    let teams = team_memberships::list_for_user(db, user_id).await?;
    for team in teams {
        for p in &candidates {
            let team_chain = repo::list_active_for_provider(
                db,
                CredentialScope::Team,
                &team.team_id.to_string(),
                p,
            )
            .await?;
            chain.extend(team_chain);
        }
    }

    debug!(
        user = %user_id,
        provider,
        len = chain.len(),
        "resolved credential chain (user + team scopes)"
    );
    Ok(chain)
}

/// The caps `cred.limits` sets (spec §7.3 / §10.3):
///
/// ```json
/// "limits": {
///   "max_tokens_per_day": 100000,        // credential-wide
///   "per_user": {
///     "max_tokens_per_day": 5000,        // per-user per-day
///     "max_requests_per_minute": 30      // per-user RPM
///   }
/// }
/// ```
///
/// Every field is optional; a missing, non-positive or non-integer value
/// means no cap. The per-user caps let a team admin stop one heavy user
/// from draining a shared key; the credential-wide cap sits above them.
/// `agent_tokens` is left at 0 for the caller to fill from the agent's
/// policy.
#[must_use]
pub fn credential_caps(cred: &Credential) -> Caps {
    let positive = |v: Option<&serde_json::Value>| {
        v.and_then(serde_json::Value::as_i64)
            .filter(|&n| n > 0)
            .unwrap_or(0)
    };
    let per_user = cred.limits.get("per_user");
    Caps {
        credential_tokens: positive(cred.limits.get("max_tokens_per_day")),
        user_tokens: positive(per_user.and_then(|p| p.get("max_tokens_per_day"))),
        user_requests_per_minute: positive(per_user.and_then(|p| p.get("max_requests_per_minute"))),
        agent_tokens: 0,
    }
}

/// "Today" boundary in the same RFC 3339 / UTC format as `created_at` columns,
/// so lexicographic comparison in the SQL `WHERE created_at >= ?` matches.
pub fn today_start_utc_rfc3339() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT00:00:00.000Z")
        .to_string()
}

/// 60-second-ago anchor, same format as above. The clock skew between
/// the gateway's `now()` and SQLite's `strftime('now')` is sub-second
/// in practice; a one-minute window is wide enough that this doesn't
/// matter operationally.
pub fn minute_ago_utc_rfc3339() -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(60))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::Policy;
    use havn_db::connect_in_memory;
    use havn_db::repo::credentials::NewCredential;
    use havn_db::repo::roles::{NewRole, create as create_role};
    use havn_db::repo::teams::{NewTeam, create as create_team};
    use havn_db::repo::users::{NewUser, create as create_user};

    #[tokio::test]
    async fn returns_empty_when_no_credentials_match_provider() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id;

        let chain = resolve_for_user(&pool, owner, "anthropic")
            .await
            .expect("resolve");
        assert!(chain.is_empty());
    }

    #[tokio::test]
    async fn descending_priority_within_user_scope() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id;

        for (priority, key) in [(1i32, &b"low"[..]), (10, &b"high"[..]), (5, &b"mid"[..])] {
            havn_db::repo::credentials::create(
                &pool,
                NewCredential {
                    scope: CredentialScope::User,
                    scope_id: &owner.to_string(),
                    provider: "anthropic",
                    name: None,
                    api_key_ciphertext: key,
                    priority,
                    limits: serde_json::json!({}),
                },
            )
            .await
            .expect("create cred");
        }

        let chain = resolve_for_user(&pool, owner, "anthropic")
            .await
            .expect("resolve");
        let priorities: Vec<_> = chain.iter().map(|c| c.priority).collect();
        assert_eq!(priorities, vec![10, 5, 1]);
    }

    #[tokio::test]
    async fn user_scope_precedes_team_scope() {
        // Admin sets up a team key with priority 100; user has their own
        // key at priority 1. The user key MUST come first regardless,
        // because the user scope wins by spec §7.2.
        let pool = connect_in_memory().await.expect("connect");
        let user = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user");
        let team = create_team(&pool, NewTeam { name: "t" })
            .await
            .expect("team");
        let role = create_role(
            &pool,
            NewRole {
                team_id: Some(team.id),
                name: "admin",
                policy: &Policy::default(),
            },
        )
        .await
        .expect("role");
        team_memberships::add(&pool, user.id, team.id, role.id)
            .await
            .expect("add");

        // User key, priority 1.
        havn_db::repo::credentials::create(
            &pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &user.id.to_string(),
                provider: "anthropic",
                name: None,
                api_key_ciphertext: b"user-key",
                priority: 1,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("user cred");
        // Team key, priority 100 — should still come AFTER the user key.
        havn_db::repo::credentials::create(
            &pool,
            NewCredential {
                scope: CredentialScope::Team,
                scope_id: &team.id.to_string(),
                provider: "anthropic",
                name: None,
                api_key_ciphertext: b"team-key",
                priority: 100,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("team cred");

        let chain = resolve_for_user(&pool, user.id, "anthropic")
            .await
            .expect("resolve");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].api_key_ciphertext, b"user-key");
        assert_eq!(chain[1].api_key_ciphertext, b"team-key");
    }

    /// Helper: create one credential with the given limits JSON and return it.
    async fn make_cred(
        pool: &SqlitePool,
        owner: UserId,
        limits: serde_json::Value,
    ) -> havn_db::repo::credentials::Credential {
        havn_db::repo::credentials::create(
            pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &owner.to_string(),
                provider: "anthropic",
                name: None,
                api_key_ciphertext: b"k",
                priority: 1,
                limits,
            },
        )
        .await
        .expect("create cred")
    }

    #[tokio::test]
    async fn caps_are_read_from_limits() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id;
        let none = make_cred(&pool, owner, serde_json::json!({})).await;
        assert_eq!(credential_caps(&none), Caps::default());
        let zero = make_cred(
            &pool,
            owner,
            serde_json::json!({"max_tokens_per_day": 0, "max_usd_per_day": 5.0}),
        )
        .await;
        assert_eq!(credential_caps(&zero), Caps::default());
        let all = make_cred(
            &pool,
            owner,
            serde_json::json!({
                "max_tokens_per_day": 100_000,
                "per_user": { "max_tokens_per_day": 5_000, "max_requests_per_minute": 30 }
            }),
        )
        .await;
        assert_eq!(
            credential_caps(&all),
            Caps {
                credential_tokens: 100_000,
                user_tokens: 5_000,
                user_requests_per_minute: 30,
                agent_tokens: 0,
            }
        );
    }
}
