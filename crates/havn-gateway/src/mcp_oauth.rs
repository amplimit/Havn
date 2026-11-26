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
//! Every endpoint the authorization server names is checked the same way
//! before it is used, so its metadata cannot send the owner's browser to a
//! `javascript:` URL or the gateway to a private address.

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
/// Connects one user may have waiting at once.
const MAX_PENDING_PER_USER: usize = 8;
/// Connects waiting at once, on the whole gateway.
const MAX_PENDING: usize = 1024;
/// A token is refreshed this long before it expires at most, and at half
/// its lifetime when that is shorter.
pub const REFRESH_AHEAD: chrono::Duration = chrono::Duration::minutes(5);
/// How long one OAuth request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the background loop waits for one agent's refreshes; they go on
/// past it.
pub const REFRESH_WAIT: Duration = Duration::from_secs(60);
/// How long the discovery before a refresh may take; past it the metadata
/// kept from the connect is used.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// After a failed refresh, the next try waits this, doubling up to
/// [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// Agents whose tokens are refreshed at once in the background.
const PARALLEL_REFRESHES: usize = 8;
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
    /// The client is one this gateway registered, not the owner's.
    #[serde(default)]
    registered: bool,
    /// The secret registration gave that client, when it gave one. The
    /// owner's client secret is not kept: it is resolved when it is used.
    #[serde(default)]
    registered_secret: Option<String>,
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

    fn refresh_token(&self) -> Option<String> {
        self.credentials
            .token_response
            .as_ref()
            .and_then(|t| t.refresh_token())
            .map(|t| t.secret().clone())
    }

    /// The access token's lifetime and when it expires, when the provider
    /// said.
    fn lifetime(&self) -> Option<(chrono::Duration, DateTime<Utc>)> {
        let token = self.credentials.token_response.as_ref()?;
        let lifetime = chrono::Duration::from_std(token.expires_in()?).ok()?;
        let received =
            DateTime::from_timestamp(i64::try_from(self.credentials.token_received_at?).ok()?, 0)?;
        Some((lifetime, received + lifetime))
    }

    fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.lifetime().map(|(_, at)| at)
    }

    /// When the access token is refreshed: [`REFRESH_AHEAD`] before it
    /// expires, or half its lifetime when that is shorter.
    fn refresh_at(&self) -> Option<DateTime<Utc>> {
        let (lifetime, expires) = self.lifetime()?;
        Some(expires - REFRESH_AHEAD.min(lifetime / 2))
    }

    /// The client secret: the one registration gave, or the owner's.
    fn client_secret(&self, owner_secret: Option<&str>) -> Option<String> {
        if self.registered {
            self.registered_secret.clone()
        } else {
            owner_secret.map(str::to_string)
        }
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
    registered: bool,
    registered_secret: Option<String>,
    metadata: AuthorizationMetadata,
    started: Instant,
}

type Grants = BTreeMap<String, Grant>;

/// The OAuth grants of every agent's remote MCP servers.
#[derive(Clone)]
pub struct OAuthGrants {
    db: SqlitePool,
    keyring: Option<KeyRing>,
    http: Arc<dyn OAuthHttpClient>,
    /// The client revocations are sent with.
    revoker: reqwest::Client,
    pending: Arc<std::sync::Mutex<HashMap<String, Pending>>>,
    /// Per agent, its grants once decrypted; each slot's lock serializes
    /// the agent's reads and changes, and is never held across a request.
    slots: Arc<Mutex<HashMap<AgentId, Arc<Mutex<Option<Grants>>>>>>,
    /// Per agent and server, held while its token is refreshed: one refresh
    /// of a server at a time, apart from the reads.
    refreshing: Arc<Mutex<HashMap<(AgentId, String), Arc<Mutex<()>>>>>,
    /// Refreshes that failed, by agent and server: how many in a row, and
    /// when the next may be tried.
    failures: Arc<std::sync::Mutex<HashMap<(AgentId, String), (u32, Instant)>>>,
}

impl std::fmt::Debug for OAuthGrants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthGrants").finish_non_exhaustive()
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
    /// The provider refused the refresh token: the grant ended.
    #[error("{0}")]
    Rejected(String),
    /// The provider gave no refresh token: the access token lasts until it
    /// expires.
    #[error("{0}")]
    NoRefresh(String),
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

/// The servers of `policy` (the agent's, as resolved) whose OAuth tokens
/// are used: the agent's `own`, remote, OAuth, at a host its network
/// reaches and does not block. The operator's `[mcp]` is applied to the
/// policy already.
pub fn served<'a>(policy: &'a Policy, own: &[String]) -> Vec<(&'a String, &'a McpServerConfig)> {
    let net = &policy.network_policy;
    if !(policy.permissions.can_access_network && net.egress_allowed) {
        return Vec::new();
    }
    policy
        .mcp_servers
        .iter()
        .filter(|(name, cfg)| {
            own.contains(name)
                && matches!(cfg.auth, McpAuth::Oauth(_))
                && cfg.remote_host().is_some_and(|host| !net.is_blocked(&host))
        })
        .collect()
}

/// Whether `now` is the grant `was`: not connected again, refreshed or ended
/// since.
fn same_grant(now: &Grant, was: &Grant) -> bool {
    now.url == was.url
        && now.client_id == was.client_id
        && now.refresh_token() == was.refresh_token()
        && now.access_token() == was.access_token()
}

/// Whether `grant` is still for `cfg`: its URL, and its client when the
/// owner configured one.
fn grant_fits(grant: &Grant, cfg: &McpServerConfig) -> bool {
    let McpAuth::Oauth(oauth) = &cfg.auth else {
        return false;
    };
    grant.url == cfg.url
        && if oauth.client_id.is_empty() {
            grant.registered
        } else {
            !grant.registered && grant.client_id == oauth.client_id
        }
}

impl OAuthGrants {
    pub fn new(db: SqlitePool, keyring: Option<KeyRing>) -> Self {
        let http = Http::new();
        Self {
            db,
            keyring,
            revoker: http.stop.clone(),
            http: Arc::new(http),
            pending: Arc::default(),
            slots: Arc::default(),
            refreshing: Arc::default(),
            failures: Arc::default(),
        }
    }

    /// OAuth requests made through `http`.
    #[cfg(test)]
    pub fn with_http(mut self, http: Arc<dyn OAuthHttpClient>) -> Self {
        self.http = http;
        self
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

    async fn encrypt(&self, grant: &Grant) -> Result<Vec<u8>, OAuthError> {
        let ring = self.keyring.clone().ok_or(OAuthError::NoKey)?;
        let plain = serde_json::to_vec(grant).map_err(|e| OAuthError::Internal(e.to_string()))?;
        tokio::task::spawn_blocking(move || ring.encrypt(&plain))
            .await
            .map_err(|e| OAuthError::Internal(e.to_string()))?
            .map_err(|e| OAuthError::Internal(e.to_string()))
    }

    async fn store(
        &self,
        agent_id: AgentId,
        server: &str,
        grant: &Grant,
        ciphertext: &[u8],
    ) -> Result<(), OAuthError> {
        agent_secrets::oauth_set(
            &self.db,
            agent_id,
            server,
            &grant.url,
            ciphertext,
            grant.expires_at(),
        )
        .await
        .map_err(|e| OAuthError::Internal(e.to_string()))
    }

    async fn save(&self, agent_id: AgentId, server: &str, grant: &Grant) -> Result<(), OAuthError> {
        let ciphertext = self.encrypt(grant).await?;
        self.store(agent_id, server, grant, &ciphertext).await
    }

    /// `server`'s grant as it is now.
    async fn grant(&self, agent_id: AgentId, server: &str) -> Option<Grant> {
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        self.loaded(agent_id, &mut held).await.get(server).cloned()
    }

    /// Put `next` in place of `server`'s grant when that is still `was`: not
    /// connected again, disconnected or pruned meanwhile. Whether it was.
    /// Encrypted before the agent's lock is taken.
    async fn replace(
        &self,
        agent_id: AgentId,
        server: &str,
        was: &Grant,
        next: Grant,
    ) -> Result<bool, OAuthError> {
        let ciphertext = self.encrypt(&next).await?;
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let mut grants = self.loaded(agent_id, &mut held).await;
        if !grants.get(server).is_some_and(|now| same_grant(now, was)) {
            return Ok(false);
        }
        self.store(agent_id, server, &next, &ciphertext).await?;
        grants.insert(server.to_string(), next);
        *held = Some(grants);
        Ok(true)
    }

    /// Note why refreshing `server` failed, when its grant is still `was`;
    /// whether it was.
    async fn note_failure(&self, agent_id: AgentId, server: &str, was: &Grant, why: &str) -> bool {
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let grants = self.loaded(agent_id, &mut held).await;
        if !grants.get(server).is_some_and(|now| same_grant(now, was)) {
            return false;
        }
        if let Err(db) = agent_secrets::oauth_error(&self.db, agent_id, server, why).await {
            warn!(%agent_id, server, error = %db, "could not note a failed refresh");
        }
        true
    }

    /// The lock one refresh of `server` holds.
    async fn refresh_gate(&self, agent_id: AgentId, server: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.refreshing
                .lock()
                .await
                .entry((agent_id, server.to_string()))
                .or_default(),
        )
    }

    /// Start connecting `server` of `agent` for its owner `user_id`: the URL
    /// to send the owner's browser to, always https. `cfg` is the server as
    /// the agent runs it; `owner_secret` its OAuth client secret, resolved.
    pub async fn start(
        &self,
        agent: &Agent,
        user_id: UserId,
        server: &str,
        cfg: &McpServerConfig,
        redirect_uri: &str,
        owner_secret: Option<String>,
    ) -> Result<String, OAuthError> {
        if self.keyring.is_none() {
            return Err(OAuthError::NoKey);
        }
        let McpAuth::Oauth(oauth) = &cfg.auth else {
            return Err(OAuthError::Refused(format!(
                "MCP server {server:?} does not use OAuth"
            )));
        };
        self.room_for(user_id)?;
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(cfg.url.as_str(), self.http.clone())
                .await?;
        let resolution = manager.resolve_metadata().await?;
        check_metadata(&resolution.metadata)?;
        let metadata = resolution.metadata.clone();
        manager.set_metadata(resolution.metadata);
        let store = InMemoryCredentialStore::new();
        manager.set_credential_store(store.clone());
        let mut request = AuthorizationRequest::new(redirect_uri).with_client_name(CLIENT_NAME);
        if !oauth.scopes.is_empty() {
            request = request.with_scopes(oauth.scopes.iter().cloned());
        }
        // The client: the owner's, one this gateway registered for this
        // server and redirect before, or one it registers now; its secret
        // goes with it.
        let (client_id, client_secret, registered) = if !oauth.client_id.is_empty() {
            (oauth.client_id.clone(), owner_secret, false)
        } else {
            let earlier = {
                let slot = self.slot(agent.id).await;
                let mut held = slot.lock().await;
                self.loaded(agent.id, &mut held)
                    .await
                    .get(server)
                    .filter(|g| g.registered && g.url == cfg.url && g.redirect_uri == redirect_uri)
                    .map(|g| (g.client_id.clone(), g.registered_secret.clone()))
            };
            match earlier {
                Some((id, secret)) => (id, secret, true),
                None => {
                    if metadata.registration_endpoint.is_none() {
                        return Err(OAuthError::Refused(
                            "the authorization server offers no client registration: set the \
                             client id of an app registered there"
                                .into(),
                        ));
                    }
                    let scopes: Vec<&str> = oauth.scopes.iter().map(String::as_str).collect();
                    let client = manager
                        .register_client(CLIENT_NAME, redirect_uri, &scopes)
                        .await?;
                    (client.client_id, client.client_secret, true)
                }
            }
        };
        request = request.with_preregistered_client(client_id);
        if let Some(secret) = &client_secret {
            request = request.with_client_secret(secret.clone());
        }
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| OAuthError::from(e))?;
        let url = reqwest::Url::parse(&session.auth_url)
            .map_err(|e| OAuthError::Provider(format!("the authorization URL: {e}")))?;
        check_url(&url).map_err(OAuthError::Refused)?;
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| OAuthError::Internal("the authorization URL has no state".into()))?;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A state is ours and new: one already waiting is never replaced.
        if pending.contains_key(&state) {
            return Err(OAuthError::Refused(
                "the authorization URL's state is not this connect's".into(),
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
                registered,
                registered_secret: if registered { client_secret } else { None },
                metadata,
                started: Instant::now(),
            },
        );
        Ok(url.to_string())
    }

    /// Whether `user_id` may start another connect.
    fn room_for(&self, user_id: UserId) -> Result<(), OAuthError> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|_, p| p.started.elapsed() < PENDING_FOR);
        let theirs = pending.values().filter(|p| p.user_id == user_id).count();
        if theirs >= MAX_PENDING_PER_USER || pending.len() >= MAX_PENDING {
            return Err(OAuthError::Refused(
                "too many connects wait for their providers; try again in a few minutes".into(),
            ));
        }
        Ok(())
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
            registered: pending.registered,
            registered_secret: pending.registered_secret,
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
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(pending.agent_id, pending.server.clone()));
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

    /// Forget what `server` was connected with, and revoke its token at the
    /// provider without waiting; whether it was. `owner_secret` is the
    /// owner's client secret, for a revocation that needs it.
    pub async fn disconnect(
        &self,
        agent_id: AgentId,
        server: &str,
        owner_secret: Option<String>,
    ) -> Result<bool, OAuthError> {
        self.remove_unless(agent_id, server, owner_secret, |_| false)
            .await
    }

    /// Remove `server`'s grant, unless `keep` says otherwise of it as it is
    /// under the agent's lock, and revoke it; whether one was removed.
    async fn remove_unless(
        &self,
        agent_id: AgentId,
        server: &str,
        owner_secret: Option<String>,
        keep: impl Fn(&Grant) -> bool,
    ) -> Result<bool, OAuthError> {
        let removed = {
            let slot = self.slot(agent_id).await;
            let mut held = slot.lock().await;
            let mut grants = self.loaded(agent_id, &mut held).await;
            if grants.get(server).is_some_and(&keep) {
                return Ok(false);
            }
            let was = agent_secrets::oauth_remove(&self.db, agent_id, server)
                .await
                .map_err(|e| OAuthError::Internal(e.to_string()))?;
            let grant = grants.remove(server);
            *held = Some(grants);
            if let Some(grant) = grant {
                self.revoke(grant, owner_secret);
            }
            was
        };
        self.clear_server(agent_id, server).await;
        Ok(removed)
    }

    /// Forget the refresh state of `server`, whose grant is gone.
    async fn clear_server(&self, agent_id: AgentId, server: &str) {
        let key = (agent_id, server.to_string());
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        self.refreshing.lock().await.remove(&key);
    }

    /// Forget the grants of the agent's servers that are gone from `servers`
    /// (its own settings), or whose URL, authorization or client changed:
    /// their tokens are for something else now. They are revoked with the
    /// client secret of the server as it was (`before`), resolved from the
    /// agent's `secrets`.
    pub async fn prune(
        &self,
        agent_id: AgentId,
        servers: &HashMap<String, McpServerConfig>,
        before: &HashMap<String, McpServerConfig>,
        secrets: &BTreeMap<String, String>,
    ) {
        let stale: Vec<String> = {
            let slot = self.slot(agent_id).await;
            let mut held = slot.lock().await;
            self.loaded(agent_id, &mut held)
                .await
                .iter()
                .filter(|(name, grant)| {
                    servers.get(*name).is_none_or(|cfg| !grant_fits(grant, cfg))
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        for server in stale {
            let secret = before
                .get(&server)
                .and_then(|cfg| owner_secret(cfg, secrets).ok())
                .flatten();
            // A connect that finished meanwhile, for the server as it is
            // now, stays.
            let fits = |grant: &Grant| {
                servers
                    .get(&server)
                    .is_some_and(|cfg| grant_fits(grant, cfg))
            };
            match self.remove_unless(agent_id, &server, secret, fits).await {
                Ok(false) => {}
                Ok(true) => {
                    info!(%agent_id, server, "an MCP server's OAuth is forgotten: the server changed")
                }
                Err(e) => {
                    warn!(%agent_id, server, error = %e, "could not forget an MCP server's OAuth")
                }
            }
        }
    }

    /// Revoke `grant`'s refresh token (or its access token without one) at
    /// the provider's revocation endpoint, when it has one (RFC 7009), in
    /// the background, best effort.
    fn revoke(&self, grant: Grant, owner_secret: Option<String>) {
        let Some(endpoint) = grant
            .metadata
            .additional_fields
            .get("revocation_endpoint")
            .and_then(serde_json::Value::as_str)
            .and_then(|e| reqwest::Url::parse(e).ok())
            .filter(|e| check_url(e).is_ok())
        else {
            return;
        };
        let (token, hint) = match (grant.refresh_token(), grant.access_token()) {
            (Some(refresh), _) => (refresh, "refresh_token"),
            (None, Some(access)) => (access, "access_token"),
            (None, None) => return,
        };
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("token", &token)
            .append_pair("token_type_hint", hint)
            .append_pair("client_id", &grant.client_id);
        if let Some(secret) = grant.client_secret(owner_secret.as_deref()) {
            form.append_pair("client_secret", &secret);
        }
        let body = form.finish();
        let client = self.revoker.clone();
        tokio::spawn(async move {
            let sent = client
                .post(endpoint)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(body)
                .send()
                .await;
            match sent {
                Ok(response) => {
                    info!(status = %response.status(), "an MCP server's OAuth token was revoked at its provider")
                }
                Err(e) => warn!(error = %e, "revoking an MCP server's OAuth token failed"),
            }
        });
    }

    /// Forget what was read of a deleted agent.
    pub async fn forget(&self, agent_id: AgentId) {
        self.slots.lock().await.remove(&agent_id);
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(agent, _), _| *agent != agent_id);
        self.refreshing
            .lock()
            .await
            .retain(|(agent, _), _| *agent != agent_id);
    }

    /// The access tokens of the agent's [`served`] servers, each for the URL
    /// it was issued for, when that is the server's URL.
    pub async fn tokens(
        &self,
        agent_id: AgentId,
        policy: &Policy,
        own: &[String],
    ) -> BTreeMap<String, McpToken> {
        let wanted = served(policy, own);
        if wanted.is_empty() {
            return BTreeMap::new();
        }
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let grants = self.loaded(agent_id, &mut held).await;
        wanted
            .into_iter()
            .filter_map(|(name, cfg)| {
                let grant = grants.get(name).filter(|g| grant_fits(g, cfg))?;
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

    /// The servers among `wanted` whose tokens are due for a refresh now,
    /// and not waiting after a failure.
    pub async fn due(
        &self,
        agent_id: AgentId,
        wanted: &[(&String, &McpServerConfig)],
    ) -> Vec<String> {
        let slot = self.slot(agent_id).await;
        let mut held = slot.lock().await;
        let grants = self.loaded(agent_id, &mut held).await;
        let now = Utc::now();
        let failures = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wanted
            .iter()
            .filter(|(name, cfg)| {
                grants.get(*name).is_some_and(|g| {
                    grant_fits(g, cfg)
                        && g.access_token().is_some()
                        && g.refresh_at().is_some_and(|at| at <= now)
                })
            })
            .filter(|(name, _)| {
                failures
                    .get(&(agent_id, (*name).clone()))
                    .is_none_or(|(_, next)| *next <= Instant::now())
            })
            .map(|(name, _)| (*name).clone())
            .collect()
    }

    /// Refresh the access token of `server`. `owner_secret` is the owner's
    /// client secret, resolved now. The agent's grants stay readable while
    /// the request runs; what it returns is kept unless the grant changed
    /// meanwhile (connected again, disconnected). A refusal ends the grant;
    /// other failures wait longer each time.
    pub async fn refresh(
        &self,
        agent_id: AgentId,
        server: &str,
        owner_secret: Option<String>,
    ) -> Result<(), OAuthError> {
        let gate = self.refresh_gate(agent_id, server).await;
        let _one = gate.lock().await;
        let Some(grant) = self.grant(agent_id, server).await else {
            return Ok(());
        };
        // A refresh that waited for another finds it done, or failed and
        // waiting.
        let waiting = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(agent_id, server.to_string()))
            .is_some_and(|(_, next)| *next > Instant::now());
        if waiting || grant.refresh_at().is_none_or(|at| at > Utc::now()) {
            return Ok(());
        }
        let secret = grant.client_secret(owner_secret.as_deref());
        let outcome = self.refreshed(&grant, secret).await;
        let key = (agent_id, server.to_string());
        match outcome {
            Ok(credentials) => {
                let next = Grant {
                    credentials,
                    ..grant.clone()
                };
                if self.replace(agent_id, server, &grant, next).await? {
                    self.failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&key);
                    info!(%agent_id, server, "MCP server's OAuth token refreshed");
                } else {
                    info!(%agent_id, server, "an MCP server's refreshed OAuth token is not kept: its grant changed meanwhile");
                }
                Ok(())
            }
            Err(e) => {
                let why = match &e {
                    OAuthError::Provider(_) => format!("refreshing the token failed: {e}"),
                    other => other.to_string(),
                };
                if matches!(e, OAuthError::Rejected(_)) {
                    // The grant ended: its token is dropped, and it is not
                    // tried again until the owner connects the server again.
                    let mut ended = grant.clone();
                    ended.credentials.token_response = None;
                    if !self
                        .replace(agent_id, server, &grant, ended.clone())
                        .await?
                        || !self.note_failure(agent_id, server, &ended, &why).await
                    {
                        return Ok(());
                    }
                } else {
                    // A grant connected again meanwhile is not the one that
                    // failed.
                    if !self.note_failure(agent_id, server, &grant, &why).await {
                        return Ok(());
                    }
                    let mut failures = self
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let count = failures.get(&key).map_or(1, |(n, _)| n + 1);
                    let wait = FIRST_BACKOFF
                        .saturating_mul(1 << (count - 1).min(10))
                        .min(MAX_BACKOFF);
                    // Without a refresh token there is nothing to try before
                    // the token expires.
                    let wait = match (&e, grant.expires_at()) {
                        (OAuthError::NoRefresh(_), Some(at)) => {
                            (at - Utc::now()).to_std().unwrap_or(wait).max(wait)
                        }
                        _ => wait,
                    };
                    failures.insert(key, (count, Instant::now() + wait));
                }
                Err(e)
            }
        }
    }

    async fn refreshed(
        &self,
        grant: &Grant,
        client_secret: Option<String>,
    ) -> Result<StoredCredentials, OAuthError> {
        if grant.refresh_token().is_none() {
            return Err(no_refresh());
        }
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(grant.url.as_str(), self.http.clone())
                .await?;
        // Discovery again, for the resource the server names itself as and
        // the authorization server it uses now, which must be the one that
        // issued the tokens. It has a deadline; the token request after it
        // has none but its request timeout, so what a provider returns, a
        // rotated refresh token with it, is never dropped half-way.
        let metadata =
            match tokio::time::timeout(DISCOVERY_TIMEOUT, manager.resolve_metadata()).await {
                Ok(Ok(resolution)) if resolution.metadata.issuer == grant.metadata.issuer => {
                    resolution.metadata
                }
                Ok(Ok(_)) => {
                    return Err(OAuthError::Rejected(
                        "the server's authorization server changed; connect it again".into(),
                    ));
                }
                Ok(Err(_)) | Err(_) => grant.metadata.clone(),
            };
        check_metadata(&metadata)?;
        manager.set_metadata(metadata);
        let mut client =
            OAuthClientConfig::new(grant.client_id.clone(), grant.redirect_uri.clone())
                .with_scopes(grant.scopes.clone());
        if let Some(secret) = client_secret {
            client = client.with_client_secret(secret);
        }
        manager.configure_client(client)?;
        let store = InMemoryCredentialStore::new();
        store.save(grant.credentials.clone()).await?;
        manager.set_credential_store(store.clone());
        manager.refresh_token().await.map_err(|e| match e {
            AuthError::TokenRefreshRejected(_) => OAuthError::Rejected(format!(
                "the authorization server refused to refresh the token ({e}); connect the \
                 server again"
            )),
            AuthError::AuthorizationRequired => no_refresh(),
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
        age_secs: u64,
    ) {
        let token = serde_json::from_value(serde_json::json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "refresh-0123",
        }))
        .expect("token");
        let received = u64::try_from(Utc::now().timestamp())
            .unwrap_or_default()
            .saturating_sub(age_secs);
        let grant = Grant {
            url: url.to_string(),
            client_id: "havn".into(),
            registered: true,
            registered_secret: None,
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

    /// The agents with tokens that expire within [`REFRESH_AHEAD`].
    pub async fn expiring_agents(&self) -> Vec<AgentId> {
        match agent_secrets::oauth_expiring(&self.db, Utc::now() + REFRESH_AHEAD).await {
            Ok(due) => {
                let mut agents: Vec<AgentId> = Vec::new();
                for (agent, _) in due {
                    if !agents.contains(&agent) {
                        agents.push(agent);
                    }
                }
                agents
            }
            Err(e) => {
                warn!(error = %e, "reading MCP OAuth tokens to refresh failed");
                Vec::new()
            }
        }
    }
}

fn no_refresh() -> OAuthError {
    OAuthError::NoRefresh(
        "the authorization server gave no refresh token; connect the server again once its \
         token expired"
            .into(),
    )
}

/// `value`, or the agent's secret it names.
pub fn resolve(value: &str, secrets: &BTreeMap<String, String>) -> Result<String, OAuthError> {
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

/// The owner's client secret of `cfg`, resolved from `secrets`; `None`
/// without one.
pub fn owner_secret(
    cfg: &McpServerConfig,
    secrets: &BTreeMap<String, String>,
) -> Result<Option<String>, OAuthError> {
    match &cfg.auth {
        McpAuth::Oauth(oauth) if !oauth.client_secret.is_empty() => {
            resolve(&oauth.client_secret, secrets).map(Some)
        }
        _ => Ok(None),
    }
}

/// Every endpoint the authorization server names, as https to public
/// addresses; its authorization endpoint without a `state` of its own.
fn check_metadata(metadata: &AuthorizationMetadata) -> Result<(), OAuthError> {
    let revocation = metadata
        .additional_fields
        .get("revocation_endpoint")
        .and_then(serde_json::Value::as_str);
    for (what, endpoint) in [
        (
            "authorization endpoint",
            Some(metadata.authorization_endpoint.as_str()),
        ),
        ("token endpoint", Some(metadata.token_endpoint.as_str())),
        (
            "registration endpoint",
            metadata.registration_endpoint.as_deref(),
        ),
        ("revocation endpoint", revocation),
    ] {
        let Some(endpoint) = endpoint else { continue };
        let url = reqwest::Url::parse(endpoint)
            .map_err(|e| OAuthError::Refused(format!("the authorization server's {what}: {e}")))?;
        check_url(&url).map_err(|why| {
            OAuthError::Refused(format!("the authorization server's {what}: {why}"))
        })?;
        if what == "authorization endpoint" && url.query_pairs().any(|(k, _)| k == "state") {
            return Err(OAuthError::Refused(
                "the authorization server's authorization endpoint carries a state of its own"
                    .into(),
            ));
        }
    }
    Ok(())
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

/// Every 15 seconds, refresh the due tokens of running agents, at most
/// [`PARALLEL_REFRESHES`] agents at once, and send each its new ones.
pub async fn refresh_loop(
    secrets: crate::secrets::SecretStore,
    registry: crate::agent_registry::AgentRegistry,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let agents = secrets.oauth().expiring_agents().await;
        futures_util::stream::iter(agents)
            .for_each_concurrent(PARALLEL_REFRESHES, |agent_id| {
                let secrets = secrets.clone();
                let registry = registry.clone();
                async move {
                    if !registry.is_connected(agent_id).await {
                        return;
                    }
                    secrets
                        .refresh_oauth(&registry, agent_id, REFRESH_WAIT)
                        .await;
                }
            })
            .await;
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
            "javascript:alert(document.cookie)",
            "data:text/html,<script>x</script>",
            "https://127.0.0.1/token",
            "https://10.0.0.5/token",
            "https://169.254.169.254/latest",
            "https://[::1]/token",
        ] {
            assert!(check_url(&url(bad)).is_err(), "{bad}");
        }
    }

    fn metadata(authorize: &str) -> AuthorizationMetadata {
        serde_json::from_value(serde_json::json!({
            "authorization_endpoint": authorize,
            "token_endpoint": "https://auth.example.com/token",
            "registration_endpoint": "https://auth.example.com/register",
        }))
        .unwrap()
    }

    #[test]
    fn an_authorization_server_names_https_endpoints_only() {
        assert!(check_metadata(&metadata("https://auth.example.com/authorize")).is_ok());
        for bad in [
            "javascript:alert(document.domain)//",
            "data:text/html,hi",
            "http://auth.example.com/authorize",
            "https://10.0.0.1/authorize",
            "https://auth.example.com/authorize?state=fixed",
        ] {
            assert!(check_metadata(&metadata(bad)).is_err(), "{bad}");
        }
        let mut token = metadata("https://auth.example.com/authorize");
        token.token_endpoint = "http://auth.example.com/token".into();
        assert!(check_metadata(&token).is_err());
        let mut revocation = metadata("https://auth.example.com/authorize");
        revocation.additional_fields.insert(
            "revocation_endpoint".into(),
            serde_json::json!("https://127.0.0.1/revoke"),
        );
        assert!(check_metadata(&revocation).is_err());
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
        let oauth = OAuthGrants::new(db, Some(KeyRing::from_passphrase("test-key")));
        assert!(matches!(
            oauth
                .finish(UserId::new(), "no-such-state", "code", None)
                .await,
            Err(OAuthError::UnknownState)
        ));
    }

    /// A provider that takes half a minute to answer anything.
    struct Slow;

    impl OAuthHttpClient for Slow {
        fn execute(&self, _request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err("too slow".into())
            })
        }
    }

    #[tokio::test]
    async fn a_slow_refresh_does_not_hold_the_agents_tokens() {
        use havn_db::repo::agents::{NewAgent, create as create_agent};
        use havn_db::repo::users::{NewUser, create as create_user};
        let db = havn_db::connect_in_memory().await.expect("db");
        let owner = create_user(&db, NewUser { display_name: "o" })
            .await
            .expect("user");
        let agent = create_agent(
            &db,
            NewAgent {
                owner_id: owner.id,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        let oauth = OAuthGrants::new(db, Some(KeyRing::from_passphrase("test-key")))
            .with_http(Arc::new(Slow));
        let url = "https://mcp.example.com/mcp";
        // Issued 3500 s ago with an hour's lifetime: due.
        oauth
            .keep_for_test(agent.id, "slow", url, "oauth-access-slow", 3500)
            .await;
        let mut policy = Policy::default();
        policy.permissions.can_access_network = true;
        policy.network_policy.egress_allowed = true;
        policy.mcp_servers = HashMap::from([(
            "slow".to_string(),
            McpServerConfig {
                url: url.into(),
                auth: McpAuth::Oauth(havn_core::McpOAuthConfig::default()),
                ..Default::default()
            },
        )]);
        let own = vec!["slow".to_string()];
        let served = served(&policy, &own);
        assert_eq!(oauth.due(agent.id, &served).await, vec!["slow".to_string()]);
        let refreshing = tokio::spawn({
            let oauth = oauth.clone();
            async move { oauth.refresh(agent.id, "slow", None).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // What a starting agent reads, while the provider has not answered.
        let tokens = tokio::time::timeout(
            Duration::from_secs(2),
            oauth.tokens(agent.id, &policy, &own),
        )
        .await
        .expect("tokens wait for the refresh");
        assert_eq!(tokens["slow"].access_token, "oauth-access-slow");
        let due = tokio::time::timeout(Duration::from_secs(2), oauth.due(agent.id, &served))
            .await
            .expect("due waits for the refresh");
        assert_eq!(due.len(), 1);
        assert!(!refreshing.is_finished());
        refreshing.abort();
    }

    #[tokio::test]
    async fn a_failure_of_a_grant_since_replaced_is_not_noted() {
        use havn_db::repo::agents::{NewAgent, create as create_agent};
        use havn_db::repo::users::{NewUser, create as create_user};
        let db = havn_db::connect_in_memory().await.expect("db");
        let owner = create_user(&db, NewUser { display_name: "o" })
            .await
            .expect("user");
        let agent = create_agent(
            &db,
            NewAgent {
                owner_id: owner.id,
                team_id: None,
                name: "a",
                config: serde_json::json!({}),
            },
        )
        .await
        .expect("agent");
        let oauth = OAuthGrants::new(db.clone(), Some(KeyRing::from_passphrase("test-key")));
        let url = "https://mcp.example.com/mcp";
        oauth
            .keep_for_test(agent.id, "s", url, "old-access", 0)
            .await;
        let old = oauth.grant(agent.id, "s").await.expect("grant");
        // Connected again while the old grant's refresh ran.
        oauth
            .keep_for_test(agent.id, "s", url, "new-access", 0)
            .await;
        assert!(!oauth.note_failure(agent.id, "s", &old, "refused").await);
        let rows = agent_secrets::oauth_list(&db, agent.id)
            .await
            .expect("rows");
        assert_eq!(rows[0].error, None);
        let now = oauth.grant(agent.id, "s").await.expect("grant");
        assert!(oauth.note_failure(agent.id, "s", &now, "refused").await);
        // Its refresh state goes with the grant.
        oauth
            .failures
            .lock()
            .unwrap()
            .insert((agent.id, "s".to_string()), (1, Instant::now()));
        let _gate = oauth.refresh_gate(agent.id, "s").await;
        assert!(
            oauth
                .disconnect(agent.id, "s", None)
                .await
                .expect("disconnect")
        );
        assert!(oauth.failures.lock().unwrap().is_empty());
        assert!(oauth.refreshing.lock().await.is_empty());
        // A grant that fits the server as it is now is not pruned.
        oauth.keep_for_test(agent.id, "s", url, "kept", 0).await;
        let servers = HashMap::from([(
            "s".to_string(),
            McpServerConfig {
                url: url.into(),
                auth: McpAuth::Oauth(havn_core::McpOAuthConfig::default()),
                ..Default::default()
            },
        )]);
        oauth
            .prune(agent.id, &servers, &HashMap::new(), &BTreeMap::new())
            .await;
        assert!(oauth.grant(agent.id, "s").await.is_some());
        oauth.forget(agent.id).await;
        assert!(oauth.refreshing.lock().await.is_empty());
    }

    #[test]
    fn a_short_lived_token_is_refreshed_at_half_its_lifetime() {
        let grant = |lifetime: u64| Grant {
            url: "https://mcp.example.com/mcp".into(),
            client_id: "c".into(),
            registered: true,
            registered_secret: None,
            redirect_uri: "https://havn.example.com/api/mcp/oauth/callback".into(),
            scopes: Vec::new(),
            metadata: AuthorizationMetadata::default(),
            credentials: StoredCredentials::new(
                "c".into(),
                Some(
                    serde_json::from_value(serde_json::json!({
                        "access_token": "a", "token_type": "Bearer", "expires_in": lifetime
                    }))
                    .unwrap(),
                ),
                Vec::new(),
                Some(1_000_000),
            ),
        };
        let at = |secs: i64| DateTime::from_timestamp(1_000_000 + secs, 0).unwrap();
        assert_eq!(grant(120).refresh_at(), Some(at(60)));
        assert_eq!(grant(3600).refresh_at(), Some(at(3300)));
        let fits = McpServerConfig {
            url: "https://mcp.example.com/mcp".into(),
            auth: McpAuth::Oauth(havn_core::McpOAuthConfig::default()),
            ..Default::default()
        };
        assert!(grant_fits(&grant(60), &fits));
        let moved = McpServerConfig {
            url: "https://mcp.example.com/v2".into(),
            ..fits.clone()
        };
        assert!(!grant_fits(&grant(60), &moved));
        let owners = McpServerConfig {
            auth: McpAuth::Oauth(havn_core::McpOAuthConfig {
                client_id: "mine".into(),
                ..Default::default()
            }),
            ..fits
        };
        assert!(
            !grant_fits(&grant(60), &owners),
            "a registered client is not the owner's"
        );
    }
}
