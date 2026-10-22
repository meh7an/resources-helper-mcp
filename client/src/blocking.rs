//! A blocking front end for callers without an async runtime: the CLI and the hook.

use rh_shared::{
    api::{
        AcquireRequest, AcquireResponse, CanFitRequest, HealthResponse, HeartbeatResponse,
        JobSummary, LeaseQuery, ReleaseResponse, Verdict,
    },
    types::{Holder, JobRun, Lease, LeaseId, SessionKey, SessionUsage, Snapshot},
};
use tokio::runtime::{Builder, Runtime};

use crate::{Client, ClientError, ClientOptions, DaemonStart, Result};

/// [`Client`] driven by its own single-threaded runtime. Do not call it from inside an
/// async runtime; use [`Client`] there.
#[derive(Debug)]
pub struct BlockingClient {
    runtime: Runtime,
    client: Client,
}

impl BlockingClient {
    /// A blocking client with `options`.
    ///
    /// # Errors
    ///
    /// Fails when the runtime or the HTTP client cannot be built.
    pub fn new(options: ClientOptions) -> Result<Self> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ClientError::Setup(error.to_string()))?;
        Ok(Self {
            runtime,
            client: Client::new(options)?,
        })
    }

    /// A blocking client for the local install, see [`ClientOptions::discover`].
    ///
    /// # Errors
    ///
    /// Fails when the local configuration cannot be read.
    pub fn discover(auto_spawn: bool) -> Result<Self> {
        Self::new(ClientOptions::discover(auto_spawn)?)
    }

    /// See [`Client::base_url`].
    pub fn base_url(&self) -> &str {
        self.client.base_url()
    }

    /// See [`Client::health`].
    ///
    /// # Errors
    ///
    /// As [`Client::health`].
    pub fn health(&self) -> Result<HealthResponse> {
        self.runtime.block_on(self.client.health())
    }

    /// See [`Client::ensure_running`].
    ///
    /// # Errors
    ///
    /// As [`Client::ensure_running`].
    pub fn ensure_running(&self) -> Result<DaemonStart> {
        self.runtime.block_on(self.client.ensure_running())
    }

    /// See [`Client::snapshot`].
    ///
    /// # Errors
    ///
    /// As [`Client::snapshot`].
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.runtime.block_on(self.client.snapshot())
    }

    /// See [`Client::can_fit`].
    ///
    /// # Errors
    ///
    /// As [`Client::can_fit`].
    pub fn can_fit(&self, request: &CanFitRequest) -> Result<Verdict> {
        self.runtime.block_on(self.client.can_fit(request))
    }

    /// See [`Client::acquire`].
    ///
    /// # Errors
    ///
    /// As [`Client::acquire`].
    pub fn acquire(&self, request: &AcquireRequest) -> Result<AcquireResponse> {
        self.runtime.block_on(self.client.acquire(request))
    }

    /// See [`Client::heartbeat`].
    ///
    /// # Errors
    ///
    /// As [`Client::heartbeat`].
    pub fn heartbeat(&self, id: LeaseId, session: &SessionKey) -> Result<HeartbeatResponse> {
        self.runtime.block_on(self.client.heartbeat(id, session))
    }

    /// See [`Client::release`].
    ///
    /// # Errors
    ///
    /// As [`Client::release`].
    pub fn release(&self, id: LeaseId, session: &SessionKey) -> Result<ReleaseResponse> {
        self.runtime.block_on(self.client.release(id, session))
    }

    /// See [`Client::leases`].
    ///
    /// # Errors
    ///
    /// As [`Client::leases`].
    pub fn leases(&self, query: &LeaseQuery) -> Result<Vec<Lease>> {
        self.runtime.block_on(self.client.leases(query))
    }

    /// See [`Client::history`].
    ///
    /// # Errors
    ///
    /// As [`Client::history`].
    pub fn history(&self) -> Result<Vec<JobSummary>> {
        self.runtime.block_on(self.client.history())
    }

    /// See [`Client::history_runs`].
    ///
    /// # Errors
    ///
    /// As [`Client::history_runs`].
    pub fn history_runs(&self, fingerprint: &str, limit: Option<u32>) -> Result<Vec<JobRun>> {
        self.runtime
            .block_on(self.client.history_runs(fingerprint, limit))
    }

    /// See [`Client::sessions`].
    ///
    /// # Errors
    ///
    /// As [`Client::sessions`].
    pub fn sessions(&self) -> Result<Vec<SessionUsage>> {
        self.runtime.block_on(self.client.sessions())
    }

    /// See [`Client::register_session`].
    ///
    /// # Errors
    ///
    /// As [`Client::register_session`].
    pub fn register_session(&self, holder: &Holder) -> Result<()> {
        self.runtime.block_on(self.client.register_session(holder))
    }

    /// See [`Client::shutdown`].
    ///
    /// # Errors
    ///
    /// As [`Client::shutdown`].
    pub fn shutdown(&self) -> Result<()> {
        self.runtime.block_on(self.client.shutdown())
    }
}
