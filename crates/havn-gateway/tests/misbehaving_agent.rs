//! A misbehaving agent against a real gateway process: a client that
//! speaks the agent socket protocol and breaks policy, and the gateway
//! has to refuse or contain it.
//!
//! The tests run the gateway with the subprocess spawner and a stand-in
//! runtime that never connects, so the test can connect in its place. The
//! `#[ignore]`d test needs root and the namespace spawner, where the agent's
//! cgroup decides who may connect as it; the NamespaceSpawner CI job runs
//! it with `--ignored`.

#![cfg(target_os = "linux")]

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use havn_core::{AgentId, MessageContent, OutboundMessage, Policy};
use havn_proto::channel::{ChannelFrame, ChannelMessageContent};
use havn_proto::{
    AdminOutcome, AgentQuery, AgentQueryOutcome, AgentQueryResponse, AgentToGateway, CronOutcome,
    CronResult, GatewayToAgent, LlmRequest, MemoryForgetResponse, PROTOCOL_VERSION, read_frame,
    write_frame,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest as _};

const STEP: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_secs(1);
/// The gateway holds a Hello for an unregistered agent for up to ten
/// seconds, in case the spawn that started it has not registered it yet.
const HELLO: Duration = Duration::from_secs(20);

/// Stands in for havn-runtime: keeps the agent's process alive without
/// connecting, and exits with the gateway (or after ten minutes).
const RUNTIME_STUB: &str = "#!/bin/sh\n\
    i=0\n\
    while kill -0 \"$PPID\" 2>/dev/null && [ \"$i\" -lt 600 ]; do sleep 1; i=$((i + 1)); done\n";

/// Relays stdin/stdout to the agent socket, so that the connecting process
/// is this one, which the test places in an agent's cgroup.
const SOCKET_RELAY: &str = "import os, socket, sys, threading
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
def pump():
    while True:
        b = os.read(0, 65536)
        if not b:
            break
        s.sendall(b)
    s.shutdown(socket.SHUT_WR)
threading.Thread(target=pump, daemon=True).start()
while True:
    b = s.recv(65536)
    if not b:
        break
    os.write(1, b)
";

const AGE_KEY: &str = "misbehaving-agent-test";

struct Gateway {
    dir: tempfile::TempDir,
    process: Option<std::process::Child>,
    url: String,
    http: reqwest::Client,
    isolated: bool,
}

impl Gateway {
    async fn start(extra_config: &str) -> Self {
        Self::launch(false, extra_config).await
    }

    async fn launch(isolated: bool, extra_config: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("havn-misbehaving-")
            .tempdir()
            .expect("tempdir");
        // Under the namespace spawner the binaries are executed from inside
        // a user namespace, so every directory on their path must be
        // traversable by others.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("bin dir");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod bin");
        let stub = bin.join("runtime-stub");
        std::fs::write(&stub, RUNTIME_STUB).expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        if isolated {
            let built = Path::new(env!("CARGO_BIN_EXE_havn-gateway")).with_file_name("havn-init");
            assert!(
                built.exists(),
                "{} is missing; run `cargo build --bins` first",
                built.display()
            );
            std::fs::copy(&built, bin.join("havn-init")).expect("copy havn-init");
        }
        let mut gateway = Self {
            dir,
            process: None,
            url: String::new(),
            http: reqwest::Client::new(),
            isolated,
        };
        gateway.run(extra_config).await;
        gateway
    }

    /// Write the config and start the gateway process on a free port.
    async fn run(&mut self, extra_config: &str) {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let dir = self.dir.path();
        let init = if self.isolated {
            format!("init_binary = {:?}\n", dir.join("bin/havn-init"))
        } else {
            String::new()
        };
        let config = format!(
            "listen = \"127.0.0.1:{port}\"\n\
             db_path = {:?}\n\
             agent_socket_path = {:?}\n\
             workspace_root = {:?}\n\
             runtime_binary = {:?}\n\
             {init}\n\
             {extra_config}\n",
            dir.join("havn.db"),
            dir.join("agent.sock"),
            dir.join("agents"),
            dir.join("bin/runtime-stub"),
        );
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, config).expect("write config");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("gateway.log"))
            .expect("log file");
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_havn-gateway"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", dir)
            .env("HAVN_CONFIG", &config_path)
            .env("HAVN_AGE_KEY", AGE_KEY)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log fd"))
            .stderr(log);
        if !self.isolated {
            cmd.env("HAVN_SPAWNER", "subprocess");
        }
        self.process = Some(cmd.spawn().expect("spawn gateway"));
        self.url = format!("http://127.0.0.1:{port}");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(resp) = self.http.get(self.at("/healthz")).send().await
                && resp.status().is_success()
            {
                return;
            }
            if let Some(status) = self
                .process
                .as_mut()
                .and_then(|p| p.try_wait().expect("wait"))
            {
                panic!("gateway exited with {status}:\n{}", self.log());
            }
            assert!(
                Instant::now() < deadline,
                "gateway never became healthy:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stop the gateway and start it again on the same data with
    /// `extra_config`, for settings that are read only at startup.
    async fn restart(&mut self, extra_config: &str) {
        self.terminate();
        self.run(extra_config).await;
    }

    fn terminate(&mut self) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        if let Ok(pid) = i32::try_from(process.id()) {
            // SAFETY: plain kill(2) on the child we spawned and still own.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(process.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = process.kill();
        let _ = process.wait();
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("gateway.log")).unwrap_or_default()
    }

    fn at(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("agent.sock")
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut req = self.http.request(method.clone(), self.at(path));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.expect("http");
        let status = resp.status();
        let text = resp.text().await.expect("body");
        assert!(status.is_success(), "{method} {path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn create_agent(&self, name: &str, config: Value) -> AgentId {
        let agent = self
            .request(
                reqwest::Method::POST,
                "/agents",
                Some(json!({ "name": name, "config": config })),
            )
            .await;
        agent["id"]
            .as_str()
            .expect("agent id")
            .parse()
            .expect("uuid")
    }

    /// `POST /agents/{id}/start`, which returns once the agent has
    /// connected (or gives up after ten seconds).
    fn start_agent(&self, id: AgentId) -> tokio::task::JoinHandle<reqwest::StatusCode> {
        let req = self.http.post(self.at(&format!("/agents/{id}/start")));
        tokio::spawn(async move { req.send().await.expect("start").status() })
    }

    async fn agent_view(&self, id: AgentId) -> Value {
        self.request(reqwest::Method::GET, &format!("/agents/{id}"), None)
            .await
    }

    async fn wait_until(&self, what: &str, within: Duration, mut done: impl AsyncFnMut() -> bool) {
        let deadline = Instant::now() + within;
        while !done().await {
            assert!(
                Instant::now() < deadline,
                "timed out waiting until {what}:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn connected(&self, id: AgentId) -> bool {
        self.agent_view(id).await["connected"] == json!(true)
    }

    async fn assert_healthy(&self) {
        let resp = self
            .http
            .get(self.at("/healthz"))
            .send()
            .await
            .expect("healthz");
        assert!(resp.status().is_success(), "{}", resp.status());
    }

    /// Create an agent, start it, and connect to the gateway as it.
    async fn running_agent(&self, name: &str, config: Value) -> Agent {
        let id = self.create_agent(name, config).await;
        self.connect(id).await
    }

    /// Start an existing agent and connect as it once the gateway expects it.
    async fn connect(&self, id: AgentId) -> Agent {
        self.connect_session(id).await.reading()
    }

    async fn connect_session(&self, id: AgentId) -> Session {
        let started = self.start_agent(id);
        let deadline = Instant::now() + STEP;
        let session = loop {
            if let Some(session) = Link::socket(&self.socket())
                .await
                .hello(&id.to_string())
                .await
            {
                break session;
            }
            assert!(
                Instant::now() < deadline,
                "agent {id} was never accepted:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let status = started.await.expect("join");
        assert!(status.is_success(), "start {id}: {status}\n{}", self.log());
        session
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.terminate();
        if std::thread::panicking() {
            eprintln!("--- gateway log ---\n{}", self.log());
        }
    }
}

type Reader = BufReader<Box<dyn AsyncRead + Unpin + Send>>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

/// An unauthenticated connection to the agent socket.
struct Link {
    reader: Reader,
    writer: Writer,
    relay: Option<tokio::process::Child>,
}

impl Link {
    async fn socket(path: &Path) -> Self {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .expect("connect agent socket");
        let (read, write) = stream.into_split();
        Self {
            reader: BufReader::new(Box::new(read)),
            writer: Box::new(write),
            relay: None,
        }
    }

    /// Connect from a process that first moves itself into `cgroup`.
    async fn from_cgroup(cgroup: &Path, socket: &Path) -> Self {
        let mut relay = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("echo $$ > \"$1\" && exec python3 -c \"$2\" \"$3\"")
            .arg("sh")
            .arg(cgroup.join("cgroup.procs"))
            .arg(SOCKET_RELAY)
            .arg(socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn relay");
        let stdout = relay.stdout.take().expect("relay stdout");
        let stdin = relay.stdin.take().expect("relay stdin");
        Self {
            reader: BufReader::new(Box::new(stdout)),
            writer: Box::new(stdin),
            relay: Some(relay),
        }
    }

    /// Send Hello for `agent_id`. `None` when the gateway closes the
    /// connection instead of sending Welcome.
    async fn hello(mut self, agent_id: &str) -> Option<Session> {
        let hello = AgentToGateway::Hello {
            agent_id: agent_id.to_string(),
            version: PROTOCOL_VERSION.to_string(),
        };
        write_frame(&mut self.writer, &hello).await.ok()?;
        let reply = tokio::time::timeout(HELLO, read_frame::<_, GatewayToAgent>(&mut self.reader))
            .await
            .expect("the gateway answers Hello or closes the connection");
        match reply {
            Ok(Some(GatewayToAgent::Welcome { .. })) => Some(Session {
                reader: self.reader,
                writer: self.writer,
                relay: self.relay,
            }),
            Ok(Some(other)) => panic!("expected Welcome or a closed connection, got {other:?}"),
            Ok(None) | Err(_) => None,
        }
    }
}

/// A connection the gateway accepted. Nothing reads from it until
/// [`Session::reading`].
struct Session {
    reader: Reader,
    writer: Writer,
    relay: Option<tokio::process::Child>,
}

impl Session {
    fn reading(self) -> Agent {
        let (tx, frames) = mpsc::unbounded_channel();
        let mut reader = self.reader;
        tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame::<_, GatewayToAgent>(&mut reader).await {
                if !matches!(frame, GatewayToAgent::HeartbeatTick) && tx.send(frame).is_err() {
                    break;
                }
            }
        });
        Agent {
            writer: self.writer,
            frames,
            held: VecDeque::new(),
            fences: 0,
            _relay: self.relay,
        }
    }
}

#[derive(Debug)]
enum Next {
    Frame(Box<GatewayToAgent>),
    Closed,
    Quiet,
}

struct Agent {
    writer: Writer,
    frames: mpsc::UnboundedReceiver<GatewayToAgent>,
    /// Frames that arrived while waiting for a fence.
    held: VecDeque<GatewayToAgent>,
    fences: u32,
    _relay: Option<tokio::process::Child>,
}

impl Agent {
    async fn send(&mut self, frame: &AgentToGateway) {
        write_frame(&mut self.writer, frame)
            .await
            .expect("send frame");
    }

    async fn next(&mut self, within: Duration) -> Next {
        if let Some(frame) = self.held.pop_front() {
            return Next::Frame(Box::new(frame));
        }
        match tokio::time::timeout(within, self.frames.recv()).await {
            Ok(Some(frame)) => Next::Frame(Box::new(frame)),
            Ok(None) => Next::Closed,
            Err(_) => Next::Quiet,
        }
    }

    async fn expect(&mut self, what: &str) -> GatewayToAgent {
        match self.next(STEP).await {
            Next::Frame(frame) => *frame,
            other => panic!("expected {what}, got {other:?}"),
        }
    }

    async fn expect_quiet(&mut self, why: &str) {
        match self.next(QUIET).await {
            Next::Quiet => {}
            other => panic!("{why}: got {other:?}"),
        }
    }

    /// Round-trip one request, so every frame sent before it has been
    /// handled by the gateway. Frames that arrive meanwhile are kept.
    async fn fence(&mut self) {
        self.fences += 1;
        let id = format!("fence-{}", self.fences);
        self.send(&llm_request(&id)).await;
        let deadline = Instant::now() + STEP;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.frames.recv()).await {
                Ok(Some(GatewayToAgent::LlmResponse(r))) if r.request_id == id => return,
                Ok(Some(other)) => self.held.push_back(other),
                Ok(None) => panic!("connection closed while waiting for {id}"),
                Err(_) => panic!("no reply to {id}"),
            }
        }
    }
}

fn llm_request(request_id: &str) -> AgentToGateway {
    AgentToGateway::LlmRequest(LlmRequest {
        request_id: request_id.into(),
        model: "claude-sonnet-4-6".into(),
        messages: json!([{ "role": "user", "content": "hi" }]),
        options: json!({}),
        billing_user_id: None,
    })
}

fn query(request_id: &str, target: AgentId) -> AgentToGateway {
    AgentToGateway::AgentQuery(AgentQuery {
        request_id: request_id.into(),
        target_agent_id: target.to_string(),
        prompt: "what are you working on?".into(),
        timeout_seconds: 30,
        include_transcript: false,
    })
}

fn may_query_other_agents() -> Value {
    let mut policy = Policy::default();
    policy.permissions.can_query_other_agents = true;
    json!({ "policy": policy })
}

fn outbound(channel_id: &str, text: &str) -> AgentToGateway {
    AgentToGateway::OutboundMessage(OutboundMessage {
        channel_id: channel_id.into(),
        content: MessageContent::Text { text: text.into() },
        reply_to: None,
    })
}

#[tokio::test]
async fn claims_to_be_a_connected_or_unknown_agent_are_refused() {
    let gw = Gateway::start("").await;
    let mut agent = gw.running_agent("owner", json!({})).await;
    let id = gw.create_agent("victim", json!({})).await;
    let mut victim = gw.connect(id).await;

    for claim in [
        id.to_string(),
        AgentId::new().to_string(),
        "not-an-agent-id".to_string(),
    ] {
        let accepted = Link::socket(&gw.socket()).await.hello(&claim).await;
        assert!(accepted.is_none(), "a claim to be {claim} was accepted");
    }

    assert!(gw.connected(id).await);
    victim.fence().await;
    agent.fence().await;
    victim
        .expect_quiet("the real agent keeps its connection")
        .await;
}

#[tokio::test]
async fn agent_query_without_permission_is_refused() {
    let gw = Gateway::start("").await;
    let mut caller = gw.running_agent("caller", json!({})).await;
    let target_id = gw.create_agent("target", json!({})).await;
    let mut target = gw.connect(target_id).await;

    caller.send(&query("q1", target_id)).await;

    match caller.expect("AgentQueryResult").await {
        GatewayToAgent::AgentQueryResult(r) => {
            assert_eq!(r.request_id, "q1");
            assert!(
                matches!(&r.outcome, AgentQueryOutcome::Error { message } if message.contains("not permitted")),
                "{:?}",
                r.outcome
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    target
        .expect_quiet("the target must not see a refused query")
        .await;
}

#[tokio::test]
async fn answers_to_requests_addressed_to_another_agent_are_ignored() {
    let gw = Gateway::start("").await;
    let mut caller = gw.running_agent("caller", may_query_other_agents()).await;
    let target_id = gw.create_agent("target", json!({})).await;
    let mut target = gw.connect(target_id).await;
    let mut intruder = gw.running_agent("intruder", json!({})).await;

    caller.send(&query("q1", target_id)).await;
    let dispatched = match target.expect("IncomingQuery").await {
        GatewayToAgent::IncomingQuery(q) => q.request_id,
        other => panic!("unexpected {other:?}"),
    };
    intruder
        .send(&AgentToGateway::AgentQueryResponse(AgentQueryResponse {
            request_id: dispatched.clone(),
            outcome: AgentQueryOutcome::Ok,
            text: "forged".into(),
            transcript: Value::Null,
        }))
        .await;
    intruder.fence().await;
    target
        .send(&AgentToGateway::AgentQueryResponse(AgentQueryResponse {
            request_id: dispatched,
            outcome: AgentQueryOutcome::Ok,
            text: "genuine".into(),
            transcript: Value::Null,
        }))
        .await;
    match caller.expect("AgentQueryResult").await {
        GatewayToAgent::AgentQueryResult(r) => {
            assert_eq!(r.request_id, "q1");
            assert_eq!(r.text, "genuine");
        }
        other => panic!("unexpected {other:?}"),
    }

    // The same for an admin RPC: the dashboard deleting a memory entry.
    let forget = gw
        .http
        .delete(gw.at(&format!("/agents/{target_id}/memory/some-key")))
        .send();
    let forget = tokio::spawn(forget);
    let rpc = match target.expect("MemoryForgetRequest").await {
        GatewayToAgent::MemoryForgetRequest(r) => r.request_id,
        other => panic!("unexpected {other:?}"),
    };
    intruder
        .send(&AgentToGateway::MemoryForgetResponse(
            MemoryForgetResponse {
                request_id: rpc.clone(),
                outcome: AdminOutcome::Ok {
                    details: Value::Null,
                },
            },
        ))
        .await;
    intruder.fence().await;
    target
        .send(&AgentToGateway::MemoryForgetResponse(
            MemoryForgetResponse {
                request_id: rpc,
                outcome: AdminOutcome::NotFound,
            },
        ))
        .await;
    let status = forget.await.expect("join").expect("http").status();
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "the target's answer decides the request"
    );
}

#[tokio::test]
async fn outbound_through_unbound_channels_is_dropped() {
    let mut gw = Gateway::start("").await;
    let bound_id = gw.create_agent("bound", json!({})).await;
    let intruder_id = gw.create_agent("intruder", json!({})).await;
    gw.request(
        reqwest::Method::POST,
        "/credentials",
        Some(json!({ "provider": "channel:telegram", "name": "bot", "api_key": "adapter-token" })),
    )
    .await;
    // Bindings are read at startup.
    gw.restart(&format!(
        "[[channels.telegram.accounts]]\n\
         id = \"bot\"\n\
         adapter_token_ref = \"secret:channel:telegram:bot\"\n\
         dm_policy = \"allowlist\"\n\
         allow_from = [\"tg:1\"]\n\
         \n\
         [[bindings]]\n\
         agent_id = \"{bound_id}\"\n\
         channel = \"telegram\"\n\
         account_id = \"bot\"\n"
    ))
    .await;

    let mut request = format!(
        "{}/api/v1/channel?channel=telegram&account=bot",
        gw.url.replacen("http", "ws", 1)
    )
    .into_client_request()
    .expect("ws request");
    request.headers_mut().insert(
        "x-adapter-token",
        tungstenite::http::HeaderValue::from_static("adapter-token"),
    );
    let (mut adapter, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("adapter connects");
    let mut bound = gw.connect(bound_id).await;
    let mut intruder = gw.connect(intruder_id).await;

    intruder
        .send(&outbound("ch|telegram|bot|tg:1", "from an unbound agent"))
        .await;
    intruder.fence().await;
    bound
        .send(&outbound(
            "ch|telegram|bot|tg:2",
            "to a sender outside allow_from",
        ))
        .await;
    bound.fence().await;
    bound
        .send(&outbound("ch|telegram|bot|tg:1", "allowed"))
        .await;

    let mut delivered = Vec::new();
    let deadline = Instant::now() + STEP;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let wait = if delivered.is_empty() { left } else { QUIET };
        let Ok(message) = tokio::time::timeout(wait, adapter.next()).await else {
            break;
        };
        match message.expect("adapter stream open").expect("ws frame") {
            tungstenite::Message::Text(text) => {
                match serde_json::from_str::<ChannelFrame>(&text).expect("channel frame") {
                    ChannelFrame::Outbound(out) => delivered.push(out),
                    ChannelFrame::Ping => {
                        let pong = serde_json::to_string(&ChannelFrame::Pong).expect("pong");
                        adapter
                            .send(tungstenite::Message::Text(pong.into()))
                            .await
                            .expect("send pong");
                    }
                    _ => {}
                }
            }
            tungstenite::Message::Ping(p) => {
                adapter
                    .send(tungstenite::Message::Pong(p))
                    .await
                    .expect("pong");
            }
            other => panic!("unexpected adapter frame {other:?}"),
        }
    }
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0].to, "tg:1");
    assert_eq!(
        delivered[0].content,
        ChannelMessageContent::Text {
            text: "allowed".into()
        }
    );
}

#[tokio::test]
async fn agent_that_never_reads_is_disconnected_and_others_keep_working() {
    let gw = Gateway::start("").await;
    let mut steady = gw.running_agent("steady", json!({})).await;
    let flood_id = gw.create_agent("flood", json!({})).await;
    let session = gw.connect_session(flood_id).await;
    let (reader, mut writer) = (session.reader, session.writer);

    // Requests as fast as the gateway takes them; the replies are never read.
    let flood = tokio::spawn(async move {
        let mut sent = 0u64;
        while write_frame(&mut writer, &llm_request(&format!("r{sent}")))
            .await
            .is_ok()
        {
            sent += 1;
        }
        sent
    });

    gw.wait_until(
        "the gateway disconnects the agent",
        Duration::from_secs(60),
        async || !gw.connected(flood_id).await,
    )
    .await;
    let sent = tokio::time::timeout(STEP, flood)
        .await
        .expect("writes fail once the gateway has closed the connection")
        .expect("join");
    assert!(sent > 0);
    drop(reader);

    gw.assert_healthy().await;
    steady.fence().await;
}

#[tokio::test]
async fn oversized_frames_close_the_connection() {
    let gw = Gateway::start("").await;
    let mut steady = gw.running_agent("steady", json!({})).await;
    let five_mib = 5 * 1024 * 1024;

    // No newline at all.
    let endless_id = gw.create_agent("endless", json!({})).await;
    let mut endless = gw.connect(endless_id).await;
    let _ = endless.writer.write_all(&vec![b'x'; five_mib]).await;
    assert!(matches!(endless.next(STEP).await, Next::Closed));

    // A well-formed request that is too large.
    let large_id = gw.create_agent("large", json!({})).await;
    let mut large = gw.connect(large_id).await;
    let mut frame = serde_json::to_vec(&AgentToGateway::LlmRequest(LlmRequest {
        request_id: "big".into(),
        model: "claude-sonnet-4-6".into(),
        messages: json!([{ "role": "user", "content": "x".repeat(five_mib) }]),
        options: json!({}),
        billing_user_id: None,
    }))
    .expect("encode");
    frame.push(b'\n');
    let _ = large.writer.write_all(&frame).await;
    assert!(matches!(large.next(STEP).await, Next::Closed));

    for id in [endless_id, large_id] {
        gw.wait_until("the agent is disconnected", STEP, async || {
            !gw.connected(id).await
        })
        .await;
    }
    gw.assert_healthy().await;
    steady.fence().await;
}

#[tokio::test]
async fn unsolicited_cron_results_are_not_recorded() {
    let gw = Gateway::start("").await;
    let owner_id = gw.create_agent("owner", json!({})).await;
    let mut owner = gw.connect(owner_id).await;
    let mut other = gw.running_agent("other", json!({})).await;
    // A one-shot far in the future: no run is due, so none is pending.
    let job = gw
        .request(
            reqwest::Method::POST,
            &format!("/agents/{owner_id}/cron"),
            Some(json!({
                "name": "report",
                "prompt": "write the report",
                "schedule": { "at": "2099-01-01T00:00:00Z" },
                "deliver": "local",
            })),
        )
        .await;
    let job_id = job["id"].as_str().expect("job id").to_string();

    for (from_owner, run_id) in [(true, ""), (true, "forged-run"), (false, "")] {
        let agent = if from_owner { &mut owner } else { &mut other };
        agent
            .send(&AgentToGateway::CronResult(CronResult {
                job_id: job_id.clone(),
                run_id: run_id.into(),
                outcome: CronOutcome::Success,
                output: "forged output".into(),
                silent: false,
            }))
            .await;
        agent.fence().await;
    }

    let runs = gw
        .request(
            reqwest::Method::GET,
            &format!("/agents/{owner_id}/cron/{job_id}/runs"),
            None,
        )
        .await;
    assert_eq!(runs, json!([]));
    let output_dir = gw
        .dir
        .path()
        .join("agents")
        .join(owner_id.to_string())
        .join("cron")
        .join(&job_id);
    assert!(!output_dir.exists(), "{} was written", output_dir.display());
}

#[tokio::test]
async fn requests_with_oversized_ids_get_no_reply() {
    let gw = Gateway::start("").await;
    let target_id = gw.create_agent("target", json!({})).await;
    let mut target = gw.connect(target_id).await;
    let agent_id = gw.create_agent("agent", may_query_other_agents()).await;
    let mut agent = gw.connect(agent_id).await;

    agent.send(&llm_request(&"r".repeat(200))).await;
    agent.send(&query(&"q".repeat(200), target_id)).await;
    agent.fence().await;

    agent
        .expect_quiet("requests with oversized ids are dropped")
        .await;
    target
        .expect_quiet("the dropped query is not dispatched")
        .await;
    assert!(gw.connected(agent_id).await);
}

/// The cgroup the spawner put agent `id`'s process in, found from that
/// process rather than from the spawner's naming scheme.
fn agent_cgroup(id: AgentId) -> Option<PathBuf> {
    let marker = format!("--agent-id\0{id}\0");
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            if !String::from_utf8_lossy(&cmdline).contains(&marker) {
                return None;
            }
            let cgroup = std::fs::read_to_string(entry.path().join("cgroup")).ok()?;
            let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
            path.contains(&id.to_string())
                .then(|| PathBuf::from(format!("/sys/fs/cgroup{path}")))
        })
}

#[tokio::test]
#[ignore = "needs root and the namespace spawner; run by the NamespaceSpawner CI job"]
async fn under_isolation_only_the_agents_cgroup_may_connect_as_it() {
    // SAFETY: geteuid has no preconditions.
    assert_eq!(unsafe { libc::geteuid() }, 0, "run as root");
    let gw = Gateway::launch(true, "").await;
    assert!(
        gw.log().contains("agent spawner: NamespaceSpawner"),
        "{}",
        gw.log()
    );
    let a = gw.create_agent("a", json!({})).await;
    let b = gw.create_agent("b", json!({})).await;
    let starts = [gw.start_agent(a), gw.start_agent(b)];
    for id in [a, b] {
        gw.wait_until("the agent's process runs in its cgroup", STEP, async || {
            gw.agent_view(id).await["status"] == json!("running") && agent_cgroup(id).is_some()
        })
        .await;
    }
    let cgroup = |id| agent_cgroup(id).expect("agent cgroup");

    let outside = Link::socket(&gw.socket()).await.hello(&a.to_string()).await;
    assert!(
        outside.is_none(),
        "a process outside the agent's cgroup connected as it"
    );
    let other = Link::from_cgroup(&cgroup(a), &gw.socket())
        .await
        .hello(&b.to_string())
        .await;
    assert!(other.is_none(), "agent {a}'s cgroup connected as agent {b}");
    let mut sessions = Vec::new();
    for id in [a, b] {
        let own = Link::from_cgroup(&cgroup(id), &gw.socket())
            .await
            .hello(&id.to_string())
            .await
            .unwrap_or_else(|| panic!("agent {id}'s own cgroup was refused:\n{}", gw.log()));
        sessions.push(own.reading());
    }
    for start in starts {
        let status = start.await.expect("join");
        assert!(status.is_success(), "start: {status}");
    }
    for agent in &mut sessions {
        agent.fence().await;
    }
    // Best effort: the gateway's exit does not end namespaced agents.
    drop(sessions);
    for id in [a, b] {
        let _ = gw
            .http
            .post(gw.at(&format!("/agents/{id}/stop")))
            .send()
            .await;
    }
}
