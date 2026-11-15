//! The egress proxy (issue #136). An agent whose policy sends egress
//! through the proxy has a veth but no route past the gateway host, and its
//! veth drops everything not addressed to the host, so its tools reach the
//! internet only here (`HTTP_PROXY`/`HTTPS_PROXY` point at the host end of
//! its link). For each connection the proxy finds the agent by its
//! address, refuses blocked domains, lets allowed ones through, and for a
//! domain on neither list asks the agent's owner in the dashboard chat,
//! with one question for the hosts of a known package source. Connections
//! go only to public addresses.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use havn_core::{AgentId, Policy};
use havn_spawner::network::AgentSubnetPool;
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::webchat::WebChatRouter;

/// Longest request head read.
const MAX_HEAD_BYTES: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a connection waits for the owner's answer.
const DECISION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long "allow" (not "always") lets a domain through, and how long a
/// denial holds before the owner is asked again.
const DECIDED_FOR: Duration = Duration::from_secs(10 * 60);
/// How long a connection through the proxy may carry nothing.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Open connections one agent link may have through the proxy.
const MAX_CONNECTIONS_PER_AGENT: usize = 64;
/// New questions one agent may raise per minute; past that, domains on
/// neither list are refused without asking.
const MAX_QUESTIONS_PER_MINUTE: usize = 5;

/// The request id prefix of the proxy's approval requests.
pub const REQUEST_PREFIX: &str = "egress:";

/// Hosts of common package sources, asked about together.
const KNOWN_SOURCES: &[(&str, &[&str])] = &[
    (
        "PyPI, the Python package index",
        &["pypi.org", "files.pythonhosted.org"],
    ),
    (
        "npm, the JavaScript package registry",
        &["registry.npmjs.org", "registry.yarnpkg.com"],
    ),
    (
        "crates.io, the Rust package registry",
        &["crates.io", "static.crates.io", "index.crates.io"],
    ),
    (
        "the Go module proxy",
        &["proxy.golang.org", "sum.golang.org"],
    ),
    (
        "GitHub",
        &[
            "github.com",
            "api.github.com",
            "codeload.github.com",
            "objects.githubusercontent.com",
            "raw.githubusercontent.com",
        ],
    ),
    (
        "the Debian package archive",
        &["deb.debian.org", "security.debian.org"],
    ),
    (
        "the Ubuntu package archive",
        &[
            "archive.ubuntu.com",
            "security.ubuntu.com",
            "ports.ubuntu.com",
        ],
    ),
];

/// The hosts asked about together with `host`, and what they are when
/// they are a known package source.
fn group_of(host: &str) -> (Vec<String>, Option<&'static str>) {
    for (name, hosts) in KNOWN_SOURCES {
        let list: Vec<String> = hosts.iter().map(|h| (*h).to_string()).collect();
        if list.iter().any(|h| h == host) {
            return (list, Some(*name));
        }
    }
    (vec![host.to_string()], None)
}

#[derive(Debug, Default)]
struct State {
    /// The owner's recent answers, per agent and first host of a group:
    /// allowed or denied, and when.
    decided: HashMap<(AgentId, String), (bool, Instant)>,
    /// Questions waiting, per agent and first host of the group, with the
    /// connections waiting on them: the owner's answer, `None` when there
    /// was none.
    asking: HashMap<(AgentId, String), Vec<oneshot::Sender<Option<bool>>>>,
    /// When each agent's recent questions were asked.
    asked: HashMap<AgentId, Vec<Instant>>,
    /// Open connections per agent address.
    connections: HashMap<Ipv4Addr, usize>,
}

#[derive(Debug)]
pub struct EgressProxy {
    db: SqlitePool,
    router: WebChatRouter,
    multi_user: bool,
    pool: AgentSubnetPool,
    state: std::sync::Mutex<State>,
}

/// An agent link's open connection, counted until dropped.
struct Slot {
    proxy: Arc<EgressProxy>,
    peer: Ipv4Addr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut state = self.proxy.lock();
        if let Some(n) = state.connections.get_mut(&self.peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                state.connections.remove(&self.peer);
            }
        }
    }
}

impl EgressProxy {
    #[must_use]
    pub fn new(
        db: SqlitePool,
        router: WebChatRouter,
        multi_user: bool,
        pool: AgentSubnetPool,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            router,
            multi_user,
            pool,
            state: std::sync::Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The proxy URL the tools of the agent at link `slot` use.
    #[must_use]
    pub fn url_for(pool: AgentSubnetPool, slot: u16, port: u16) -> String {
        let net = havn_spawner::network::network_for(slot, pool);
        format!("http://{}:{port}", net.host_ip)
    }

    /// Bind the proxy's port on every address. Done before any agent
    /// starts, so no other program holds the port agents are let through
    /// to.
    pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
        TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await
    }

    /// Serve on `listener`; connections not from an agent's address to
    /// its own link are closed at once.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        if let Ok(addr) = listener.local_addr() {
            info!(port = addr.port(), "egress proxy listening");
        }
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    warn!(error = %e, "egress proxy accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let proxy = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(e) = proxy.handle(stream, peer).await {
                    debug!(%peer, error = %e, "egress proxy connection ended");
                }
            });
        }
    }

    /// `peer` when it is an address of the agent pool.
    fn in_pool(&self, peer: IpAddr) -> Option<Ipv4Addr> {
        let IpAddr::V4(peer) = peer else {
            return None;
        };
        let [a, b, ..] = peer.octets();
        ((a, b) == (self.pool.first_octet, self.pool.second_octet)).then_some(peer)
    }

    /// The running agent whose link `peer` came from to `local`, if
    /// exactly one is.
    async fn agent_at(
        &self,
        peer: Ipv4Addr,
        local: IpAddr,
    ) -> Option<havn_db::repo::agents::Agent> {
        let IpAddr::V4(local) = local else {
            return None;
        };
        let slot = havn_spawner::network::slot_of(peer, self.pool)?;
        let net = havn_spawner::network::network_for(slot, self.pool);
        if net.host_ip != local {
            return None;
        }
        let agent = havn_db::repo::agents::at_link(&self.db, slot)
            .await
            .ok()??;
        (agent.status == havn_db::repo::agents::AgentStatus::Running).then_some(agent)
    }

    async fn handle(
        self: Arc<Self>,
        mut client: TcpStream,
        peer: SocketAddr,
    ) -> Result<(), String> {
        let local = client.local_addr().map_err(|e| e.to_string())?;
        let Some(peer) = self.in_pool(peer.ip()) else {
            return Err("not an agent's link".into());
        };
        // Counted before any lookup, so a connect loop costs no queries
        // past the cap.
        let full = {
            let mut state = self.lock();
            let open = state.connections.entry(peer).or_insert(0);
            let full = *open >= MAX_CONNECTIONS_PER_AGENT;
            if !full {
                *open += 1;
            }
            full
        };
        if full {
            let _ = reply(&mut client, 429, "too many connections through the proxy").await;
            return Err("connection cap".into());
        }
        let _slot = Slot {
            proxy: Arc::clone(&self),
            peer,
        };
        let Some(agent) = self.agent_at(peer, local.ip()).await else {
            return Err("not an agent's link".into());
        };
        let (head, rest) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client))
            .await
            .map_err(|_| "no request".to_string())??;
        let request = match parse_head(&head) {
            Ok(request) => request,
            Err(e) => {
                let _ = reply(&mut client, 400, &e).await;
                return Err(e);
            }
        };
        let policy = crate::policy_resolver::for_session(&self.db, &agent, self.multi_user).await;
        if let Err((code, why)) = self.admit(&agent, &policy, &request.host).await {
            let _ = reply(&mut client, code, &why).await;
            return Err(why);
        }
        let mut upstream = match connect_public(&request.host, request.port).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = reply(&mut client, 502, &e).await;
                return Err(e);
            }
        };
        if request.connect {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .map_err(|e| e.to_string())?;
            upstream.write_all(&rest).await.map_err(|e| e.to_string())?;
            relay(&mut client, &mut upstream).await;
            return Ok(());
        }
        forward_plain(&mut client, &mut upstream, &request, rest).await
    }

    /// Whether `agent` may reach `host`: `Err` with the status and reason
    /// when not. A domain on neither list waits for the owner's answer.
    async fn admit(
        &self,
        agent: &havn_db::repo::agents::Agent,
        policy: &Policy,
        host: &str,
    ) -> Result<(), (u16, String)> {
        let net = &policy.network_policy;
        if !(policy.permissions.can_access_network && net.egress_allowed) {
            return Err((
                403,
                "this agent's policy does not allow network access".into(),
            ));
        }
        if net.is_blocked(host) {
            return Err((403, format!("{host} is on this agent's blocked domains")));
        }
        if net.is_allowed(host) || !net.egress_proxy {
            return Ok(());
        }
        let (group, source) = group_of(host);
        let key = (agent.id, group[0].clone());
        let (rx, first) = {
            let mut guard = self.lock();
            let state = &mut *guard;
            state
                .decided
                .retain(|_, (_, at)| at.elapsed() < DECIDED_FOR);
            match state.decided.get(&key) {
                Some((true, _)) => return Ok(()),
                Some((false, _)) => {
                    return Err((
                        403,
                        format!("the owner did not allow {host}; ask them again later"),
                    ));
                }
                None => {}
            }
            let (tx, rx) = oneshot::channel();
            let waiting = state.asking.entry(key.clone()).or_default();
            let first = waiting.is_empty();
            waiting.push(tx);
            if first {
                let asked = state.asked.entry(agent.id).or_default();
                asked.retain(|at| at.elapsed() < Duration::from_secs(60));
                if asked.len() >= MAX_QUESTIONS_PER_MINUTE {
                    state.asking.remove(&key);
                    return Err((
                        403,
                        format!(
                            "{host} is not on the agent's allowed domains, and it asked too often"
                        ),
                    ));
                }
                asked.push(Instant::now());
            }
            (rx, first)
        };
        if first {
            self.ask(agent, host, &group, source, key).await;
        }
        match tokio::time::timeout(DECISION_TIMEOUT, rx).await {
            Ok(Ok(Some(true))) => Ok(()),
            Ok(Ok(Some(false))) => Err((403, format!("the owner did not allow {host}"))),
            _ => Err((
                403,
                format!("the owner did not answer about {host}; ask them to allow it"),
            )),
        }
    }

    /// Ask the owner about `group` for `agent` and wait for the answer,
    /// which the connections waiting on `key` share.
    async fn ask(
        &self,
        agent: &havn_db::repo::agents::Agent,
        host: &str,
        group: &[String],
        source: Option<&str>,
        key: (AgentId, String),
    ) {
        let id = format!("{REQUEST_PREFIX}{}", Uuid::now_v7());
        let input = match source {
            Some(name) => format!("Reach {} ({name}).", group.join(", ")),
            None => format!("Reach {host}."),
        };
        let request = havn_proto::ApprovalRequest {
            request_id: id.clone(),
            channel_id: String::new(),
            tool: "network".into(),
            input,
            reason: format!("{host} is not on the agent's allowed domains"),
        };
        let posted = self.router.ask_gateway_approval(agent.id, request).await;
        let asked = posted.is_some();
        let answer = match posted {
            Some(rx) => match tokio::time::timeout(DECISION_TIMEOUT, rx).await {
                Ok(Ok(answer)) => Some(answer),
                _ => {
                    let _ = self.router.close_approval(agent.id, &id).await;
                    self.router.take_gateway_waiter(&id);
                    None
                }
            },
            None => None,
        };
        let answered = answer.is_some();
        let (allow, always, decided_by) = answer.unwrap_or((false, false, String::new()));
        if allow && always {
            if let Err(e) = self.add_allowed(agent.id, group).await {
                warn!(agent_id = %agent.id, error = %e, "could not save allowed domains");
            }
        }
        if asked {
            crate::audit::record(
                &self.db,
                agent.owner_id,
                agent.team_id,
                Some(agent.id),
                "egress.domain",
                serde_json::json!({
                    "hosts": group,
                    "answered": answered,
                    "allowed": allow,
                    "always": always,
                    "decided_by": decided_by,
                }),
            )
            .await;
        }
        let mut state = self.lock();
        // Only an answer holds; an unanswered question is asked again.
        if answered {
            state.decided.insert(key.clone(), (allow, Instant::now()));
        }
        for tx in state.asking.remove(&key).unwrap_or_default() {
            let _ = tx.send(answered.then_some(allow));
        }
    }

    /// Add `hosts` to the agent's own allowed domains, on its config as it
    /// is now.
    async fn add_allowed(&self, agent_id: AgentId, hosts: &[String]) -> Result<(), String> {
        havn_db::repo::agents::update_config(&self.db, agent_id, None, |current| {
            let mut config = current.as_object().cloned().unwrap_or_default();
            let mut policy = config
                .get("policy")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut network = policy
                .get("network_policy")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut allowed: Vec<String> = network
                .get("allowed_domains")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            for host in hosts {
                if !allowed.contains(host) {
                    allowed.push(host.clone());
                }
            }
            havn_core::policy::validate_domains("allowed_domains", &allowed)
                .map_err(|e| e.to_string())?;
            network.insert("allowed_domains".into(), serde_json::json!(allowed));
            policy.insert("network_policy".into(), serde_json::Value::Object(network));
            config.insert("policy".into(), serde_json::Value::Object(policy));
            Ok(serde_json::Value::Object(config))
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|_| ())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    connect: bool,
    method: String,
    host: String,
    port: u16,
    /// The origin-form target of a plain HTTP request.
    path: String,
    /// Header lines kept for a plain HTTP request: not Host, not proxy or
    /// connection ones.
    headers: Vec<String>,
    /// The body length of a plain HTTP request, when it has one.
    content_length: Option<u64>,
}

/// The head of a request up to its blank line, and what came after it.
async fn read_head(stream: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("closed before the request ended".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err("request head too long".into());
        }
    }
}

/// A request head, strictly: CRLF line ends only, no control bytes, no
/// whitespace before a header's colon, no folded lines, no
/// Transfer-Encoding and at most one Content-Length, so what the proxy
/// checks is what the server sees.
fn parse_head(head: &[u8]) -> Result<Request, String> {
    let text = std::str::from_utf8(head).map_err(|_| "request head is not UTF-8".to_string())?;
    let text = text
        .strip_suffix("\r\n\r\n")
        .ok_or("request head does not end with a blank line")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let bad_bytes = |s: &str| s.chars().any(|c| c.is_control() && c != '\t');
    if bad_bytes(line) {
        return Err("control characters in the request line".into());
    }
    let mut parts = line.split(' ');
    let (method, target, version) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err("not an HTTP/1 request".into());
    }
    if method.is_empty() || !method.chars().all(|c| c.is_ascii_uppercase()) {
        return Err("bad method".into());
    }
    let mut headers = Vec::new();
    let mut content_length: Option<u64> = None;
    for header in lines {
        if bad_bytes(header) {
            return Err("control characters in a header".into());
        }
        if header.starts_with([' ', '\t']) {
            return Err("folded header lines are not accepted".into());
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err("malformed header".into());
        };
        if name.is_empty() || name.ends_with([' ', '\t']) {
            return Err("malformed header name".into());
        }
        let name_lower = name.to_ascii_lowercase();
        match name_lower.as_str() {
            "content-length" => {
                if content_length.is_some() {
                    return Err("more than one Content-Length".into());
                }
                let digits = value.trim_matches([' ', '\t']);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("bad Content-Length".into());
                }
                content_length = Some(
                    digits
                        .parse()
                        .map_err(|_| "bad Content-Length".to_string())?,
                );
                // Written again from the parsed value.
                continue;
            }
            "transfer-encoding" => {
                return Err("Transfer-Encoding is not accepted; send a Content-Length".into());
            }
            _ => {}
        }
        let dropped = name_lower == "host"
            || name_lower.starts_with("proxy-")
            || name_lower == "connection"
            || name_lower == "keep-alive";
        if !dropped {
            headers.push(header.to_string());
        }
    }
    if method == "CONNECT" {
        let (host, port) = split_host_port(target, 443)?;
        if port != 443 {
            return Err(format!("the proxy tunnels port 443 only, not {port}"));
        }
        return Ok(Request {
            connect: true,
            method: method.to_string(),
            host,
            port,
            path: String::new(),
            headers,
            content_length,
        });
    }
    let rest = target
        .strip_prefix("http://")
        .ok_or("only http:// URLs and CONNECT go through the proxy")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(authority, 80)?;
    if port != 80 {
        return Err(format!(
            "the proxy forwards plain http on port 80 only, not {port}"
        ));
    }
    Ok(Request {
        connect: false,
        method: method.to_string(),
        host,
        port,
        path: path.to_string(),
        headers,
        content_length,
    })
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(String, u16), String> {
    if authority.contains('@') || authority.starts_with('[') {
        return Err("the proxy takes host names, not credentials or IPv6 literals".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| "bad port".to_string())?),
        None => (authority, default_port),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let valid = !host.is_empty()
        && host.contains('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
    if !valid {
        return Err(format!("{host:?} is not a host name"));
    }
    if host
        .rsplit('.')
        .next()
        .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()))
    {
        return Err("the proxy takes host names, not addresses".into());
    }
    Ok((host, port))
}

/// Connect to a public address of `host`.
async fn connect_public(host: &str, port: u16) -> Result<TcpStream, String> {
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("{host} does not resolve: {e}"))?
        .filter(|a| crate::web_fetch::is_public(a.ip()))
        .collect();
    if found.is_empty() {
        return Err(format!("{host} has no public address"));
    }
    let mut last = String::new();
    for addr in found {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "timed out".into(),
        }
    }
    Err(format!("connecting to {host} failed: {last}"))
}

/// One plain HTTP request: the head in origin form with the checked host
/// as `Host` and `Connection: close`, its body by `Content-Length`, then
/// the response.
async fn forward_plain(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    request: &Request,
    already: Vec<u8>,
) -> Result<(), String> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\n",
        request.method, request.path, request.host
    );
    for header in &request.headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    if let Some(length) = request.content_length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    upstream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut remaining = request.content_length.unwrap_or(0);
    let first = usize::try_from(remaining)
        .unwrap_or(usize::MAX)
        .min(already.len());
    upstream
        .write_all(&already[..first])
        .await
        .map_err(|e| e.to_string())?;
    remaining = remaining.saturating_sub(first as u64);
    if remaining > 0 {
        copy_idle(&mut (&mut *client).take(remaining), upstream).await?;
    }
    copy_idle(upstream, client).await
}

/// Copy `from` to `to` until `from` ends, giving up when nothing moves
/// for [`IDLE_TIMEOUT`].
async fn copy_idle<R, W>(from: &mut R, to: &mut W) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    pipe(from, to, &Activity::new()).await
}

/// When a connection last carried anything, shared by its directions.
struct Activity {
    start: tokio::time::Instant,
    last_ms: std::sync::atomic::AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self {
            start: tokio::time::Instant::now(),
            last_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn touch(&self) {
        self.last_ms
            .store(self.now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    fn idle(&self) -> bool {
        let last = self.last_ms.load(std::sync::atomic::Ordering::Relaxed);
        self.now_ms().saturating_sub(last)
            >= u64::try_from(IDLE_TIMEOUT.as_millis()).unwrap_or(u64::MAX)
    }
}

/// One direction of a connection, until `from` ends. A read waits as long
/// as either direction moves; a write that does not finish within
/// [`IDLE_TIMEOUT`] ends it.
async fn pipe<R, W>(from: &mut R, to: &mut W, activity: &Activity) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    let mut buf = vec![0_u8; 16 * 1024];
    loop {
        let n = match tokio::time::timeout(IDLE_TIMEOUT, from.read(&mut buf)).await {
            Ok(read) => read.map_err(|e| e.to_string())?,
            Err(_) if activity.idle() => return Err("idle".into()),
            Err(_) => continue,
        };
        if n == 0 {
            return Ok(());
        }
        activity.touch();
        tokio::time::timeout(IDLE_TIMEOUT, to.write_all(&buf[..n]))
            .await
            .map_err(|_| "the other end stopped reading".to_string())?
            .map_err(|e| e.to_string())?;
        activity.touch();
    }
}

/// Both directions of a tunnel, each on its own, until both end; a
/// direction that fails or idles ends the tunnel.
async fn relay(client: &mut TcpStream, upstream: &mut TcpStream) {
    let (mut client_read, mut client_write) = client.split();
    let (mut upstream_read, mut upstream_write) = upstream.split();
    let activity = Activity::new();
    let sending = async {
        pipe(&mut client_read, &mut upstream_write, &activity).await?;
        let _ = upstream_write.shutdown().await;
        Ok::<_, String>(())
    };
    let receiving = async {
        pipe(&mut upstream_read, &mut client_write, &activity).await?;
        let _ = client_write.shutdown().await;
        Ok::<_, String>(())
    };
    let _ = tokio::try_join!(sending, receiving);
}

async fn reply(stream: &mut TcpStream, code: u16, why: &str) -> std::io::Result<()> {
    let status = match code {
        400 => "Bad Request",
        403 => "Forbidden",
        429 => "Too Many Requests",
        _ => "Bad Gateway",
    };
    let body = format!("havn egress proxy: {why}\n");
    let text = format!(
        "HTTP/1.1 {code} {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(text.as_bytes()).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn requests_are_parsed() {
        let connect = parse_head(
            b"CONNECT pypi.org:443 HTTP/1.1\r\nHost: pypi.org:443\r\nProxy-Authorization: x\r\n\r\n",
        )
        .unwrap();
        assert!(connect.connect);
        assert_eq!((connect.host.as_str(), connect.port), ("pypi.org", 443));
        let plain = parse_head(
            b"GET http://Deb.Debian.org/debian/dists HTTP/1.1\r\nHost: attacker.example\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        assert!(!plain.connect);
        assert_eq!(plain.host, "deb.debian.org");
        assert_eq!(plain.path, "/debian/dists");
        assert_eq!(
            plain.headers,
            vec!["Accept: */*"],
            "the client's Host is dropped"
        );
    }

    #[test]
    fn unclear_requests_are_refused() {
        for bad in [
            &b"CONNECT pypi.org:22 HTTP/1.1\r\n\r\n"[..],
            b"CONNECT 1.2.3.4:443 HTTP/1.1\r\n\r\n",
            b"CONNECT [::1]:443 HTTP/1.1\r\n\r\n",
            b"GET https://pypi.org/ HTTP/1.1\r\n\r\n",
            b"GET /local HTTP/1.1\r\n\r\n",
            b"GET http://user@pypi.org/ HTTP/1.1\r\n\r\n",
            b"GET http://pypi.org:8080/ HTTP/1.1\r\n\r\n",
            b"GET http://localhost/ HTTP/1.1\r\n\r\n",
            b"SSH-2.0-OpenSSH\r\n\r\n",
            b"POST http://deb.debian.org/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST http://deb.debian.org/ HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"POST http://deb.debian.org/ HTTP/1.1\r\nContent-Length: +5\r\n\r\n",
            b"GET http://deb.debian.org/ HTTP/1.1\r\nX: a\nHost: evil\r\n\r\n",
            b"GET http://deb.debian.org/ HTTP/1.1\r\nHost : evil\r\n\r\n",
            b"GET http://deb.debian.org/ HTTP/1.1\r\nX: a\r\n folded\r\n\r\n",
            b"get http://deb.debian.org/ HTTP/1.1\r\n\r\n",
        ] {
            assert!(parse_head(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_direction_ends_when_nothing_moves() {
        // Nothing to read, and no one reading what is written.
        let (_quiet, mut from) = tokio::io::duplex(64);
        let (_deaf, mut to) = tokio::io::duplex(64);
        let activity = Activity::new();
        assert_eq!(
            pipe(&mut from, &mut to, &activity).await,
            Err("idle".to_string())
        );
        let (mut sender, mut from) = tokio::io::duplex(1024);
        sender.write_all(&[0_u8; 1024]).await.unwrap();
        let (_deaf, mut to) = tokio::io::duplex(64);
        let stalled = pipe(&mut from, &mut to, &Activity::new()).await;
        assert_eq!(stalled, Err("the other end stopped reading".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_direction_waits_while_the_other_moves() {
        let activity = Activity::new();
        let (_quiet, mut from) = tokio::io::duplex(64);
        let (_deaf, mut to) = tokio::io::duplex(64);
        let busy = async {
            for _ in 0..4 {
                tokio::time::sleep(IDLE_TIMEOUT / 2).await;
                activity.touch();
            }
        };
        let quiet = pipe(&mut from, &mut to, &activity);
        tokio::pin!(quiet);
        tokio::select! {
            () = busy => {}
            ended = &mut quiet => panic!("ended while the other direction moved: {ended:?}"),
        }
        assert_eq!(quiet.await, Err("idle".to_string()));
    }

    #[test]
    fn package_sources_are_asked_about_together() {
        let (group, source) = group_of("files.pythonhosted.org");
        assert_eq!(group, ["pypi.org", "files.pythonhosted.org"]);
        assert!(source.unwrap().contains("PyPI"));
        let (alone, none) = group_of("uploads.github.com");
        assert_eq!(
            alone,
            ["uploads.github.com"],
            "only the listed hosts, not subdomains"
        );
        assert!(none.is_none());
    }
}
