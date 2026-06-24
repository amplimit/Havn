//! Identity resolution for incoming HTTP requests (spec §1.7).
//!
//! havn does no authentication. This module resolves the *authorisation
//! subject* of each request using two sources, in order:
//!
//! 1. **`X-User-ID` header** — when `gateway.trust_header.enabled` is
//!    true (configured for deployments behind a reverse proxy that
//!    handles login). The header value is looked up in the `users`
//!    table; unknown subjects get `403`.
//! 2. **Single-user loopback fallback** — when `trust_header` is off,
//!    the gateway is bound to loopback only and every request is
//!    attributed to the bootstrap user (`users::ensure_default`). This
//!    is the dev / single-operator default; if the gateway is bound
//!    non-loopback without `trust_header.enabled = true`, startup
//!    refuses (see [`crate::main`] guard).
//!
//! The resolver is exposed as an axum [`FromRequestParts`] extractor
//! so each handler declares `user: AuthedUser` and gets a typed,
//! already-validated subject. Replaces the v0.5 `state.current_user`
//! single-user assumption.

use std::str::FromStr as _;

use axum::extract::{FromRef as _, FromRequestParts};
use axum::http::request::Parts;
use havn_core::UserId;
use havn_db::repo::users::{self, User};
use tracing::warn;

use crate::AppState;
use crate::api::ApiError;

const HEADER_NAME: &str = "x-user-id";

/// Resolved subject of an incoming request. Handlers take this as an
/// extractor parameter to know "who is calling".
#[derive(Debug, Clone)]
pub struct AuthedUser {
    pub id: UserId,
    pub display_name: String,
}

impl From<User> for AuthedUser {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            display_name: u.display_name,
        }
    }
}

impl<S> FromRequestParts<S> for AuthedUser
where
    AppState: axum::extract::FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app: AppState = AppState::from_ref(state);
        resolve(parts, &app).await
    }
}

/// Plain function form so the WebSocket upgrade handler (which doesn't
/// use `FromRequestParts` because it owns its own parameter shape) can
/// share the same logic.
pub async fn resolve(parts: &Parts, app: &AppState) -> Result<AuthedUser, ApiError> {
    // Pass 1: trusted header.
    if app.trust_header_enabled {
        let raw = parts
            .headers
            .get(HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(raw) = raw {
            let id = UserId::from_str(raw).map_err(|_| {
                ApiError::BadRequest(format!("X-User-ID is not a valid UUID v7: {raw:?}"))
            })?;
            let user = users::find_by_id(&app.db, id).await?.ok_or_else(|| {
                // Don't leak whether the ID was malformed vs missing —
                // both routes return the same Forbidden so a probing
                // client can't enumerate the users table.
                ApiError::Forbidden(format!(
                    "no user provisioned for X-User-ID {id}; \
                         operator must run `havn user add {id} --display-name \"...\"` \
                         before this header is accepted"
                ))
            })?;
            return Ok(user.into());
        }
        // trust_header on but no header present: refuse rather than
        // silently fall back to bootstrap. A misconfigured proxy
        // forgetting to set the header would otherwise grant every
        // anonymous caller the operator's identity.
        return Err(ApiError::Forbidden(
            "X-User-ID header missing; trust_header is enabled and the upstream \
             proxy must forward identity (spec §1.7)"
                .into(),
        ));
    }

    // Pass 2: single-user loopback fallback.
    //
    // We don't verify the connection is actually loopback here — the
    // startup guard in main.rs refuses to bind non-loopback without
    // trust_header on. So if we reach this branch, we know the only
    // way a request got here is over loopback (or the operator
    // disabled the guard, which we can't second-guess at runtime).
    //
    // Heads-up: a stray X-User-ID header in single-user mode is
    // ignored on purpose (the proxy isn't trusted), but we log it
    // because it usually means the operator forgot to enable
    // trust_header on a deployment that needs it.
    if parts.headers.contains_key(HEADER_NAME) {
        warn!(
            "X-User-ID header present but trust_header is disabled — ignoring. \
             Set [gateway.trust_header] enabled = true if you're behind a proxy."
        );
    }
    let user = users::find_by_id(&app.db, app.bootstrap_user_id)
        .await?
        .ok_or_else(|| {
            ApiError::Internal(
                "single-user bootstrap row missing; database may be corrupted".into(),
            )
        })?;
    Ok(user.into())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::ignored_unit_patterns
    )]

    use super::*;
    use axum::http::Request;
    use havn_db::connect_in_memory;
    use havn_db::repo::users::{NewUser, create as create_user};

    /// Build an AppState with a real in-memory DB + a bootstrap user.
    /// Returns the state and the bootstrap user's ID.
    async fn state_with(trust: bool) -> (AppState, UserId) {
        use crate::agent_registry::AgentRegistry;
        use crate::webchat::WebChatRouter;
        use arc_swap::ArcSwap;
        use std::path::PathBuf;
        use std::sync::Arc;

        let pool = connect_in_memory().await.expect("db");
        let bootstrap = create_user(
            &pool,
            NewUser {
                display_name: "havn",
            },
        )
        .await
        .expect("bootstrap")
        .id;

        // Stub out the rest of AppState's plumbing — these tests never
        // touch them, but Rust's struct-init makes us produce values.
        let dummy_path = PathBuf::from("/tmp/havn-test-stub");
        let app = AppState {
            db: pool,
            bootstrap_user_id: bootstrap,
            trust_header_enabled: trust,
            http: reqwest::Client::new(),
            registry: AgentRegistry::new(),
            spawner: Arc::new(havn_spawner::subprocess::SubprocessSpawner::new()),
            workspace_root: dummy_path.clone(),
            runtime_binary: dummy_path.clone(),
            init_binary: None,
            agent_socket_path: dummy_path,
            extra_mounts: Vec::new(),
            tmpfs_mounts: Vec::new(),
            seccomp_allow_extra: Vec::new(),
            agent_dns: Vec::new(),
            channels: Arc::new(ArcSwap::from_pointee(std::collections::BTreeMap::new())),
            bindings: Arc::new(ArcSwap::from_pointee(Vec::new())),
            channel_router: crate::channel_router::ChannelRouter::new(),
            webchat_router: WebChatRouter::new(),
            allowed_origins: Arc::new(ArcSwap::from_pointee(Vec::new())),
            admin_rpc: crate::admin_rpc::AdminRpc::new(),
            start_locks: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            embedding_config: Arc::new(ArcSwap::from_pointee(
                serde_json::json!({"provider": "disabled"}),
            )),
            keyring: None,
        };
        (app, bootstrap)
    }

    fn parts_with_header(name: &str, value: &str) -> Parts {
        let req = Request::builder()
            .header(name, value)
            .body(())
            .expect("req");
        let (parts, _) = req.into_parts();
        parts
    }

    fn parts_no_header() -> Parts {
        let req = Request::builder().body(()).expect("req");
        let (parts, _) = req.into_parts();
        parts
    }

    #[tokio::test]
    async fn loopback_mode_returns_bootstrap_user() {
        let (app, bs) = state_with(false).await;
        let parts = parts_no_header();
        let resolved = resolve(&parts, &app).await.expect("ok");
        assert_eq!(resolved.id, bs);
    }

    #[tokio::test]
    async fn loopback_mode_ignores_x_user_id_with_warning() {
        // A stray header in loopback mode is ignored (the proxy isn't
        // trusted). The bootstrap user wins.
        let (app, bs) = state_with(false).await;
        let other = UserId::new();
        let parts = parts_with_header(HEADER_NAME, &other.to_string());
        let resolved = resolve(&parts, &app).await.expect("ok");
        assert_eq!(
            resolved.id, bs,
            "header must not override bootstrap in loopback mode"
        );
    }

    #[tokio::test]
    async fn trust_mode_requires_header() {
        let (app, _bs) = state_with(true).await;
        let parts = parts_no_header();
        let err = resolve(&parts, &app).await.expect_err("should fail");
        assert!(matches!(err, ApiError::Forbidden(_)), "{err:?}");
    }

    #[tokio::test]
    async fn trust_mode_rejects_unknown_user() {
        let (app, _bs) = state_with(true).await;
        let stranger = UserId::new();
        let parts = parts_with_header(HEADER_NAME, &stranger.to_string());
        let err = resolve(&parts, &app).await.expect_err("should fail");
        assert!(matches!(err, ApiError::Forbidden(_)), "{err:?}");
    }

    #[tokio::test]
    async fn trust_mode_resolves_known_user() {
        let (app, bs) = state_with(true).await;
        let parts = parts_with_header(HEADER_NAME, &bs.to_string());
        let resolved = resolve(&parts, &app).await.expect("ok");
        assert_eq!(resolved.id, bs);
    }

    #[tokio::test]
    async fn trust_mode_rejects_malformed_uuid() {
        let (app, _bs) = state_with(true).await;
        let parts = parts_with_header(HEADER_NAME, "not-a-uuid");
        let err = resolve(&parts, &app).await.expect_err("should fail");
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn trust_mode_trims_whitespace() {
        let (app, bs) = state_with(true).await;
        let value = format!("  {bs}  ");
        let parts = parts_with_header(HEADER_NAME, &value);
        let resolved = resolve(&parts, &app).await.expect("ok");
        assert_eq!(resolved.id, bs);
    }
}
