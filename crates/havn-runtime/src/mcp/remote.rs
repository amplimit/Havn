//! Remote MCP servers (issue #234): Streamable HTTP, and the older HTTP+SSE
//! transport, which rmcp has no client for any more. Every request goes
//! through the agent's egress: the gateway's egress proxy when its network
//! goes through it, so a server's host is let through, refused or asked
//! about like any other.
//!
//! A server's headers and bearer token may be `secret:<name>`, and an OAuth
//! server gets the access token its owner connected it with; both only for
//! a server the agent's own settings define, and an OAuth token only for the
//! URL it was issued for. The token is looked up for every request, so one
//! the gateway refreshed is used from the next request on.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use havn_core::{McpAuth, McpServerConfig, McpTransport, Policy};
use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rmcp::RoleClient;
use rmcp::ServiceExt as _;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::service::RunningService;
use rmcp::transport::Transport;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpClientTransport,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
    StreamableHttpResponseLimits,
};
use sse_stream::{Sse, SseStream};
use tokio::sync::mpsc;
use tracing::warn;

/// How long connecting to a server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the MCP handshake may take, and the HTTP+SSE endpoint event.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes an HTTP+SSE stream may send without completing an event.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// Server messages of an HTTP+SSE stream waiting to be read.
const QUEUE: usize = 64;
/// Redirects followed, on the server's own origin only.
const MAX_REDIRECTS: usize = 5;

/// Connect to the remote server `name`, do the MCP handshake. `own` says
/// whether the agent's own settings define it as it runs: only then are
/// its secrets resolved and its OAuth token sent.
pub async fn connect(
    name: &str,
    cfg: &McpServerConfig,
    policy: &Policy,
    own: bool,
) -> anyhow::Result<RunningService<RoleClient, ()>> {
    let (_, host) = havn_core::policy::remote_url(&cfg.url).map_err(anyhow::Error::msg)?;
    if !(policy.permissions.can_access_network && policy.network_policy.egress_allowed) {
        anyhow::bail!("it is remote, and this agent's policy allows no network access");
    }
    if policy.network_policy.is_blocked(&host) {
        anyhow::bail!("{host} is on this agent's blocked domains");
    }
    if cfg.uses_secrets() && !own {
        anyhow::bail!(
            "it refers to the owner's secrets or OAuth tokens, and it runs with settings the \
             owner's role supplies, not the agent's own, so it does not get them; the owner can \
             define the server in the agent's MCP settings"
        );
    }
    let auth = match &cfg.auth {
        McpAuth::None => Authorization::None,
        McpAuth::Bearer { token } => Authorization::Static(
            crate::secrets::resolve(token).map_err(|e| anyhow::anyhow!("auth.token: {e}"))?,
        ),
        McpAuth::Oauth(_) => {
            if crate::secrets::mcp_token(name, &cfg.url).is_none() {
                anyhow::bail!(
                    "it is not connected: its owner connects it on the agent's settings page \
                     (again after its URL changed), then restarts the agent"
                );
            }
            Authorization::OAuth {
                server: name.to_string(),
                url: cfg.url.clone(),
            }
        }
    };
    let mut headers = HashMap::new();
    for (key, value) in &cfg.headers {
        let resolved =
            crate::secrets::resolve(value).map_err(|e| anyhow::anyhow!("header {key}: {e}"))?;
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| anyhow::anyhow!("header {key}: {e}"))?;
        let mut value = HeaderValue::from_str(&resolved)
            .map_err(|_| anyhow::anyhow!("header {key}: not a header value"))?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    let client = Client {
        http: http_client(&cfg.url)?,
        auth: Arc::new(auth),
    };
    match cfg.transport {
        McpTransport::StreamableHttp => streamable(client, &cfg.url, headers).await,
        McpTransport::Sse => sse(client, &cfg.url, headers).await,
        McpTransport::Auto => {
            match streamable(client.clone(), &cfg.url, headers.clone()).await {
                Ok(service) => Ok(service),
                // A server of the older transport answers the POST with a
                // 4xx; MCP's backwards compatibility tries its event stream.
                Err(first) => sse(client, &cfg.url, headers).await.map_err(|second| {
                    anyhow::anyhow!("Streamable HTTP: {first:#}; HTTP+SSE: {second:#}")
                }),
            }
        }
    }
}

async fn streamable(
    client: Client,
    url: &str,
    headers: HashMap<HeaderName, HeaderValue>,
) -> anyhow::Result<RunningService<RoleClient, ()>> {
    let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
    let transport = StreamableHttpClientTransport::with_client(client, config);
    let service = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| anyhow::anyhow!("no MCP handshake in {}s", HANDSHAKE_TIMEOUT.as_secs()))??;
    Ok(service)
}

async fn sse(
    client: Client,
    url: &str,
    headers: HashMap<HeaderName, HeaderValue>,
) -> anyhow::Result<RunningService<RoleClient, ()>> {
    let headers: HeaderMap = headers.into_iter().collect();
    let transport =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, SseTransport::open(client, url, headers))
            .await
            .map_err(|_| {
                anyhow::anyhow!("no endpoint event in {}s", HANDSHAKE_TIMEOUT.as_secs())
            })??;
    let service = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| anyhow::anyhow!("no MCP handshake in {}s", HANDSHAKE_TIMEOUT.as_secs()))??;
    Ok(service)
}

/// The HTTP client of a remote server: through the egress proxy when the
/// agent's network goes through it, never through proxies of the
/// environment, following redirects on the server's own origin only.
fn http_client(url: &str) -> anyhow::Result<reqwest::Client> {
    let origin = Url::parse(url)?.origin();
    let redirects = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if attempt.url().origin() == origin {
            attempt.follow()
        } else {
            attempt.error("a redirect to another origin")
        }
    });
    let builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .no_proxy()
        .redirect(redirects);
    #[cfg(target_os = "linux")]
    let builder = match crate::sandbox::tool_proxy() {
        Some(proxy) => builder.proxy(reqwest::Proxy::all(proxy)?),
        None => builder,
    };
    Ok(builder.build()?)
}

/// What a remote server is authorized with.
#[derive(Debug)]
enum Authorization {
    None,
    /// A bearer token, resolved when the server started.
    Static(String),
    /// The OAuth access token the runtime holds now for `server`, issued
    /// for `url`.
    OAuth {
        server: String,
        url: String,
    },
}

impl Authorization {
    fn token(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Static(token) => Some(token.clone()),
            Self::OAuth { server, url } => crate::secrets::mcp_token(server, url),
        }
    }
}

/// rmcp's reqwest client with the server's authorization, as it is at each
/// request.
#[derive(Debug, Clone)]
struct Client {
    http: reqwest::Client,
    auth: Arc<Authorization>,
}

impl StreamableHttpClient for Client {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.http
            .post_message(uri, message, session_id, self.auth.token(), custom_headers)
            .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.http
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                self.auth.token(),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn post_message_with_response_limits(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        limits: StreamableHttpResponseLimits,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.http
            .post_message_with_response_limits(
                uri,
                message,
                session_id,
                self.auth.token(),
                custom_headers,
                limits,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.http
            .delete_session(uri, session_id, self.auth.token(), custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.http
            .get_stream(
                uri,
                session_id,
                last_event_id,
                self.auth.token(),
                custom_headers,
            )
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.http
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                self.auth.token(),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}

/// The HTTP+SSE transport of MCP 2024-11-05: a GET opens the server's event
/// stream, whose `endpoint` event names where to POST messages, and the
/// server's messages arrive as its `message` events.
struct SseTransport {
    incoming: mpsc::Receiver<ServerJsonRpcMessage>,
    poster: Poster,
    reader: tokio::task::JoinHandle<()>,
}

/// Where and how an [`SseTransport`] sends its messages.
#[derive(Clone)]
struct Poster {
    client: Client,
    endpoint: Url,
    headers: HeaderMap,
}

impl SseTransport {
    async fn open(client: Client, url: &str, headers: HeaderMap) -> anyhow::Result<Self> {
        let base = Url::parse(url)?;
        let mut request = client
            .http
            .get(base.clone())
            .header(ACCEPT, "text/event-stream")
            .headers(headers.clone());
        if let Some(token) = client.auth.token() {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("GET {base}: HTTP {status}");
        }
        let event_stream = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
        if !event_stream {
            anyhow::bail!("GET {base}: not an event stream");
        }
        let pending = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&pending);
        let bytes = response.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(std::io::Error::other)?;
            if counted.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len() > MAX_EVENT_BYTES {
                return Err(std::io::Error::other("an event over 16 MiB"));
            }
            Ok(chunk)
        });
        let mut events = Box::pin(SseStream::from_bytes_stream(bytes));
        let endpoint = loop {
            match events.next().await {
                Some(Ok(event)) => {
                    pending.store(0, Ordering::Relaxed);
                    if event.event.as_deref() == Some("endpoint") {
                        break event.data.unwrap_or_default();
                    }
                }
                Some(Err(e)) => anyhow::bail!("reading the event stream: {e}"),
                None => anyhow::bail!("the event stream ended before its endpoint event"),
            }
        };
        let endpoint = base
            .join(endpoint.trim())
            .map_err(|e| anyhow::anyhow!("endpoint {endpoint:?}: {e}"))?;
        // Messages carry the server's authorization: they go to its own
        // origin only.
        if endpoint.origin() != base.origin() {
            anyhow::bail!("its endpoint {endpoint} is not on the origin of {base}");
        }
        let (tx, incoming) = mpsc::channel(QUEUE);
        let reader = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                pending.store(0, Ordering::Relaxed);
                let event = match event {
                    Ok(event) => event,
                    Err(e) => {
                        warn!(error = %e, "an MCP server's event stream failed");
                        break;
                    }
                };
                if !matches!(event.event.as_deref(), None | Some("message")) {
                    continue;
                }
                let Some(data) = event.data else { continue };
                match serde_json::from_str::<ServerJsonRpcMessage>(&data) {
                    Ok(message) => {
                        if tx.send(message).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!(error = %e, "an MCP server sent a message that does not parse"),
                }
            }
        });
        Ok(Self {
            incoming,
            poster: Poster {
                client,
                endpoint,
                headers,
            },
            reader,
        })
    }
}

impl Poster {
    async fn send(self, message: ClientJsonRpcMessage) -> Result<(), std::io::Error> {
        let body = serde_json::to_vec(&message).map_err(std::io::Error::other)?;
        let mut request = self
            .client
            .http
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .headers(self.headers)
            .body(body);
        if let Some(token) = self.client.auth.token() {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(std::io::Error::other)?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "POST {}: HTTP {status}",
                self.endpoint
            )))
        }
    }
}

impl Transport<RoleClient> for SseTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: ClientJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.poster.clone().send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<ServerJsonRpcMessage>> + Send {
        self.incoming.recv()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.reader.abort();
        Ok(())
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn remote(url: &str) -> McpServerConfig {
        McpServerConfig {
            url: url.into(),
            ..Default::default()
        }
    }

    fn networked() -> Policy {
        let mut policy = Policy::default();
        policy.permissions.can_access_network = true;
        policy.network_policy.egress_allowed = true;
        policy
    }

    #[tokio::test]
    async fn a_remote_server_needs_network_and_an_allowed_host() {
        let mut policy = Policy::default();
        policy.permissions.can_access_network = false;
        let Err(e) = connect("x", &remote("https://mcp.example.com/mcp"), &policy, true).await
        else {
            panic!("connected without network");
        };
        assert!(e.to_string().contains("no network"), "{e}");

        let mut policy = networked();
        policy.network_policy.blocked_domains = vec!["example.com".into()];
        let Err(e) = connect("x", &remote("https://mcp.example.com/mcp"), &policy, true).await
        else {
            panic!("connected to a blocked domain");
        };
        assert!(e.to_string().contains("blocked"), "{e}");
    }

    #[tokio::test]
    async fn a_server_a_role_supplies_gets_no_token() {
        let mut cfg = remote("https://mcp.example.com/mcp");
        cfg.auth = McpAuth::Bearer {
            token: "secret:linear".into(),
        };
        let Err(e) = connect("x", &cfg, &networked(), false).await else {
            panic!("a role's server got the owner's secret");
        };
        assert!(e.to_string().contains("role"), "{e}");
        cfg.auth = McpAuth::Oauth(havn_core::McpOAuth::default());
        let Err(e) = connect("x", &cfg, &networked(), false).await else {
            panic!("a role's server got the owner's token");
        };
        assert!(e.to_string().contains("role"), "{e}");
    }

    #[tokio::test]
    async fn an_oauth_server_without_a_token_is_not_connected() {
        let mut cfg = remote("https://mcp.example.com/mcp");
        cfg.auth = McpAuth::Oauth(havn_core::McpOAuth::default());
        let Err(e) = connect("unconnected", &cfg, &networked(), true).await else {
            panic!("connected without a token");
        };
        assert!(e.to_string().contains("not connected"), "{e}");
    }

    #[tokio::test]
    async fn the_sse_endpoint_must_be_on_the_servers_origin() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n\
                      event: endpoint\ndata: https://evil.example/messages\n\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let url = format!("http://127.0.0.1:{port}/sse");
        let client = Client {
            http: http_client(&url).unwrap(),
            auth: Arc::new(Authorization::Static("token-0123".into())),
        };
        let Err(e) = SseTransport::open(client, &url, HeaderMap::new()).await else {
            panic!("an endpoint on another origin was taken");
        };
        assert!(e.to_string().contains("not on the origin"), "{e}");
    }
}
