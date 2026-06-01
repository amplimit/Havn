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
//! heavy user from draining the team's daily quota — see [`per_user_ceilings_remaining`].
//!
//! [`daily_tokens_remaining`] enforces the credential-WIDE daily token
//! cap (still relevant for team creds — the team-level ceiling sits
//! ABOVE any per-user ceiling).

use havn_core::UserId;
use havn_db::DbError;
use havn_db::repo::credential_usage;
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

/// Remaining daily token budget for `cred` overall (spec §7.3 v0.6).
///
/// Returns:
/// - `Ok(None)` — no `max_tokens_per_day` cap, or cap = 0 (treated as
///   unlimited per spec §7.3). Proxy proceeds without a gate.
/// - `Ok(Some(remaining))` — `remaining > 0` means usable; `≤ 0`
///   means today's cap is exhausted and the proxy must skip.
/// - `Err(_)` — DB failure; proxy treats as fail-safe-skip.
pub async fn daily_tokens_remaining(
    db: &SqlitePool,
    cred: &Credential,
) -> Result<Option<i64>, DbError> {
    let cap = cred
        .limits
        .get("max_tokens_per_day")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    if cap <= 0 {
        return Ok(None);
    }
    let today_start = today_start_utc_rfc3339();
    let used = credential_usage::tokens_since(db, cred.id, &today_start).await?;
    Ok(Some(cap - used))
}

/// Outcome of the per-user ceiling check. `Ok` either way (the call
/// site only treats the rate-limited variants as "skip this credential
/// for this user").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerUserVerdict {
    /// No per-user limits configured, or all limits leave headroom.
    Allowed,
    /// At least one per-user limit is exhausted for `user_id` against
    /// `cred` — credential resolver moves to the next entry in the
    /// chain. Spec §7.2: equivalent to "credential exhausted, try
    /// next" for the *requesting user*; other users on the same
    /// credential are unaffected.
    DenyTokens,
    DenyRequests,
}

/// Per-user-quota gate for team-shared credentials. Reads the
/// `limits.per_user` sub-object:
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
/// Both per-user fields are optional. When absent, returns `Allowed`
/// — admins didn't configure a per-user clamp on this credential.
///
/// Spec §10.3: "shared keys with per-user token / RPM caps" is the
/// admin pattern this enables. Without the per-user ceiling, one
/// runaway agent on a team key drains the daily budget and causes
/// `429`-style fallthrough for everyone else.
pub async fn per_user_ceilings_remaining(
    db: &SqlitePool,
    cred: &Credential,
    user_id: UserId,
) -> Result<PerUserVerdict, DbError> {
    let Some(per_user) = cred.limits.get("per_user").and_then(|v| v.as_object()) else {
        return Ok(PerUserVerdict::Allowed);
    };

    if let Some(token_cap) = per_user
        .get("max_tokens_per_day")
        .and_then(serde_json::Value::as_i64)
        .filter(|&n| n > 0)
    {
        let today_start = today_start_utc_rfc3339();
        let used =
            credential_usage::tokens_since_for_user(db, cred.id, user_id, &today_start).await?;
        if used >= token_cap {
            debug!(
                cred = %cred.id, %user_id, used, token_cap,
                "per-user daily token cap exhausted"
            );
            return Ok(PerUserVerdict::DenyTokens);
        }
    }

    if let Some(rpm_cap) = per_user
        .get("max_requests_per_minute")
        .and_then(serde_json::Value::as_i64)
        .filter(|&n| n > 0)
    {
        let one_minute_ago = minute_ago_utc_rfc3339();
        let recent =
            credential_usage::requests_since_for_user(db, cred.id, user_id, &one_minute_ago)
                .await?;
        if recent >= rpm_cap {
            debug!(
                cred = %cred.id, %user_id, recent, rpm_cap,
                "per-user RPM cap exhausted"
            );
            return Ok(PerUserVerdict::DenyRequests);
        }
    }

    Ok(PerUserVerdict::Allowed)
}

/// "Today" boundary in the same RFC 3339 / UTC format as `created_at` columns,
/// so lexicographic comparison in the SQL `WHERE created_at >= ?` matches.
fn today_start_utc_rfc3339() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT00:00:00.000Z")
        .to_string()
}

/// 60-second-ago anchor, same format as above. The clock skew between
/// the gateway's `now()` and SQLite's `strftime('now')` is sub-second
/// in practice; a one-minute window is wide enough that this doesn't
/// matter operationally.
fn minute_ago_utc_rfc3339() -> String {
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
    use havn_db::repo::credential_usage::{NewUsage, record as record_usage};
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
    async fn unlimited_when_no_cap_or_zero_cap() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id;
        let no_cap = make_cred(&pool, owner, serde_json::json!({})).await;
        assert!(
            daily_tokens_remaining(&pool, &no_cap)
                .await
                .expect("query")
                .is_none()
        );
        let zero_cap = make_cred(&pool, owner, serde_json::json!({"max_tokens_per_day": 0})).await;
        assert!(
            daily_tokens_remaining(&pool, &zero_cap)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn per_user_token_cap_exhausts_after_threshold() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id;
        // Per-user cap of 100 tokens/day, no overall cap.
        let cred = make_cred(
            &pool,
            owner,
            serde_json::json!({
                "per_user": { "max_tokens_per_day": 100 }
            }),
        )
        .await;

        // No usage yet → allowed.
        let v = per_user_ceilings_remaining(&pool, &cred, owner)
            .await
            .expect("query");
        assert_eq!(v, PerUserVerdict::Allowed);

        // Burn 100 tokens → exhausted.
        record_usage(
            &pool,
            NewUsage {
                credential_id: cred.id,
                user_id: owner,
                agent_id: None,
                provider: "anthropic",
                model: "claude-sonnet-4-6",
                tokens_in: 60,
                tokens_out: 40,
            },
        )
        .await
        .expect("usage");
        let v = per_user_ceilings_remaining(&pool, &cred, owner)
            .await
            .expect("query");
        assert_eq!(v, PerUserVerdict::DenyTokens);
    }

    #[tokio::test]
    async fn per_user_rpm_cap_blocks_after_threshold() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = create_user(&pool, NewUser { display_name: "u" })
            .await
            .expect("user")
            .id;
        let cred = make_cred(
            &pool,
            owner,
            serde_json::json!({
                "per_user": { "max_requests_per_minute": 2 }
            }),
        )
        .await;

        for _ in 0..2 {
            record_usage(
                &pool,
                NewUsage {
                    credential_id: cred.id,
                    user_id: owner,
                    agent_id: None,
                    provider: "anthropic",
                    model: "claude-sonnet-4-6",
                    tokens_in: 1,
                    tokens_out: 1,
                },
            )
            .await
            .expect("usage");
        }
        let v = per_user_ceilings_remaining(&pool, &cred, owner)
            .await
            .expect("query");
        assert_eq!(v, PerUserVerdict::DenyRequests);
    }
}
