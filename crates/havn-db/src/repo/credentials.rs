//! Credentials repository.
//!
//! The on-disk SQL column is still `api_key BLOB` (renaming would
//! require a migration for no observable benefit), but the in-memory
//! field is now `api_key_ciphertext` — a deliberate type-level
//! reminder that **these bytes cannot be passed to a provider as-is**.
//! Decryption is the gateway's job (`gateway::keyring::KeyRing::decrypt`)
//! and happens at the single read site in `llm_proxy`.
//!
//! Spec §13 Phase 3: encryption-at-rest landed via age symmetric
//! passphrase mode, with the operator-owned `HAVN_AGE_KEY` env var.
//! The DB layer is intentionally crypto-agnostic — the repo just
//! stores opaque bytes; gateway decides what's in them.

use chrono::{DateTime, Utc};
use havn_core::CredentialId;
use sqlx::SqlitePool;

use crate::repo::parse_db_uuid;
use crate::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialScope {
    User,
    Team,
}

impl CredentialScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Team => "team",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "user" => Ok(Self::User),
            "team" => Ok(Self::Team),
            other => Err(DbError::InvalidValue {
                column: "credentials.scope",
                message: format!("unknown scope {other:?}"),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub id: CredentialId,
    pub scope: CredentialScope,
    pub scope_id: String,
    pub provider: String,
    /// Operator-supplied handle (spec §7.3). `None` = legacy v0.1 row
    /// addressable only via the priority-chain fallback. `Some(...)` =
    /// v0.2 named row addressable as `(scope, scope_id, provider, name)`
    /// — used by channel adapter tokens and OAuth2 SaaS credentials.
    pub name: Option<String>,
    /// Opaque ciphertext bytes. Decrypt with the gateway `KeyRing`
    /// before passing to a provider. Pre-Phase-3 rows may still hold
    /// plaintext bytes here until the gateway's startup migration
    /// runs once on this database.
    pub api_key_ciphertext: Vec<u8>,
    pub priority: i32,
    pub limits: serde_json::Value,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewCredential<'a> {
    pub scope: CredentialScope,
    pub scope_id: &'a str,
    pub provider: &'a str,
    /// `None` for v0.1 priority-chain LLM rows; `Some(...)` for v0.2
    /// handle-addressable rows. The DB enforces uniqueness of named
    /// rows per (scope, scope_id, provider) via a partial unique index
    /// (see migration 0006).
    pub name: Option<&'a str>,
    /// Already-encrypted bytes. The gateway encrypts before calling
    /// `create`; the DB layer never sees plaintext.
    pub api_key_ciphertext: &'a [u8],
    pub priority: i32,
    pub limits: serde_json::Value,
}

pub async fn create(pool: &SqlitePool, new: NewCredential<'_>) -> Result<Credential> {
    let id = CredentialId::new();
    // App-layer guard for empty-string `name`. SQLite can't enforce
    // `CHECK (name IS NULL OR length(name) > 0)` after ALTER TABLE
    // ADD COLUMN without rebuilding the table, so this guard is the
    // canonical line of defence against a config typo
    // (`adapter_token_ref = "secret:channel:telegram:"` parses to
    // `name = ""`). Empty `name=""` would otherwise satisfy the
    // partial unique index and `find_by_handle("")` would match.
    if matches!(new.name, Some("")) {
        return Err(DbError::InvalidValue {
            column: "credentials.name",
            message:
                "empty string is not a valid handle; use None for unnamed (priority-chain) rows"
                    .into(),
        });
    }
    let limits = serde_json::to_string(&new.limits).map_err(|e| DbError::InvalidValue {
        column: "credentials.limits",
        message: e.to_string(),
    })?;
    sqlx::query(
        "INSERT INTO credentials (id, scope, scope_id, provider, name, api_key, priority, limits) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(id.to_string())
    .bind(new.scope.as_str())
    .bind(new.scope_id)
    .bind(new.provider)
    .bind(new.name)
    .bind(new.api_key_ciphertext)
    .bind(new.priority)
    .bind(limits)
    .execute(pool)
    .await?;

    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn find_by_id(pool: &SqlitePool, id: CredentialId) -> Result<Option<Credential>> {
    let row: Option<CredentialRow> = sqlx::query_as::<_, CredentialRow>(
        "SELECT id, scope, scope_id, provider, name, api_key, priority, limits, enabled, created_at \
         FROM credentials WHERE id = ?1",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;

    row.map(Credential::try_from).transpose()
}

/// Look up a credential by its v0.2 handle: (scope, scope_id, provider, name).
/// Used by callers that hold a `secret:<provider>:<name>` reference string —
/// channel adapter token resolution at WS upgrade, OAuth2 token reads from
/// SaaS pack tools. Returns `None` when no row matches the exact handle.
///
/// Differs from `list_active_for_provider` (the LLM fallback chain): that one
/// returns ALL rows for a provider in priority order; this one returns at
/// most ONE row by full handle. The partial unique index on
/// (scope, scope_id, provider, name WHERE name IS NOT NULL) ensures the
/// "at most one" guarantee.
pub async fn find_by_handle(
    pool: &SqlitePool,
    scope: CredentialScope,
    scope_id: &str,
    provider: &str,
    name: &str,
) -> Result<Option<Credential>> {
    let row: Option<CredentialRow> = sqlx::query_as::<_, CredentialRow>(
        "SELECT id, scope, scope_id, provider, name, api_key, priority, limits, enabled, created_at \
         FROM credentials \
         WHERE scope = ?1 AND scope_id = ?2 AND provider = ?3 AND name = ?4",
    )
    .bind(scope.as_str())
    .bind(scope_id)
    .bind(provider)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    row.map(Credential::try_from).transpose()
}

/// List every credential for `(scope, scope_id)` (including disabled ones)
/// ordered by `(provider, priority desc, created_at)`. Used by the management API.
pub async fn list_for_scope(
    pool: &SqlitePool,
    scope: CredentialScope,
    scope_id: &str,
) -> Result<Vec<Credential>> {
    let rows: Vec<CredentialRow> = sqlx::query_as::<_, CredentialRow>(
        "SELECT id, scope, scope_id, provider, name, api_key, priority, limits, enabled, created_at \
         FROM credentials \
         WHERE scope = ?1 AND scope_id = ?2 \
         ORDER BY provider, priority DESC, created_at",
    )
    .bind(scope.as_str())
    .bind(scope_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(Credential::try_from).collect()
}

/// List enabled credentials for `(scope, scope_id, provider)` ordered by priority desc.
/// This is the priority chain consumed by the credential resolver (spec §7.2).
pub async fn list_active_for_provider(
    pool: &SqlitePool,
    scope: CredentialScope,
    scope_id: &str,
    provider: &str,
) -> Result<Vec<Credential>> {
    let rows: Vec<CredentialRow> = sqlx::query_as::<_, CredentialRow>(
        "SELECT id, scope, scope_id, provider, name, api_key, priority, limits, enabled, created_at \
         FROM credentials \
         WHERE scope = ?1 AND scope_id = ?2 AND provider = ?3 AND enabled = 1 \
         ORDER BY priority DESC, created_at",
    )
    .bind(scope.as_str())
    .bind(scope_id)
    .bind(provider)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(Credential::try_from).collect()
}

/// Count of every row in the table, regardless of scope or enabled
/// flag. Used by the gateway's startup boot guard to decide whether
/// missing `HAVN_AGE_KEY` is a fresh-install (allowed) or an
/// unmigrated-rows hazard (refuse).
pub async fn count_all(pool: &SqlitePool) -> Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credentials")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

pub async fn delete(pool: &SqlitePool, id: CredentialId) -> Result<()> {
    let result = sqlx::query("DELETE FROM credentials WHERE id = ?1")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct CredentialUpdate {
    pub priority: Option<i32>,
    pub limits: Option<serde_json::Value>,
    pub enabled: Option<bool>,
}

pub async fn update(
    pool: &SqlitePool,
    id: CredentialId,
    patch: CredentialUpdate,
) -> Result<Credential> {
    // SQLite has no convenient "partial update" syntax. Use COALESCE with bound NULLs
    // so the existing column is preserved when a field is `None`.
    let limits_json = patch
        .limits
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| DbError::InvalidValue {
            column: "credentials.limits",
            message: e.to_string(),
        })?;

    let result = sqlx::query(
        "UPDATE credentials \
         SET priority = COALESCE(?1, priority), \
             limits   = COALESCE(?2, limits),   \
             enabled  = COALESCE(?3, enabled)   \
         WHERE id = ?4",
    )
    .bind(patch.priority)
    .bind(limits_json)
    .bind(patch.enabled.map(i32::from))
    .bind(id.to_string())
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    find_by_id(pool, id).await?.ok_or(DbError::NotFound)
}

pub async fn set_enabled(pool: &SqlitePool, id: CredentialId, enabled: bool) -> Result<()> {
    let result = sqlx::query("UPDATE credentials SET enabled = ?1 WHERE id = ?2")
        .bind(i32::from(enabled))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct CredentialRow {
    id: String,
    scope: String,
    scope_id: String,
    provider: String,
    name: Option<String>,
    api_key: Vec<u8>,
    priority: i32,
    limits: String,
    enabled: i32,
    created_at: DateTime<Utc>,
}

impl TryFrom<CredentialRow> for Credential {
    type Error = DbError;
    fn try_from(r: CredentialRow) -> Result<Self> {
        let limits = serde_json::from_str(&r.limits).map_err(|e| DbError::InvalidValue {
            column: "credentials.limits",
            message: e.to_string(),
        })?;
        Ok(Self {
            id: CredentialId::from_uuid(parse_db_uuid(&r.id, "credentials.id")?),
            scope: CredentialScope::parse(&r.scope)?,
            scope_id: r.scope_id,
            provider: r.provider,
            name: r.name,
            api_key_ciphertext: r.api_key,
            priority: r.priority,
            limits,
            enabled: r.enabled != 0,
            created_at: r.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::connect_in_memory;
    use crate::repo::users::{NewUser, create as create_user};

    async fn user_id(pool: &SqlitePool, _disambiguator: &str) -> String {
        create_user(pool, NewUser { display_name: "t" })
            .await
            .expect("create user")
            .id
            .to_string()
    }

    #[tokio::test]
    async fn create_then_round_trip_columns() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = user_id(&pool, "u@e").await;
        let cred = create(
            &pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &owner,
                provider: "anthropic",
                name: None,
                api_key_ciphertext: b"sk-test-1234",
                priority: 10,
                limits: serde_json::json!({"max_tokens_per_day": 50_000}),
            },
        )
        .await
        .expect("create");

        // Catch column-binding mistakes: every field must round-trip exactly.
        assert_eq!(cred.scope, CredentialScope::User);
        assert_eq!(cred.scope_id, owner);
        assert_eq!(cred.provider, "anthropic");
        assert_eq!(cred.api_key_ciphertext, b"sk-test-1234");
        assert_eq!(cred.priority, 10);
        assert_eq!(cred.limits["max_tokens_per_day"], 50_000);
        assert!(cred.enabled);
    }

    #[tokio::test]
    async fn priority_chain_is_descending() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = user_id(&pool, "u@e").await;
        for (priority, key) in [(1i32, &b"low"[..]), (10, &b"high"[..]), (5, &b"mid"[..])] {
            create(
                &pool,
                NewCredential {
                    scope: CredentialScope::User,
                    scope_id: &owner,
                    provider: "anthropic",
                    name: None,
                    api_key_ciphertext: key,
                    priority,
                    limits: serde_json::json!({}),
                },
            )
            .await
            .expect("create");
        }

        let chain = list_active_for_provider(&pool, CredentialScope::User, &owner, "anthropic")
            .await
            .expect("list");
        let priorities: Vec<_> = chain.iter().map(|c| c.priority).collect();
        assert_eq!(priorities, vec![10, 5, 1]);
    }

    #[tokio::test]
    async fn disabled_credentials_are_excluded() {
        let pool = connect_in_memory().await.expect("connect");
        let owner = user_id(&pool, "u@e").await;
        let cred = create(
            &pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &owner,
                provider: "anthropic",
                name: None,
                api_key_ciphertext: b"k",
                priority: 1,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("create");

        set_enabled(&pool, cred.id, false).await.expect("disable");
        let chain = list_active_for_provider(&pool, CredentialScope::User, &owner, "anthropic")
            .await
            .expect("list");
        assert!(chain.is_empty());
    }

    #[tokio::test]
    async fn find_by_handle_returns_named_row() {
        // v0.2 channel adapter token lookup pattern: an account's adapter
        // token lives at (scope, scope_id, "channel:telegram", account_id).
        // find_by_handle is the resolution path for the `secret:<provider>:
        // <name>` config string format.
        let pool = connect_in_memory().await.expect("connect");
        let owner = user_id(&pool, "u@e").await;
        create(
            &pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &owner,
                provider: "channel:telegram",
                name: Some("alice-tg-bot"),
                api_key_ciphertext: b"adapter-token-bytes",
                priority: 0,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("create");

        let found = find_by_handle(
            &pool,
            CredentialScope::User,
            &owner,
            "channel:telegram",
            "alice-tg-bot",
        )
        .await
        .expect("lookup");
        let cred = found.expect("row should exist");
        assert_eq!(cred.provider, "channel:telegram");
        assert_eq!(cred.name.as_deref(), Some("alice-tg-bot"));
        assert_eq!(cred.api_key_ciphertext, b"adapter-token-bytes");

        // Wrong handle returns None — proves the tuple is unique-per-name.
        let missing = find_by_handle(
            &pool,
            CredentialScope::User,
            &owner,
            "channel:telegram",
            "bob-tg-bot",
        )
        .await
        .expect("lookup");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn empty_name_rejected_at_app_layer() {
        // Empty-string `name=""` is config-typo territory (operator
        // wrote `adapter_token_ref = "secret:channel:telegram:"` with
        // a missing name). The app layer must reject; SQLite can't
        // CHECK after ALTER ADD COLUMN.
        let pool = connect_in_memory().await.expect("connect");
        let owner = user_id(&pool, "u@e").await;
        let r = create(
            &pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &owner,
                provider: "channel:telegram",
                name: Some(""),
                api_key_ciphertext: b"x",
                priority: 0,
                limits: serde_json::json!({}),
            },
        )
        .await;
        assert!(matches!(
            r,
            Err(DbError::InvalidValue {
                column: "credentials.name",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn named_handle_unique_within_scope() {
        // Partial unique index on (scope, scope_id, provider, name) enforces
        // that an operator can't accidentally create two credentials with
        // the same handle. Unnamed rows (priority chain) remain unconstrained.
        let pool = connect_in_memory().await.expect("connect");
        let owner = user_id(&pool, "u@e").await;
        create(
            &pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &owner,
                provider: "channel:telegram",
                name: Some("alice-tg-bot"),
                api_key_ciphertext: b"v1",
                priority: 0,
                limits: serde_json::json!({}),
            },
        )
        .await
        .expect("first create");

        // Second create with the same handle MUST fail.
        let second = create(
            &pool,
            NewCredential {
                scope: CredentialScope::User,
                scope_id: &owner,
                provider: "channel:telegram",
                name: Some("alice-tg-bot"),
                api_key_ciphertext: b"v2",
                priority: 0,
                limits: serde_json::json!({}),
            },
        )
        .await;
        assert!(
            second.is_err(),
            "duplicate (scope, scope_id, provider, name) should be rejected"
        );

        // Unnamed rows for the same (scope, scope_id, provider) coexist
        // — that's the v0.1 LLM priority chain pattern.
        for key in [&b"k1"[..], &b"k2"[..]] {
            create(
                &pool,
                NewCredential {
                    scope: CredentialScope::User,
                    scope_id: &owner,
                    provider: "llm:anthropic",
                    name: None,
                    api_key_ciphertext: key,
                    priority: 0,
                    limits: serde_json::json!({}),
                },
            )
            .await
            .expect("unnamed coexist");
        }
    }
}
