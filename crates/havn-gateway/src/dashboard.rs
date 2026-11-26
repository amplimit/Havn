//! The dashboard (issue #259): `apps/dashboard` exported by `next build`
//! and embedded in the binary, served on `dashboard_listen` with the
//! management API under `/api` and the web chat under `/ws`, on one origin.
//!
//! A gateway built before the export exists serves a page saying so; in
//! development the dashboard runs from source instead (`npm run dev`).
//!
//! Without `trust_header` only a loopback name, or an origin in
//! `allowed_origins`, may be the request's `Host`, here and for the
//! management API on `listen`: a page of another site whose name resolves
//! to 127.0.0.1 (DNS rebinding) is refused rather than served the
//! bootstrap user's API.

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Extension, Json};
use rust_embed::Embed;
use tower_http::trace::TraceLayer;

use crate::AppState;

#[derive(Embed)]
#[folder = "../../apps/dashboard/out/"]
#[allow_missing = true]
struct Files;

/// Marks requests that reached the dashboard's listener, whose own origin
/// the web chat accepts besides `allowed_origins`.
#[derive(Debug, Clone, Copy)]
pub struct SameOrigin;

const NOT_BUILT: &str = "This gateway was built without the dashboard. Build it first \
                         (`npm ci && npm run build` in apps/dashboard), then the gateway; or run \
                         the dashboard from source with `npm run dev`.";

pub fn router(api: Router<AppState>, state: AppState) -> Router {
    Router::new()
        .nest("/api", api.fallback(|| async { StatusCode::NOT_FOUND }))
        .route("/ws/chat/{agent_id}", get(crate::api::webchat_ws::handler))
        .fallback(serve)
        .layer(middleware::from_fn_with_state(state.clone(), named_host))
        .layer(Extension(SameOrigin))
        .with_state(state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(request_span as fn(&Request) -> tracing::Span),
        )
}

/// A request as the log names it: its method and path, never its query,
/// which can hold an OAuth code (`/api/mcp/oauth/callback?code=...`).
pub fn request_span(request: &Request) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %request.method(),
        path = %request.uri().path(),
        version = ?request.version(),
    )
}

/// Whether `origin` is the page's own: `http://` or `https://` and the
/// request's `Host`.
pub fn same_origin(origin: &str, headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    ["http://", "https://"]
        .iter()
        .any(|scheme| origin.strip_prefix(scheme) == Some(host))
}

pub async fn named_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if state.trusted_proxies.is_some() {
        return next.run(request).await;
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let listed = state
        .allowed_origins
        .load()
        .iter()
        .any(|origin| same_origin(origin, request.headers()));
    if loopback_host(host) || listed {
        return next.run(request).await;
    }
    tracing::warn!(host, "refusing a request for another host name");
    (
        StatusCode::MISDIRECTED_REQUEST,
        Json(serde_json::json!({
            "error": "address the gateway as localhost or 127.0.0.1 (an SSH tunnel), \
                      or add this origin to allowed_origins"
        })),
    )
        .into_response()
}

fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(name, _)| name),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    if path
        .split('/')
        .any(|part| part == ".." || part.contains('\\'))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let names = if path.is_empty() {
        vec!["index.html".to_string()]
    } else {
        vec![
            path.to_string(),
            format!("{path}.html"),
            format!("{path}/index.html"),
        ]
    };
    for name in names {
        if let Some(file) = Files::get(&name) {
            return file_response(&name, file, StatusCode::OK);
        }
    }
    if let Some(to) = moved(uri.path(), uri.query()) {
        return Redirect::permanent(&to).into_response();
    }
    match Files::get("404.html") {
        Some(file) => file_response("404.html", file, StatusCode::NOT_FOUND),
        None if Files::iter().next().is_none() => {
            (StatusCode::NOT_FOUND, NOT_BUILT).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn file_response(name: &str, file: rust_embed::EmbeddedFile, status: StatusCode) -> Response {
    // Names under _next/static carry a hash of their content.
    let cache = if name.starts_with("_next/static/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = Response::new(Body::from(file.data.into_owned()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Ok(kind) = HeaderValue::from_str(file.metadata.mimetype()) {
        headers.insert(header::CONTENT_TYPE, kind);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// Where a page of the dashboard before issue #259 is now:
/// `/agents/<id>[/<tab>]` and `/teams/<id>[/<tab>]` became
/// `/agent[/<tab>]?id=<id>` and `/team[/<tab>]?id=<id>`.
fn moved(path: &str, query: Option<&str>) -> Option<String> {
    let (base, rest) = if let Some(rest) = path.strip_prefix("/agents/") {
        ("/agent", rest)
    } else {
        ("/team", path.strip_prefix("/teams/")?)
    };
    let (id, tab) = rest.split_once('/').unwrap_or((rest, ""));
    let word = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if id.is_empty() || id == "new" || !word(id) || !word(tab) {
        return None;
    }
    let mut to = String::from(base);
    if !tab.is_empty() {
        to.push('/');
        to.push_str(tab);
    }
    to.push_str("?id=");
    to.push_str(id);
    if let Some(query) = query.filter(|q| !q.is_empty()) {
        to.push('&');
        to.push_str(query);
    }
    Some(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_pages_move_to_their_address() {
        let id = "01a1d3a5-0000-4000-8000-000000000000";
        assert_eq!(
            moved(&format!("/agents/{id}"), None).as_deref(),
            Some(format!("/agent?id={id}").as_str())
        );
        assert_eq!(
            moved(&format!("/agents/{id}/workspace"), Some("path=a%2Fb")).as_deref(),
            Some(format!("/agent/workspace?id={id}&path=a%2Fb").as_str())
        );
        assert_eq!(
            moved(&format!("/teams/{id}/audit"), None).as_deref(),
            Some(format!("/team/audit?id={id}").as_str())
        );
        assert_eq!(moved("/agents/new", Some("team=x")), None);
        assert_eq!(moved("/agents/", None), None);
        assert_eq!(moved("/teams/__next._full.txt", None), None);
        assert_eq!(moved(&format!("/agents/{id}/a/b"), None), None);
        assert_eq!(moved("/models", None), None);
    }

    #[test]
    fn only_loopback_names_are_local() {
        for host in [
            "localhost",
            "localhost:3000",
            "127.0.0.1:3000",
            "127.1.2.3",
            "[::1]:3000",
            "[::1]",
        ] {
            assert!(loopback_host(host), "{host}");
        }
        for host in [
            "",
            "example.com",
            "127.0.0.1.evil.com:3000",
            "10.0.0.1:3000",
            "[::2]:3000",
        ] {
            assert!(!loopback_host(host), "{host}");
        }
    }

    #[tokio::test]
    async fn other_host_names_are_refused_without_trust_header() {
        use tower::ServiceExt as _;

        let call = |app: Router, host: &'static str| async move {
            let request = Request::builder()
                .uri("/api/healthz")
                .header(header::HOST, host)
                .body(Body::empty())
                .expect("request");
            app.oneshot(request).await.expect("response").status()
        };
        let api = Router::new().route("/healthz", get(|| async { "ok" }));
        let (single, _) = crate::auth::tests::state_with(false).await;
        single
            .allowed_origins
            .store(std::sync::Arc::new(vec!["https://havn.example.com".into()]));
        let app = router(api.clone(), single);
        assert_eq!(call(app.clone(), "127.0.0.1:3000").await, StatusCode::OK);
        assert_eq!(call(app.clone(), "localhost:3000").await, StatusCode::OK);
        assert_eq!(call(app.clone(), "havn.example.com").await, StatusCode::OK);
        assert_eq!(
            call(app.clone(), "rebound.example.com:3000").await,
            StatusCode::MISDIRECTED_REQUEST
        );
        let (multi, _) = crate::auth::tests::state_with(true).await;
        let app = router(api, multi);
        assert_eq!(call(app, "sso.example.com").await, StatusCode::OK);
    }

    #[test]
    fn same_origin_is_scheme_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("havn.example.com"));
        assert!(same_origin("https://havn.example.com", &headers));
        assert!(same_origin("http://havn.example.com", &headers));
        assert!(!same_origin("https://evil.example.com", &headers));
        assert!(!same_origin("https://havn.example.com:8443", &headers));
        assert!(!same_origin("https://havn.example.com", &HeaderMap::new()));
    }
}
