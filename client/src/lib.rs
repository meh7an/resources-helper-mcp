//! Typed client for the `rhd` daemon API, shared by the MCP server, hook, and CLI.
//!
//! [`Client`] is async; [`BlockingClient`] wraps it for callers without a runtime. Both can
//! start the daemon in the background when it is not running and retry once.

mod blocking;
mod error;
mod spawn;

use std::{
    path::PathBuf,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use reqwest::{RequestBuilder, Response};
use rh_shared::{
    api::{
        AcquireRequest, AcquireResponse, CanFitRequest, HealthResponse, HeartbeatResponse,
        HistoryResponse, HistoryRunsResponse, JobSummary, LeaseQuery, LeasesResponse,
        RegisterSessionRequest, ReleaseResponse, SessionsResponse, Verdict, WaitRequest,
    },
    config::Config,
    paths::Paths,
    types::{Holder, JobRun, Lease, LeaseId, SessionKey, SessionUsage, Snapshot},
};
use serde::de::DeserializeOwned;

pub use crate::{blocking::BlockingClient, error::ClientError, spawn::SpawnOptions};

/// Result of a client call.
pub type Result<T> = std::result::Result<T, ClientError>;

/// The connect timeout [`ClientOptions::discover`] uses.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// The request timeout [`ClientOptions::discover`] uses.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(100);

/// Where the bearer token comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// A known token, for tests.
    Fixed(String),
    /// The daemon's token file, read when first needed and again after starting rhd.
    File(PathBuf),
}

/// How a [`Client`] reaches the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOptions {
    /// For example `http://127.0.0.1:47823`.
    pub base_url: String,
    /// The bearer token.
    pub token: TokenSource,
    /// Timeout for opening the connection.
    pub connect_timeout: Duration,
    /// Timeout for every request; acquire adds its own wait on top.
    pub request_timeout: Duration,
    /// Start rhd and retry once when it is unreachable; `None` never starts it.
    pub spawn: Option<SpawnOptions>,
}

impl ClientOptions {
    /// Options for the local install: the port from `config.toml` and the token file under
    /// `%USERPROFILE%\.resources-helper\`.
    ///
    /// # Errors
    ///
    /// Fails when `USERPROFILE` is unset or the config file is invalid.
    pub fn discover(auto_spawn: bool) -> Result<Self> {
        let paths = Paths::from_env()?;
        let config = Config::load(&paths.config_file())?;
        Ok(Self::local(&paths, &config, auto_spawn))
    }

    /// Options for an install at `paths` with an already loaded `config`, with the default
    /// timeouts.
    pub fn local(paths: &Paths, config: &Config, auto_spawn: bool) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{}", config.daemon.port),
            token: TokenSource::File(paths.token_file()),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            spawn: auto_spawn.then(SpawnOptions::beside_current_exe),
        }
    }
}

/// Whether [`Client::ensure_running`] had to start the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStart {
    /// It was already answering.
    AlreadyRunning,
    /// It was started and is now answering.
    Started,
}

/// Async client for every daemon endpoint.
#[derive(Debug)]
pub struct Client {
    http: reqwest::Client,
    options: ClientOptions,
    token: Mutex<Option<String>>,
}

impl Client {
    /// A client with `options`.
    ///
    /// # Errors
    ///
    /// Fails when the HTTP client cannot be built.
    pub fn new(options: ClientOptions) -> Result<Self> {
        // Per-request timeouts (acquire's wait) override this client-wide default.
        let http = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout)
            .build()
            .map_err(|error| ClientError::Setup(error.to_string()))?;
        Ok(Self {
            http,
            options,
            token: Mutex::new(None),
        })
    }

    /// A client for the local install, see [`ClientOptions::discover`].
    ///
    /// # Errors
    ///
    /// Fails when the local configuration cannot be read.
    pub fn discover(auto_spawn: bool) -> Result<Self> {
        Self::new(ClientOptions::discover(auto_spawn)?)
    }

    /// The daemon's base URL.
    pub fn base_url(&self) -> &str {
        &self.options.base_url
    }

    /// `GET /health`, which needs no token. Never starts the daemon.
    ///
    /// # Errors
    ///
    /// [`ClientError::Unreachable`] when the daemon is not running.
    pub async fn health(&self) -> Result<HealthResponse> {
        let url = self.url("/health");
        let response = self.http.get(&url).send().await;
        let response = response.map_err(|error| transport_error(&url, &error))?;
        decode(checked(response).await?).await
    }

    /// Starts the daemon unless it already answers, and waits until it does.
    ///
    /// # Errors
    ///
    /// [`ClientError::Spawn`] when it cannot be started or does not become ready in time.
    pub async fn ensure_running(&self) -> Result<DaemonStart> {
        if self.health().await.is_ok() {
            return Ok(DaemonStart::AlreadyRunning);
        }
        let spawn = self
            .options
            .spawn
            .clone()
            .unwrap_or_else(SpawnOptions::beside_current_exe);
        self.start_daemon(&spawn).await?;
        Ok(DaemonStart::Started)
    }

    /// `GET /snapshot`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Http` with status 503 before the first sample.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.get("/snapshot").await
    }

    /// `POST /can-fit`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Http` with status 422 for an invalid profile.
    pub async fn can_fit(&self, request: &CanFitRequest) -> Result<Verdict> {
        let url = self.url("/can-fit");
        let response = self
            .execute(|http, token| http.post(&url).bearer_auth(token).json(request))
            .await?;
        decode(response).await
    }

    /// `POST /leases`. The request may stay open for `wait_timeout_secs`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Http` with status 422 for an invalid profile.
    pub async fn acquire(&self, request: &AcquireRequest) -> Result<AcquireResponse> {
        let url = self.url("/leases");
        let timeout = self.options.request_timeout + Duration::from_secs(request.wait_timeout_secs);
        let response = self
            .execute(|http, token| {
                http.post(&url)
                    .bearer_auth(token)
                    .json(request)
                    .timeout(timeout)
            })
            .await?;
        decode(response).await
    }

    /// `POST /leases/{id}/heartbeat`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Http` with 403 for another session's lease, 404 for an unknown
    /// one.
    pub async fn heartbeat(&self, id: LeaseId, session: &SessionKey) -> Result<HeartbeatResponse> {
        let path = format!("/leases/{}/heartbeat?session={}", id.0, encode(&session.0));
        let url = self.url(&path);
        let response = self
            .execute(|http, token| http.post(&url).bearer_auth(token))
            .await?;
        decode(response).await
    }

    /// `POST /leases/{id}/wait`: keeps waiting for a queued lease, which keeps its place. The
    /// request may stay open for `wait_timeout_secs`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Http` with 403 for another session's lease, 404 for an unknown
    /// one.
    pub async fn wait(
        &self,
        id: LeaseId,
        session: &SessionKey,
        wait_timeout_secs: u64,
    ) -> Result<AcquireResponse> {
        let path = format!("/leases/{}/wait?session={}", id.0, encode(&session.0));
        let url = self.url(&path);
        let request = WaitRequest { wait_timeout_secs };
        let timeout = self.options.request_timeout + Duration::from_secs(wait_timeout_secs);
        let response = self
            .execute(|http, token| {
                http.post(&url)
                    .bearer_auth(token)
                    .json(&request)
                    .timeout(timeout)
            })
            .await?;
        decode(response).await
    }

    /// `DELETE /leases/{id}`. Releasing an ended lease returns it unchanged. The release that
    /// ends a benchmark lease also carries its noise report.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Http` with 403 for another session's lease, 404 for an unknown
    /// one.
    pub async fn release(&self, id: LeaseId, session: &SessionKey) -> Result<ReleaseResponse> {
        let path = format!("/leases/{}?session={}", id.0, encode(&session.0));
        let url = self.url(&path);
        let response = self
            .execute(|http, token| http.delete(&url).bearer_auth(token))
            .await?;
        decode(response).await
    }

    /// `GET /leases`, oldest first.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn leases(&self, query: &LeaseQuery) -> Result<Vec<Lease>> {
        let mut params = Vec::new();
        if let Some(session) = &query.session {
            params.push(format!("session={}", encode(&session.0)));
        }
        if let Some(state) = query.state {
            params.push(format!("state={}", state.as_str()));
        }
        let path = if params.is_empty() {
            "/leases".to_owned()
        } else {
            format!("/leases?{}", params.join("&"))
        };
        Ok(self.get::<LeasesResponse>(&path).await?.leases)
    }

    /// `GET /history`: one summary per job fingerprint, most recently run first.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn history(&self) -> Result<Vec<JobSummary>> {
        Ok(self.get::<HistoryResponse>("/history").await?.jobs)
    }

    /// `GET /history/runs`: runs with `fingerprint`, newest first; the daemon's default count
    /// when `limit` is `None`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn history_runs(&self, fingerprint: &str, limit: Option<u32>) -> Result<Vec<JobRun>> {
        let mut path = format!("/history/runs?fingerprint={}", encode(fingerprint));
        if let Some(limit) = limit {
            path.push_str(&format!("&limit={limit}"));
        }
        Ok(self.get::<HistoryRunsResponse>(&path).await?.runs)
    }

    /// `GET /sessions`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Http` with status 503 before the first sample.
    pub async fn sessions(&self) -> Result<Vec<SessionUsage>> {
        Ok(self.get::<SessionsResponse>("/sessions").await?.sessions)
    }

    /// `POST /sessions`.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn register_session(&self, holder: &Holder) -> Result<()> {
        let url = self.url("/sessions");
        let request = RegisterSessionRequest {
            holder: holder.clone(),
        };
        self.execute(|http, token| http.post(&url).bearer_auth(token).json(&request))
            .await?;
        Ok(())
    }

    /// `POST /shutdown`. Never starts the daemon.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; `Unreachable` when it is not running.
    pub async fn shutdown(&self) -> Result<()> {
        let url = self.url("/shutdown");
        let token = self.token()?;
        let response = self.http.post(&url).bearer_auth(token).send().await;
        checked(response.map_err(|error| transport_error(&url, &error))?).await?;
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.options.base_url)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        let response = self
            .execute(|http, token| http.get(&url).bearer_auth(token))
            .await?;
        decode(response).await
    }

    /// Sends the request `build` makes. When no daemon received it and spawning is allowed,
    /// starts one and sends once more; when the token was refused, reads the token file again
    /// and sends once more.
    async fn execute<F>(&self, build: F) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        match self.attempt(&build).await {
            Err(error) if error.is_unsent() => match &self.options.spawn {
                Some(spawn) => {
                    self.start_daemon(spawn).await?;
                    self.attempt(&build).await
                }
                None => Err(error),
            },
            // The daemon writes a new token when its token file was deleted, and a long-lived
            // client still holds the old one.
            Err(error)
                if error.status() == Some(401)
                    && matches!(self.options.token, TokenSource::File(_)) =>
            {
                self.forget_token();
                self.attempt(&build).await
            }
            other => other,
        }
    }

    async fn attempt<F>(&self, build: &F) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        let token = self.token()?;
        let response = build(&self.http, &token)
            .send()
            .await
            .map_err(|error| transport_error(&self.options.base_url, &error))?;
        checked(response).await
    }

    fn token(&self) -> Result<String> {
        let path = match &self.options.token {
            TokenSource::Fixed(token) => return Ok(token.clone()),
            TokenSource::File(path) => path,
        };
        let mut cached = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(token) = cached.as_ref() {
            return Ok(token.clone());
        }
        let token = std::fs::read_to_string(path)
            .map(|token| token.trim().to_owned())
            .ok()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ClientError::NoToken(path.clone()))?;
        *cached = Some(token.clone());
        Ok(token)
    }

    fn forget_token(&self) {
        if matches!(self.options.token, TokenSource::File(_)) {
            *self.token.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    async fn start_daemon(&self, spawn: &SpawnOptions) -> Result<()> {
        spawn.spawn_detached().map_err(|error| {
            ClientError::Spawn(format!("{}: {error}", spawn.daemon_exe.display()))
        })?;
        let deadline = Instant::now() + spawn.ready_timeout;
        while Instant::now() < deadline {
            if self.health().await.is_ok() {
                // A first start writes a new token file.
                self.forget_token();
                return Ok(());
            }
            tokio::time::sleep(READY_POLL).await;
        }
        Err(ClientError::Spawn(format!(
            "{} started but did not answer within {} s; see %USERPROFILE%\\.resources-helper\\logs\\rhd.log",
            spawn.daemon_exe.display(),
            spawn.ready_timeout.as_secs()
        )))
    }
}

/// [`ClientError::Unreachable`] when no connection was made, else
/// [`ClientError::Interrupted`], since the daemon may have received the request.
fn transport_error(url: &str, error: &reqwest::Error) -> ClientError {
    let url = url.to_owned();
    let reason = error.to_string();
    if error.is_connect() {
        ClientError::Unreachable { url, reason }
    } else {
        ClientError::Interrupted { url, reason }
    }
}

/// Turns an error status into [`ClientError::Http`] with the body as message.
async fn checked(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let message = response.text().await.unwrap_or_default();
    Err(ClientError::Http {
        status: status.as_u16(),
        message,
    })
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    response
        .json()
        .await
        .map_err(|error| ClientError::Decode(error.to_string()))
}

/// Percent-encodes a query value; letters, digits, and `-._~` pass unchanged.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(encode("3f2a-9c1e_x.y~z"), "3f2a-9c1e_x.y~z");
        assert_eq!(encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }
}
