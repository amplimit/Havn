//! The egress proxy (issue #136). An agent whose policy sends egress
//! through the proxy has a veth but no route past the gateway host, so its
//! tools reach the internet only here (`HTTP_PROXY`/`HTTPS_PROXY` point at
//! the host end of its link). For each connection the proxy finds the
//! agent by its address, refuses blocked domains, lets allowed ones
//! through, and for a domain on neither list asks the agent's owner in
//! the dashboard chat, with one question for the hosts of a known package
//! registry. Connections go only to public addresses.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use havn_core::{AgentId, Policy};
use havn_spawner::network::AgentSubnetPool;
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::webchat::WebChatRouter;

/// Longest request head read.
const MAX_HEAD_BYTES: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a connection waits for the owner's answer.
const DECISION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long "allow" (not "always") lets a domain through.
const ALLOWED_FOR: Duration = Duration::from_secs(10 * 60);
/// Open connections one agent may have through the proxy.
const MAX_CONNECTIONS_PER_AGENT: usize = 64;

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

/// The hosts asked about together with `host`, and a note on what they
/// are when they are a known package source.
fn group_of(host: &str) -> (Vec<String>, Option<&'static str>) {
    for (name, hosts) in KNOWN_SOURCES {
        let list: Vec<String> = hosts.iter().map(|h| (*h).to_string()).collect();
        if havn_core::policy::domain_listed(&list, host) {
            return (list, Some(name));
        }
    }
    (vec![host.to_string()], None)
}

#[derive(Debug, Default)]
struct State {
    /// Domains allowed for a while, per agent.
    allowed: HashMap<(AgentId, String), Instant>,
    /// Questions waiting, per agent and first host of the group, with the
    /// connections waiting on them.
    asking: HashMap<(AgentId, String), Vec<oneshot::Sender<bool>>>,
    connections: HashMap<AgentId, usize>,
}

#[derive(Debug)]
pub struct EgressProxy {
    db: SqlitePool,
    router: WebChatRouter,
    multi_user: bool,
    pool: AgentSubnetPool,
    state: Mutex<State>,
}

/// An agent's open connection, counted until dropped.
struct Slot {
    proxy: Arc<EgressProxy>,
    agent: AgentId,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let proxy = Arc::clone(&self.proxy);
        let agent = self.agent;
        tokio::spawn(async move {
            let mut state = proxy.state.lock().await;
            if let Some(n) = state.connections.get_mut(&agent) {
                *n = n.saturating_sub(1);
            }
        });
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
            state: Mutex::new(State::default()),
        })
    }

    /// The proxy URL `agent`'s tools use.
    #[must_use]
    pub fn url_for(pool: AgentSubnetPool, agent: AgentId, port: u16) -> String {
        let net = havn_spawner::network::network_for(agent, pool);
        format!("http://{}:{port}", net.host_ip)
    }

    /// Serve on `port` of every address; connections from anything but
    /// an agent's address are closed at once.
    pub async fn serve(self: Arc<Self>, port: u16) -> std::io::Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
        info!(port, "egress proxy listening");
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

    /// The running agent at `ip`, if exactly one is.
    async fn agent_at(&self, ip: IpAddr) -> Option<havn_db::repo::agents::Agent> {
        let IpAddr::V4(ip) = ip else {
            return None;
        };
        let running = havn_db::repo::agents::list_with_status(
            &self.db,
            havn_db::repo::agents::AgentStatus::Running,
        )
        .await
        .ok()?;
        let mut found = running
            .into_iter()
            .filter(|a| havn_spawner::network::network_for(a.id, self.pool).ns_ip == ip);
        let agent = found.next()?;
        found.next().is_none().then_some(agent)
    }

    async fn handle(
        self: Arc<Self>,
        mut client: TcpStream,
        peer: SocketAddr,
    ) -> Result<(), String> {
        let Some(agent) = self.agent_at(peer.ip()).await else {
            return Err("not an agent's address".into());
        };
        {
            let mut state = self.state.lock().await;
            let open = state.connections.entry(agent.id).or_insert(0);
            if *open >= MAX_CONNECTIONS_PER_AGENT {
                drop(state);
                let _ = reply(&mut client, 429, "too many connections through the proxy").await;
                return Err("connection cap".into());
            }
            *open += 1;
        }
        let _slot = Slot {
            proxy: Arc::clone(&self),
            agent: agent.id,
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
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
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
        let key_host = group[0].clone();
        let receiver = {
            let mut state = self.state.lock().await;
            state.allowed.retain(|_, at| at.elapsed() < ALLOWED_FOR);
            if state.allowed.contains_key(&(agent.id, key_host.clone())) {
                return Ok(());
            }
            let (tx, rx) = oneshot::channel();
            let waiting = state
                .asking
                .entry((agent.id, key_host.clone()))
                .or_default();
            let first = waiting.is_empty();
            waiting.push(tx);
            (rx, first)
        };
        let (rx, first) = receiver;
        if first {
            self.ask(agent, host, &group, source, key_host.clone())
                .await;
        }
        match tokio::time::timeout(DECISION_TIMEOUT, rx).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err((403, format!("the owner did not allow {host}"))),
            _ => Err((403, format!("{host} was not allowed in time"))),
        }
    }

    /// Ask the owner about `group` for `agent`, and wait in the
    /// background for the answer, which the connections waiting on it
    /// share.
    async fn ask(
        &self,
        agent: &havn_db::repo::agents::Agent,
        host: &str,
        group: &[String],
        source: Option<&str>,
        key_host: String,
    ) {
        let id = format!("{REQUEST_PREFIX}{}", Uuid::now_v7());
        let input = match source {
            Some(name) => format!(
                "Reach {} ({name}, an official package source; allowing it is recommended).",
                group.join(", ")
            ),
            None => format!("Reach {host}."),
        };
        let request = havn_proto::ApprovalRequest {
            request_id: id.clone(),
            channel_id: String::new(),
            tool: "network".into(),
            input,
            reason: format!("{host} is not on the agent's allowed domains"),
        };
        let decision = self.router.ask_gateway_approval(agent.id, request).await;
        let proxy_state = &self.state;
        let answer = match decision {
            Some(rx) => match tokio::time::timeout(DECISION_TIMEOUT, rx).await {
                Ok(Ok(answer)) => Some(answer),
                _ => {
                    let _ = self.router.close_approval(agent.id, &id).await;
                    None
                }
            },
            None => None,
        };
        let (allow, always) = answer.unwrap_or((false, false));
        if allow && always {
            if let Err(e) = add_allowed(&self.db, agent, group).await {
                warn!(agent_id = %agent.id, error = %e, "could not save allowed domains");
            }
        }
        crate::audit::record(
            &self.db,
            agent.owner_id,
            agent.team_id,
            Some(agent.id),
            "egress.domain",
            serde_json::json!({
                "hosts": group,
                "allowed": allow,
                "always": always,
                "answered": answer.is_some(),
            }),
        )
        .await;
        let mut state = proxy_state.lock().await;
        if allow {
            state
                .allowed
                .insert((agent.id, key_host.clone()), Instant::now());
        }
        for tx in state
            .asking
            .remove(&(agent.id, key_host))
            .unwrap_or_default()
        {
            let _ = tx.send(allow);
        }
    }
}

/// Add `hosts` to `agent`'s own allowed domains.
async fn add_allowed(
    db: &SqlitePool,
    agent: &havn_db::repo::agents::Agent,
    hosts: &[String],
) -> Result<(), String> {
    let mut config = agent.config.as_object().cloned().unwrap_or_default();
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
    havn_core::policy::validate_domains("allowed_domains", &allowed).map_err(|e| e.to_string())?;
    network.insert("allowed_domains".into(), serde_json::json!(allowed));
    policy.insert("network_policy".into(), serde_json::Value::Object(network));
    config.insert("policy".into(), serde_json::Value::Object(policy));
    havn_db::repo::agents::patch(db, agent.id, None, Some(&serde_json::Value::Object(config)))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    connect: bool,
    method: String,
    host: String,
    port: u16,
    /// The origin-form target of a plain HTTP request.
    path: String,
    /// Header lines other than proxy and connection ones.
    headers: Vec<String>,
    content_length: Option<u64>,
    chunked: bool,
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

fn parse_head(head: &[u8]) -> Result<Request, String> {
    let text = std::str::from_utf8(head).map_err(|_| "request head is not UTF-8".to_string())?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (method, target, version) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err("not an HTTP/1 request".into());
    }
    let mut headers = Vec::new();
    let mut content_length = None;
    let mut chunked = false;
    for header in lines.filter(|l| !l.is_empty()) {
        let Some((name, value)) = header.split_once(':') else {
            return Err("malformed header".into());
        };
        let name_lower = name.trim().to_ascii_lowercase();
        match name_lower.as_str() {
            "content-length" => {
                content_length = Some(
                    value
                        .trim()
                        .parse()
                        .map_err(|_| "bad Content-Length".to_string())?,
                );
            }
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
        if !name_lower.starts_with("proxy-")
            && name_lower != "connection"
            && name_lower != "keep-alive"
        {
            headers.push(header.to_string());
        }
    }
    if method.eq_ignore_ascii_case("CONNECT") {
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
            chunked,
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
        chunked,
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
    if host.parse::<Ipv4Addr>().is_ok() {
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

/// One plain HTTP request: the head in origin form with
/// `Connection: close`, its body by `Content-Length`, then the response.
async fn forward_plain(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    request: &Request,
    already: Vec<u8>,
) -> Result<(), String> {
    if request.chunked {
        let _ = reply(
            client,
            411,
            "chunked request bodies are not forwarded; send a Content-Length",
        )
        .await;
        return Err("chunked body".into());
    }
    let mut head = format!("{} {} HTTP/1.1\r\n", request.method, request.path);
    for header in &request.headers {
        head.push_str(header);
        head.push_str("\r\n");
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
    remaining -= first as u64;
    if remaining > 0 {
        let mut body = (&mut *client).take(remaining);
        tokio::io::copy(&mut body, upstream)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::io::copy(upstream, client)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn reply(stream: &mut TcpStream, code: u16, why: &str) -> std::io::Result<()> {
    let status = match code {
        400 => "Bad Request",
        403 => "Forbidden",
        411 => "Length Required",
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
        let connect = parse_head(b"CONNECT pypi.org:443 HTTP/1.1\r\nHost: pypi.org:443\r\nProxy-Authorization: x\r\n\r\n").unwrap();
        assert!(connect.connect);
        assert_eq!((connect.host.as_str(), connect.port), ("pypi.org", 443));
        let plain = parse_head(
            b"GET http://Deb.Debian.org/debian/dists HTTP/1.1\r\nHost: deb.debian.org\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
        assert!(!plain.connect);
        assert_eq!(plain.host, "deb.debian.org");
        assert_eq!(plain.path, "/debian/dists");
        assert_eq!(
            plain.headers,
            vec!["Host: deb.debian.org", "Content-Length: 0"]
        );
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
        ] {
            assert!(parse_head(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn package_sources_are_asked_about_together() {
        let (group, source) = group_of("files.pythonhosted.org");
        assert_eq!(group, ["pypi.org", "files.pythonhosted.org"]);
        assert!(source.unwrap().contains("PyPI"));
        let (alone, none) = group_of("example.com");
        assert_eq!(alone, ["example.com"]);
        assert!(none.is_none());
    }
}
