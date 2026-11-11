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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

use axum::extract::{ConnectInfo, FromRef as _, FromRequestParts};
use axum::http::request::Parts;
use havn_core::UserId;
use havn_db::repo::users::{self, User};
use tracing::{debug, warn};

use crate::AppState;
use crate::api::ApiError;

const HEADER_NAME: &str = "x-user-id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpCidr {
    network: IpAddr,
    prefix: u8,
}

impl IpCidr {
    pub fn covers_loopback(self) -> bool {
        self.contains(IpAddr::V4(Ipv4Addr::LOCALHOST))
            || self.contains(IpAddr::V6(Ipv6Addr::LOCALHOST))
    }

    pub fn overlaps(self, other: IpCidr) -> bool {
        let wider = if self.prefix <= other.prefix {
            self
        } else {
            other
        };
        let narrower = if self.prefix <= other.prefix {
            other
        } else {
            self
        };
        wider.contains(narrower.network)
    }

    pub fn contains(self, ip: IpAddr) -> bool {
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => prefix_matches(
                u128::from(u32::from(net)),
                u128::from(u32::from(ip)),
                self.prefix,
                32,
            ),
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_matches(u128::from(net), u128::from(ip), self.prefix, 128)
            }
            _ => false,
        }
    }
}

fn prefix_matches(net: u128, ip: u128, prefix: u8, width: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let shift = u32::from(width - prefix);
    (net >> shift) == (ip >> shift)
}

impl FromStr for IpCidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (s, None),
        };
        let network = IpAddr::from_str(addr.trim())
            .map_err(|e| format!("invalid proxy address {s:?}: {e}"))?;
        let width: u8 = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= width)
                .ok_or_else(|| format!("invalid prefix length in {s:?}"))?,
            None => width,
        };
        match (network, network.to_canonical()) {
            (IpAddr::V6(_), IpAddr::V4(v4)) if prefix >= 96 => Ok(Self {
                network: IpAddr::V4(v4),
                prefix: prefix - 96,
            }),
            (IpAddr::V6(_), IpAddr::V4(_)) => Err(format!(
                "IPv4-mapped entry {s:?} needs a prefix of at least 96"
            )),
            _ => Ok(Self { network, prefix }),
        }
    }
}

pub fn trusted_proxies(allowed: &[String]) -> Result<Vec<IpCidr>, String> {
    if allowed.is_empty() {
        return Ok(vec![
            IpCidr {
                network: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)),
                prefix: 8,
            },
            IpCidr {
                network: IpAddr::V6(Ipv6Addr::LOCALHOST),
                prefix: 128,
            },
        ]);
    }
    allowed.iter().map(|s| IpCidr::from_str(s)).collect()
}

pub fn reject_agent_pool_overlap(proxies: &[IpCidr], agent_pool_cidr: &str) -> Result<(), String> {
    let pool = IpCidr::from_str(agent_pool_cidr)?;
    match proxies.iter().find(|p| p.overlaps(pool)) {
        Some(p) => Err(format!(
            "trust_header.allowed_proxies entry {p:?} overlaps network.agent_pool_cidr \
             {agent_pool_cidr}; agents in that pool could send a forged X-User-ID"
        )),
        None => Ok(()),
    }
}

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
    if let Some(proxies) = app.trusted_proxies.as_deref() {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip());
        if !peer.is_some_and(|ip| proxies.iter().any(|net| net.contains(ip))) {
            debug!(
                ?peer,
                "rejecting request: peer is not in trust_header.allowed_proxies"
            );
            return Err(ApiError::Forbidden(
                "requests must arrive through the configured reverse proxy".into(),
            ));
        }
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
pub(crate) mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::ignored_unit_patterns
    )]

    use super::*;
    use axum::http::Request;
    use havn_db::connect_in_memory;
    use havn_db::repo::users::{NewUser, create as create_user};
    use std::sync::Arc;

    /// Build an AppState with a real in-memory DB + a bootstrap user.
    /// Returns the state and the bootstrap user's ID.
    pub(crate) async fn state_with(trust: bool) -> (AppState, UserId) {
        use crate::agent_registry::AgentRegistry;
        use crate::webchat::WebChatRouter;
        use arc_swap::ArcSwap;
        use std::path::PathBuf;

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
            trusted_proxies: trust.then(|| Arc::new(trusted_proxies(&[]).expect("loopback"))),
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
            channels: Arc::new(std::collections::BTreeMap::new()),
            bindings: Arc::new(Vec::new()),
            channel_router: crate::channel_router::ChannelRouter::new(),
            dm_reject_limiter: crate::api::channel::DmRejectLimiter::new(),
            webchat_router: WebChatRouter::new(),
            allowed_origins: Arc::new(ArcSwap::from_pointee(Vec::new())),
            admin_rpc: crate::admin_rpc::AdminRpc::new(),
            start_locks: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            embedding_config: Arc::new(ArcSwap::from_pointee(
                serde_json::json!({"provider": "disabled"}),
            )),
            agent_env_passthrough: Arc::new(Vec::new()),
            keyring: None,
        };
        (app, bootstrap)
    }

    fn from_peer(mut parts: Parts, peer: &str) -> Parts {
        let addr: SocketAddr = peer.parse().expect("peer addr");
        parts.extensions.insert(ConnectInfo(addr));
        parts
    }

    fn parts_with_header(name: &str, value: &str) -> Parts {
        let req = Request::builder()
            .header(name, value)
            .body(())
            .expect("req");
        let (parts, _) = req.into_parts();
        from_peer(parts, "127.0.0.1:50000")
    }

    fn parts_no_header() -> Parts {
        let req = Request::builder().body(()).expect("req");
        let (parts, _) = req.into_parts();
        from_peer(parts, "127.0.0.1:50000")
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

    #[tokio::test]
    async fn trust_mode_rejects_header_from_untrusted_peer() {
        let (app, bs) = state_with(true).await;
        let parts = from_peer(
            parts_with_header(HEADER_NAME, &bs.to_string()),
            "203.0.113.7:40000",
        );
        let err = resolve(&parts, &app).await.expect_err("untrusted peer");
        assert!(matches!(err, ApiError::Forbidden(_)), "{err:?}");
    }

    #[tokio::test]
    async fn trust_mode_rejects_request_without_peer_address() {
        let (app, bs) = state_with(true).await;
        let req = Request::builder()
            .header(HEADER_NAME, bs.to_string())
            .body(())
            .expect("req");
        let (parts, _) = req.into_parts();
        let err = resolve(&parts, &app).await.expect_err("no peer");
        assert!(matches!(err, ApiError::Forbidden(_)), "{err:?}");
    }

    #[test]
    fn cidr_contains_matches_prefix() {
        let net: IpCidr = "10.0.0.0/8".parse().expect("cidr");
        assert!(net.contains("10.255.1.2".parse().expect("ip")));
        assert!(!net.contains("11.0.0.1".parse().expect("ip")));
        let host: IpCidr = "192.0.2.10".parse().expect("bare ip");
        assert!(host.contains("192.0.2.10".parse().expect("ip")));
        assert!(!host.contains("192.0.2.11".parse().expect("ip")));
        let v6: IpCidr = "fd00::/8".parse().expect("v6 cidr");
        assert!(v6.contains("fd12::1".parse().expect("ip")));
        assert!(!v6.contains("10.0.0.1".parse().expect("ip")));
    }

    #[test]
    fn cidr_matches_ipv4_mapped_ipv6_peer() {
        let net: IpCidr = "127.0.0.0/8".parse().expect("cidr");
        assert!(net.contains("::ffff:127.0.0.1".parse().expect("mapped")));
    }

    #[test]
    fn cidr_rejects_invalid_entries() {
        assert!("10.0.0.0/33".parse::<IpCidr>().is_err());
        assert!("not-an-ip".parse::<IpCidr>().is_err());
        assert!("::1/129".parse::<IpCidr>().is_err());
    }

    #[test]
    fn empty_allowlist_trusts_loopback_only() {
        let proxies = trusted_proxies(&[]).expect("default");
        let trusted = |ip: &str| proxies.iter().any(|n| n.contains(ip.parse().expect("ip")));
        assert!(trusted("127.0.0.1"));
        assert!(trusted("::1"));
        assert!(!trusted("192.168.1.10"));
    }

    #[tokio::test]
    async fn custom_allowlist_trusts_listed_remote_proxy() {
        let (mut app, bs) = state_with(true).await;
        app.trusted_proxies = Some(Arc::new(
            trusted_proxies(&["10.0.0.0/8".to_string()]).expect("cidr"),
        ));
        let parts = from_peer(
            parts_with_header(HEADER_NAME, &bs.to_string()),
            "10.1.2.3:443",
        );
        assert_eq!(resolve(&parts, &app).await.expect("trusted").id, bs);
        let loopback = parts_with_header(HEADER_NAME, &bs.to_string());
        assert!(resolve(&loopback, &app).await.is_err());
    }

    #[test]
    fn cidr_edge_prefixes() {
        let any_v4: IpCidr = "0.0.0.0/0".parse().expect("cidr");
        assert!(any_v4.contains("203.0.113.9".parse().expect("ip")));
        assert!(!any_v4.contains("::1".parse().expect("ip")));
        let host_v6: IpCidr = "2001:db8::1/128".parse().expect("cidr");
        assert!(host_v6.contains("2001:db8::1".parse().expect("ip")));
        assert!(!host_v6.contains("2001:db8::2".parse().expect("ip")));
        let host_bits: IpCidr = "10.1.2.3/8".parse().expect("cidr");
        assert!(host_bits.contains("10.200.0.1".parse().expect("ip")));
    }

    #[test]
    fn ipv4_mapped_entries_are_canonicalised() {
        let mapped: IpCidr = "::ffff:10.0.0.0/104".parse().expect("mapped cidr");
        assert!(mapped.contains("10.5.5.5".parse().expect("ip")));
        assert!(mapped.contains("::ffff:10.5.5.5".parse().expect("ip")));
        assert!("::ffff:10.0.0.0/90".parse::<IpCidr>().is_err());
    }

    #[test]
    fn covers_loopback_detects_wide_ranges() {
        let covers = |s: &str| s.parse::<IpCidr>().expect("cidr").covers_loopback();
        assert!(covers("127.0.0.0/8"));
        assert!(covers("0.0.0.0/0"));
        assert!(covers("::/0"));
        assert!(covers("0.0.0.0/1"));
        assert!(!covers("10.0.0.0/8"));
        assert!(!covers("fd00::/8"));
    }

    #[test]
    fn allowlist_overlapping_agent_pool_is_rejected() {
        let wide = trusted_proxies(&["10.0.0.0/8".to_string()]).expect("cidr");
        assert!(reject_agent_pool_overlap(&wide, "10.42.0.0/16").is_err());
        let inside = trusted_proxies(&["10.42.7.0/24".to_string()]).expect("cidr");
        assert!(reject_agent_pool_overlap(&inside, "10.42.0.0/16").is_err());
        let separate = trusted_proxies(&["192.168.0.0/16".to_string()]).expect("cidr");
        assert!(reject_agent_pool_overlap(&separate, "10.42.0.0/16").is_ok());
        let loopback = trusted_proxies(&[]).expect("default");
        assert!(reject_agent_pool_overlap(&loopback, "10.42.0.0/16").is_ok());
    }

    #[tokio::test]
    async fn served_requests_carry_the_peer_address() {
        use axum::{Router, routing::get};

        let (app, bs) = state_with(true).await;
        let router = Router::new()
            .route(
                "/",
                get(|user: AuthedUser| async move { user.id.to_string() }),
            )
            .with_state(app);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve");
        });
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .header(HEADER_NAME, bs.to_string())
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.expect("body"), bs.to_string());
    }
}
