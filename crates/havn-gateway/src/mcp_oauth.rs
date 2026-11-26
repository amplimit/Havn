//! OAuth of remote MCP servers (issue #234): the gateway's half.
//!
//! The owner connects a server whose `auth` is OAuth on the agent's settings
//! page. The gateway discovers the server's authorization server as MCP
//! specifies it (protected resource metadata, then the authorization
//! server's metadata), registers itself as a client where the server offers
//! dynamic registration (or uses the client the owner configured), and sends
//! the owner's browser to authorize with a PKCE challenge, a single-use
//! `state` and the server as RFC 8707 `resource`. The provider sends the
//! browser back to `/api/mcp/oauth/callback` on the dashboard's origin; the
//! gateway checks the state belongs to a connect the same user started,
//! exchanges the code with the verifier, and keeps the tokens with the
//! authorization server's metadata and the client, encrypted with
//! `HAVN_AGE_KEY` as agent secrets are, bound to the server URL they were
//! issued for. Tokens are refreshed before they expire; the agent's runtime
//! gets only the access token, for its own servers at that URL.
//!
//! The gateway makes these requests itself, to public addresses only, over
//! https, and only for a server the agent's network policy lets it reach.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::StreamExt as _;
use havn_core::{AgentId, McpAuth, McpServerConfig, Policy, UserId};
use havn_db::repo::agent_secrets;
use havn_db::repo::agents::Agent;
use havn_proto::McpToken;
use oauth2::TokenResponse as _;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationRequest,
    AuthorizationSession, CredentialStore as _, InMemoryCredentialStore, OAuthClientConfig,
    OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRedirectPolicy, OAuthHttpRequest,
    StoredCredentials,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::keyring::KeyRing;

/// Where the provider sends the owner's browser back, under the dashboard's
/// `/api`.
pub const CALLBACK_PATH: &str = "/mcp/oauth/callback";
/// How long a connect waits for its callback.
const PENDING_FOR: Duration = Duration::from_secs(10 * 60);
/// Connects waiting at once, on the whole gateway.
const MAX_PENDING: usize = 64;
/// Tokens are refreshed when they expire within this.
pub const REFRESH_AHEAD: chrono::Duration = chrono::Duration::minutes(5);
/// How long one OAuth request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of an OAuth response read at most.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Redirects an OAuth request follows, where rmcp lets it.
const MAX_REDIRECTS: usize = 5;
/// The client name the gateway registers as.
const CLIENT_NAME: &str = "Havn";

/// What a server was connected with, kept encrypted.
#[derive(Clone, Serialize, Deserialize)]
struct Grant {
    /// The server URL the tokens were issued for.
    url: String,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: String,
    scopes: Vec<String>,
    metadata: AuthorizationMetadata,
    credentials: StoredCredentials,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("url", &self.url)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl Grant {
    fn access_token(&self) -> Option<String> {
        self.credentials
            .token_response
            .as_ref()
            .map(|t| t.access_token().secret().clone())
    }

    /// When the access token expires, when the provider said.
    fn expires_at(&self) -> Option<DateTime<Utc>> {
        let token = self.credentials.token_response.as_ref()?;
        let lifetime = token.expires_in()?;
        let received = self.credentials.token_received_at?;
        DateTime::from_timestamp(
            i64::try_from(received.saturating_add(lifetime.as_secs())).ok()?,
            0,
        )
    }
}

/// A connect waiting for the provider to send the owner back.
struct Pending {
    agent_id: AgentId,
    user_id: UserId,
    server: String,
    url: String,
    session: AuthorizationSession,
    store: InMemoryCredentialStore,
    client_secret: Option<String>,
    metadata: AuthorizationMetadata,
    started: Instant,
}

type Grants = BTreeMap<String, Grant>;

/// The OAuth of every agent's remote MCP servers.
#[derive(Clone)]
pub struct McpOAuth {
    db: SqlitePool,
    keyring: Option<KeyRing>,
    http: Arc<Http>,
    pending: Arc<std::sync::Mutex<HashMap<String, Pending>>>,
    /// Per agent, its grants once decrypted; each slot's lock serializes
    /// the agent's reads and changes.
    slots: Arc<Mutex<HashMap<AgentId, Arc<Mutex<Option<Grants>>>>>>,
}

impl std::fmt::Debug for McpOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuth").finish_non_exhaustive()
    }
}

/// Why connecting, or refreshing, failed.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("tokens are not encrypted on this gateway: set HAVN_AGE_KEY and restart the gateway")]
    NoKey,
    #[error("{0}")]
    Refused(String),
    #[error("this connect is unknown or expired; connect the server again")]
    UnknownState,
    #[error("the authorization server: {0}")]
    Provider(String),
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Internal(String),
}

impl From<AuthError> for OAuthError {
    fn from(e: AuthError) -> Self {
        Self::Provider(e.to_string())
    }
}

/// How one server's OAuth is, for the settings page.
#[derive(Debug, Clone, Serialize)]
pub struct OAuthStatus {
    pub url: String,
    pub connected_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl McpOAuth {
    pub fn new(db: SqlitePool, keyring: Option<KeyRing>) -> Self {
        Self {
            db,
            keyring,
            http: Arc::new(Http::new()),
            pending: Arc::default(),
            slots: Arc::default(),
        }
    }

    async fn slot(&self, agent_id: AgentId) -> Arc<Mutex<Option<Grants>>> {
        Arc::clone(self.slots.lock().await.entry(agent_id).or_default())
    }

    /// The agent's grants, decrypted once.
    async fn loaded(&self, agent_id: AgentId, held: &mut Option<Grants>) -> Grants {
        if let Some(grants) = held.as_ref() {
            return grants.clone();
        }
        let mut grants = Grants::new();
        let Some(ring) = self.keyring.clone() else {
            return grants;
        };
        let rows = match agent_secrets::oauth_list(&self.db, agent_id).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(%agent_id, error = %e, "reading the agent's MCP OAuth failed");
                return grants;
            }
        };
        for row in rows {
            let ring = ring.clone();
            let ciphertext = row.value_ciphertext;
            let plain = tokio::task::spawn_blocking(move || ring.decrypt(&ciphertext)).await;
            match plain.map(|p| p.map(|bytes| serde_json::from_slice::<Grant>(&bytes))) {
                Ok(Ok(Ok(grant))) => {
                    grants.insert(row.server, grant);
                }
                _ => {
                    warn!(%agent_id, server = %row.server, "an MCP server's OAuth cannot be read; left out")
                }
            }
        }
        *held = Some(grants.clone());
        grants
    }

    async fn save(&self, agent_id: AgentId, server: &str, grant: &Grant) -> Result<(), OAuthError> {
        let ring = self.keyring.clone().ok_or(OAuthError::NoKey)?;
        let plain = serde_json::to_vec(grant).map_err(|e| OAuthError::Internal(e.to_string()))?;
        let ciphertext = tokio::task::spawn_blocking(move || ring.encrypt(&plain))
            .await
            .map_err(|e| OAuthError::Internal(e.to_string()))?
            .map_err(|e| OAuthError::Internal(e.to_string()))?;
        agent_secrets::oauth_set(
            &self.db,
            agent_id,
            server,
            &grant.url,
            &ciphertext,
            grant.expires_at(),
        )
        .await
        .map_err(|e| OAuthError::Internal(e.to_string()))
    }

    /// Start connecting `server` of `agent` for its owner `user_id`: the URL
    /// to send the owner's browser to. `cfg` is the server as the agent runs
    /// it, its OAuth client secret resolved from `secrets`.
    pub async fn start(
        &self,
        agent: &Agent,
        user_id: UserId,
        server: &str,
        cfg: &McpServerConfig,
        redirect_uri: &str,
        secrets: &BTreeMap<String, String>,
    ) -> Result<String, OAuthError> {
        if self.keyring.is_none() {
            return Err(OAuthError::NoKey);
        }
        let McpAuth::Oauth(oauth) = &cfg.auth else {
            return Err(OAuthError::Refused(format!(
                "MCP server {server:?} does not use OAuth"
            )));
        };
        let client_secret = if oauth.client_secret.is_empty() {
            None
        } else {
            Some(resolve(&oauth.client_secret, secrets)?)
        };
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(cfg.url.as_str(), self.http.clone())
                .await?;
        let resolution = manager.resolve_metadata().await?;
        let metadata = resolution.metadata.clone();
        manager.set_metadata(resolution.metadata);
        let store = InMemoryCredentialStore::new();
        manager.set_credential_store(store.clone());
        let mut request = AuthorizationRequest::new(redirect_uri).with_client_name(CLIENT_NAME);
        if !oauth.scopes.is_empty() {
            request = request.with_scopes(oauth.scopes.iter().cloned());
        }
        if !oauth.client_id.is_empty() {
            request = request.with_preregistered_client(oauth.client_id.clone());
            if let Some(secret) = &client_secret {
                request = request.with_client_secret(secret.clone());
            }
        } else {
            // A client this gateway registered for the server before, for
            // the same redirect.
            let slot = self.slot(agent.id).await;
            let mut held = slot.lock().await;
            let grants = self.loaded(agent.id, &mut held).await;
            if let Some(old) = grants
                .get(server)
                .filter(|g| g.url == cfg.url && g.redirect_uri == redirect_uri)
            {
                request = request.with_preregistered_client(old.client_id.clone());
                if let Some(secret) = &old.client_secret {
                    request = request.with_client_secret(secret.clone());
                }
            }
        }
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| OAuthError::from(e))?;
        let state = reqwest::Url::parse(&session.auth_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
            })
            .ok_or_else(|| OAuthError::Internal("the authorization URL has no state".into()))?;
        let url = session.auth_url.clone();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|_, p| p.started.elapsed() < PENDING_FOR);
        if pending.len() >= MAX_PENDING {
            return Err(OAuthError::Refused(
                "too many connects wait for their providers; try again in a few minutes".into(),
            ));
        }
        pending.insert(
            state,
            Pending {
                agent_id: agent.id,
                user_id,
                server: server.to_string(),
                url: cfg.url.clone(),
                session,
                store,
                client_secret,
                metadata,
                started: Instant::now(),
            },
        );
        Ok(url)
    }

    /// The provider sent `user_id`'s browser back with `state` and `code`:
    /// exchange the code and keep the tokens. The agent and server.
    pub async fn finish(
        &self,
        user_id: UserId,
        state: &str,
        code: &str,
        issuer: Option<&str>,
    ) -> Result<(AgentId, String), OAuthError> {
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Only the user who started it may finish it; anyone else's
            // attempt leaves it for them.
            match pending.get(state) {
                Some(p) if p.user_id == user_id && p.started.elapsed() < PENDING_FOR => {
                    pending.remove(state)
                }
                _ => None,
            }
        }
        .ok_or(OAuthError::UnknownState)?;
        pending
            .session
            .handle_callback_with_issuer(code, state, issuer)
            .await?;
        let credentials = pending
            .store
            .load()
            .await?
            .ok_or_else(|| OAuthError::Provider("no token came back".into()))?;
        let manager = &pending.session.auth_manager;
        let (client_id, _) = manager.get_credentials().await?;
        let grant = Grant {
            url: pending.url,
            client_id,
            client_secret: pending.client_secret,
            redirect_uri: pending.session.redirect_uri.clone(),
            scopes: credentials.granted_scopes.clone(),
            metadata: pending.metadata,
            credentials,
        };
        let slot = self.slot(pending.agent_id).await;
        let mut held = slot.lock().await;
        let mut grants = self.loaded(pending.agent_id, &mut held).await;
        self.save(pending.agent_id, &pending.server, &grant).await?;
        grants.insert(pending.server.clone(), grant);
        *held = Some(grants);
        info!(agent_id = %pending.agent_id, server = %pending.server, "MCP server connected with OAuth");
        Ok((pending.agent_id, pending.server))
    }

    /// The agent and server of `user_id`'s connect waiting as `state`.
    pub fn target(&self, user_id: UserId, state: &str) -> Option<(AgentId, String)> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(state)
            .filter(|p| p.user_id == user_id)
            .map(|p| (p.agent_id, p.server.clone()))
    }

    /// End `user_id`'s connect waiting as `state` without tokens (the
    /// provider refused); its agent and server.
    pub fn abandon(&self, user_id: UserId, state: &str) -> Option<(AgentId, String)> {
        let target = self.target(user_id, state)?;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(state);
        Some(target)
    }

    /// Forget what `server` was connected with; whether it was.
    pub async fn disconnect(&self, agent_id: AgentId, server: &str) -> Result<bool, OAuthError> {
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let was = agent_secrets::oauth_remove(&self.db, agent_id, server)
            .await
            .map_err(|e| OAuthError::Internal(e.to_string()))?;
        if let Some(grants) = held.as_mut() {
            grants.remove(server);
        }
        Ok(was)
    }

    /// Forget what was read of a deleted agent.
    pub async fn forget(&self, agent_id: AgentId) {
        self.slots.lock().await.remove(&agent_id);
    }

    /// The access tokens of `own` servers of `policy` (the agent's, as
    /// resolved) that use OAuth, each for the URL it was issued for, when
    /// that is the server's URL.
    pub async fn tokens(
        &self,
        agent_id: AgentId,
        policy: &Policy,
        own: &[String],
    ) -> BTreeMap<String, McpToken> {
        let wanted: Vec<(&String, &McpServerConfig)> = policy
            .mcp_servers
            .iter()
            .filter(|(name, cfg)| {
                own.contains(name) && matches!(cfg.auth, McpAuth::Oauth(_)) && cfg.is_remote()
            })
            .collect();
        if wanted.is_empty() {
            return BTreeMap::new();
        }
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let grants = self.loaded(agent_id, &mut held).await;
        wanted
            .into_iter()
            .filter_map(|(name, cfg)| {
                let grant = grants.get(name).filter(|g| g.url == cfg.url)?;
                Some((
                    name.clone(),
                    McpToken {
                        url: grant.url.clone(),
                        access_token: grant.access_token()?,
                    },
                ))
            })
            .collect()
    }

    /// Each connected server's OAuth, by name.
    pub async fn status(&self, agent_id: AgentId) -> BTreeMap<String, OAuthStatus> {
        match agent_secrets::oauth_list(&self.db, agent_id).await {
            Ok(rows) => rows
                .into_iter()
                .map(|row| {
                    (
                        row.server,
                        OAuthStatus {
                            url: row.url,
                            connected_at: row.updated_at,
                            expires_at: row.expires_at,
                            error: row.error,
                        },
                    )
                })
                .collect(),
            Err(e) => {
                warn!(%agent_id, error = %e, "reading the agent's MCP OAuth failed");
                BTreeMap::new()
            }
        }
    }

    /// Refresh the access token of `server`. A refusal is kept as the
    /// server's error, for the owner to connect it again.
    pub async fn refresh(&self, agent_id: AgentId, server: &str) -> Result<(), OAuthError> {
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let mut grants = self.loaded(agent_id, &mut held).await;
        let Some(grant) = grants.get(server).cloned() else {
            return Ok(());
        };
        match self.refreshed(&grant).await {
            Ok(credentials) => {
                let grant = Grant {
                    credentials,
                    ..grant
                };
                self.save(agent_id, server, &grant).await?;
                grants.insert(server.to_string(), grant);
                *held = Some(grants);
                info!(%agent_id, server, "MCP server's OAuth token refreshed");
                Ok(())
            }
            Err(e) => {
                let why = match &e {
                    OAuthError::Provider(_) => format!("refreshing the token failed: {e}"),
                    other => other.to_string(),
                };
                // A refusal ends the grant: its token is dropped, and it is
                // not tried again until the owner connects the server again.
                if matches!(e, OAuthError::Rejected(_)) {
                    let mut ended = grant;
                    ended.credentials.token_response = None;
                    self.save(agent_id, server, &ended).await?;
                    grants.insert(server.to_string(), ended);
                    *held = Some(grants);
                }
                if let Err(db) = agent_secrets::oauth_error(&self.db, agent_id, server, &why).await
                {
                    warn!(%agent_id, server, error = %db, "could not note a failed refresh");
                }
                Err(e)
            }
        }
    }

    /// Refresh the agent's tokens that expire soon, before it starts.
    pub async fn refresh_due(&self, agent_id: AgentId) {
        let soon = Utc::now() + REFRESH_AHEAD;
        for (server, status) in self.status(agent_id).await {
            if status.expires_at.is_some_and(|at| at < soon) {
                if let Err(e) = self.refresh(agent_id, &server).await {
                    warn!(%agent_id, server, error = %e, "an MCP server's OAuth token was not refreshed");
                }
            }
        }
    }

    async fn refreshed(&self, grant: &Grant) -> Result<StoredCredentials, OAuthError> {
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(grant.url.as_str(), self.http.clone())
                .await?;
        // Discovery again, for the resource the server names itself as and
        // the authorization server it uses now, which must be the one that
        // issued the tokens.
        let metadata = match manager.resolve_metadata().await {
            Ok(resolution) if resolution.metadata.issuer == grant.metadata.issuer => {
                resolution.metadata
            }
            Ok(_) => {
                return Err(OAuthError::Refused(
                    "the server's authorization server changed; connect it again".into(),
                ));
            }
            Err(_) => grant.metadata.clone(),
        };
        manager.set_metadata(metadata);
        let mut client =
            OAuthClientConfig::new(grant.client_id.clone(), grant.redirect_uri.clone())
                .with_scopes(grant.scopes.clone());
        if let Some(secret) = &grant.client_secret {
            client = client.with_client_secret(secret.clone());
        }
        manager.configure_client(client)?;
        let store = InMemoryCredentialStore::new();
        store.save(grant.credentials.clone()).await?;
        manager.set_credential_store(store.clone());
        manager.refresh_token().await.map_err(|e| match e {
            AuthError::TokenRefreshRejected(_) | AuthError::AuthorizationRequired => {
                OAuthError::Rejected(format!(
                    "the authorization server refused to refresh the token ({e}); connect the \
                     server again"
                ))
            }
            other => OAuthError::from(other),
        })?;
        store
            .load()
            .await?
            .ok_or_else(|| OAuthError::Provider("no token came back".into()))
    }

    /// Keep `access_token` for `server` of the agent, issued for `url`, as a
    /// connect would.
    #[cfg(test)]
    pub async fn keep_for_test(
        &self,
        agent_id: AgentId,
        server: &str,
        url: &str,
        access_token: &str,
    ) {
        let token = serde_json::from_value(serde_json::json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": 3600,
        }))
        .expect("token");
        let received = u64::try_from(Utc::now().timestamp()).unwrap_or_default();
        let grant = Grant {
            url: url.to_string(),
            client_id: "havn".into(),
            client_secret: None,
            redirect_uri: "http://localhost:3000/api/mcp/oauth/callback".into(),
            scopes: Vec::new(),
            metadata: AuthorizationMetadata::default(),
            credentials: StoredCredentials::new(
                "havn".into(),
                Some(token),
                Vec::new(),
                Some(received),
            ),
        };
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let mut grants = self.loaded(agent_id, &mut held).await;
        self.save(agent_id, server, &grant).await.expect("save");
        grants.insert(server.to_string(), grant);
        *held = Some(grants);
    }

    /// The agents and servers whose tokens expire soon.
    pub async fn expiring(&self) -> Vec<(AgentId, String)> {
        match agent_secrets::oauth_expiring(&self.db, Utc::now() + REFRESH_AHEAD).await {
            Ok(due) => due,
            Err(e) => {
                warn!(error = %e, "reading MCP OAuth tokens to refresh failed");
                Vec::new()
            }
        }
    }
}

/// `value`, or the agent's secret it names.
fn resolve(value: &str, secrets: &BTreeMap<String, String>) -> Result<String, OAuthError> {
    use havn_core::secret_ref::{self, SecretRef};
    if !value.starts_with(secret_ref::PREFIX) {
        return Ok(value.to_string());
    }
    match secret_ref::parse(value) {
        Ok(SecretRef::Agent { name }) => secrets.get(&name).cloned().ok_or_else(|| {
            OAuthError::Refused(format!(
                "the client secret {value} is not set; set it in the agent's Secrets"
            ))
        }),
        _ => Err(OAuthError::Refused(format!(
            "{value} is not one of the agent's secrets"
        ))),
    }
}

/// The gateway's HTTP for OAuth: https to public addresses only, at most
/// [`MAX_RESPONSE_BYTES`] read.
struct Http {
    follow: reqwest::Client,
    stop: reqwest::Client,
}

impl Http {
    fn new() -> Self {
        let build = |redirect: reqwest::redirect::Policy| {
            reqwest::Client::builder()
                .dns_resolver(Arc::new(crate::web_fetch::PublicResolver))
                .no_proxy()
                .https_only(true)
                .redirect(redirect)
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(Duration::from_secs(10))
                .user_agent(concat!("Havn/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default()
        };
        let follow = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if let Err(why) = check_url(attempt.url()) {
                attempt.error(why)
            } else {
                attempt.follow()
            }
        });
        Self {
            follow: build(follow),
            stop: build(reqwest::redirect::Policy::none()),
        }
    }
}

/// https, and an address in it only when it is public.
fn check_url(url: &reqwest::Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err(format!("{url}: OAuth goes over https only"));
    }
    match url.host() {
        Some(url::Host::Domain(_)) => Ok(()),
        Some(url::Host::Ipv4(ip)) if crate::web_fetch::is_public(IpAddr::V4(ip)) => Ok(()),
        Some(url::Host::Ipv6(ip)) if crate::web_fetch::is_public(IpAddr::V6(ip)) => Ok(()),
        _ => Err(format!("{url}: not a public address")),
    }
}

impl OAuthHttpClient for Http {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let OAuthHttpRequest {
                request,
                redirect_policy,
                ..
            } = request;
            let client = match redirect_policy {
                OAuthHttpRedirectPolicy::Follow => &self.follow,
                _ => &self.stop,
            };
            let request = reqwest::Request::try_from(request)?;
            check_url(request.url())?;
            let response = client.execute(request).await?;
            let mut builder = oauth2::http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk?;
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err("an OAuth response over 1 MiB".into());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(builder.body(body)?)
        })
    }
}

/// Refresh the tokens that expire soon, every minute, and send each running
/// agent its new ones.
pub async fn refresh_loop(
    secrets: crate::secrets::SecretStore,
    registry: crate::agent_registry::AgentRegistry,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let oauth = secrets.oauth();
        for (agent_id, server) in oauth.expiring().await {
            if !registry.is_connected(agent_id).await {
                continue;
            }
            match oauth.refresh(agent_id, &server).await {
                Ok(()) => secrets.push(&registry, agent_id).await,
                Err(e) => {
                    warn!(%agent_id, server, error = %e, "an MCP server's OAuth token was not refreshed")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn oauth_requests_go_to_public_https_only() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(check_url(&url("https://auth.example.com/token")).is_ok());
        assert!(check_url(&url("https://1.1.1.1/token")).is_ok());
        for bad in [
            "http://auth.example.com/token",
            "https://127.0.0.1/token",
            "https://10.0.0.5/token",
            "https://169.254.169.254/latest",
            "https://[::1]/token",
        ] {
            assert!(check_url(&url(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_client_secret_reference_is_the_agents_secret() {
        let secrets = BTreeMap::from([("gh-client".to_string(), "s3cr3t-value".to_string())]);
        assert_eq!(resolve("plain", &secrets).unwrap(), "plain");
        assert_eq!(
            resolve("secret:gh-client", &secrets).unwrap(),
            "s3cr3t-value"
        );
        assert!(resolve("secret:other", &secrets).is_err());
        assert!(resolve("secret:channel:x:y", &secrets).is_err());
    }

    #[tokio::test]
    async fn a_callback_finishes_only_the_connect_its_user_started() {
        let db = havn_db::connect_in_memory().await.expect("db");
        let oauth = McpOAuth::new(db, Some(KeyRing::from_passphrase("test-key")));
        assert!(matches!(
            oauth
                .finish(UserId::new(), "no-such-state", "code", None)
                .await,
            Err(OAuthError::UnknownState)
        ));
    }
}
