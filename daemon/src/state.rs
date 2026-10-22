//! State shared by the HTTP handlers and the sampling task.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use diesel::sqlite::SqliteConnection;
use rh_core::{
    classifier::Classifier,
    ledger::{Ledger, LedgerSettings, ProcessLiveness, Transition},
    sampler::RawProcess,
};
use rh_shared::{
    config::Config,
    types::{Holder, SessionKey, Snapshot},
};
use tokio::sync::{Mutex as AsyncMutex, watch};

use crate::db::{DbPool, repo};

/// Handle the handlers receive through Axum's `State`.
pub type AppState = Arc<Shared>;

/// PIDs and start times from one sample. A holder is alive when its PID is running with the
/// recorded start time; Windows reuses PIDs, so the PID alone proves nothing.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable(HashMap<u32, u64>);

impl ProcessTable {
    /// The table of every process in a sample.
    pub fn new(processes: &[RawProcess]) -> Self {
        Self(
            processes
                .iter()
                .map(|process| (process.pid, process.start_time))
                .collect(),
        )
    }
}

impl ProcessLiveness for ProcessTable {
    fn is_alive(&self, holder: &Holder) -> bool {
        self.0.get(&holder.pid) == Some(&holder.pid_start_time)
    }
}

/// The most recent sample.
#[derive(Debug)]
pub struct Latest {
    /// What clients see.
    pub snapshot: Snapshot,
    /// What liveness checks use.
    pub processes: ProcessTable,
}

/// The configuration in effect and what is built from it; replaced as a whole on reload.
#[derive(Debug)]
pub struct Settings {
    /// The loaded configuration.
    pub config: Config,
    /// The config's heavy-command rules, which fingerprint jobs for history.
    pub classifier: Classifier,
}

impl Settings {
    fn new(config: Config) -> Self {
        let classifier = Classifier::new(&config.classifier);
        Self { config, classifier }
    }
}

/// Everything the daemon shares between tasks.
#[derive(Debug)]
pub struct Shared {
    /// The bearer token every authenticated request must carry.
    pub token: String,
    /// Database connections.
    pub pool: DbPool,
    /// The ledger. It stays locked while its transitions are written, so the database sees
    /// them in the order the ledger made them.
    pub ledger: AsyncMutex<Ledger>,
    /// Bumped after every batch of lease transitions, to wake waiting acquire requests.
    pub changes: watch::Sender<u64>,
    /// Set by `POST /shutdown`; the server stops gracefully when it turns true.
    pub shutdown: watch::Sender<bool>,
    /// When the daemon started.
    pub started: Instant,
    settings: RwLock<Arc<Settings>>,
    config_error: Mutex<Option<String>>,
    latest: RwLock<Option<Arc<Latest>>>,
    sessions: Mutex<HashMap<SessionKey, Holder>>,
}

impl Shared {
    /// Fresh state with an empty ledger and no sample yet.
    pub fn new(config: Config, token: String, pool: DbPool) -> Self {
        let ledger = Ledger::new(LedgerSettings::from_config(&config));
        Self {
            token,
            pool,
            ledger: AsyncMutex::new(ledger),
            changes: watch::Sender::new(0),
            shutdown: watch::Sender::new(false),
            started: Instant::now(),
            settings: RwLock::new(Arc::new(Settings::new(config))),
            config_error: Mutex::new(None),
            latest: RwLock::new(None),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// The configuration in effect. Hold the returned value for one request, so it sees one
    /// consistent config even when a reload lands meanwhile.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Puts `config` into effect for later requests and the ledger, and clears a reported
    /// config error. Leases keep the profiles they reserved.
    pub async fn apply_config(&self, config: Config) {
        let mut ledger = self.ledger.lock().await;
        ledger.set_settings(LedgerSettings::from_config(&config));
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(Settings::new(config));
        drop(ledger);
        self.set_config_error(None);
    }

    /// Why the last config change was rejected, if it was.
    pub fn config_error(&self) -> Option<String> {
        self.config_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records or clears the reason the config file was rejected.
    pub fn set_config_error(&self, error: Option<String>) {
        *self
            .config_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = error;
    }

    /// The most recent sample, if one has arrived.
    pub fn latest(&self) -> Option<Arc<Latest>> {
        self.latest
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the most recent sample.
    pub fn set_latest(&self, latest: Latest) {
        *self.latest.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(latest));
    }

    /// The heartbeat TTL in effect.
    pub fn heartbeat_ttl(&self) -> Duration {
        Duration::from_secs(self.settings().config.leases.heartbeat_ttl_secs)
    }

    /// Adds or refreshes a session.
    pub fn register(&self, holder: Holder) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(holder.session.clone(), holder);
    }

    /// Forgets sessions whose holder is gone and returns the rest.
    pub fn live_sessions(&self, processes: &ProcessTable) -> Vec<Holder> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|_, holder| processes.is_alive(holder));
        sessions.values().cloned().collect()
    }

    /// Wakes every request waiting for a lease transition.
    pub fn notify_changes(&self) {
        self.changes.send_modify(|generation| *generation += 1);
    }

    /// Runs blocking database work off the async runtime.
    ///
    /// # Errors
    ///
    /// Fails when no connection is available, `work` fails, or the blocking task panics.
    pub async fn with_db<T, F>(&self, work: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T> + Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get()?;
            work(&mut conn)
        })
        .await?
    }

    /// Writes `transitions` to the database, all stamped `at_ms`.
    ///
    /// # Errors
    ///
    /// Fails when a write fails; earlier transitions in the batch stay written.
    pub async fn persist(&self, transitions: &[Transition], at_ms: i64) -> anyhow::Result<()> {
        if transitions.is_empty() {
            return Ok(());
        }
        for transition in transitions {
            tracing::info!(
                lease = transition.id.0,
                to = transition.to.as_str(),
                reason = %transition.reason,
                "lease transition"
            );
        }
        let transitions = transitions.to_vec();
        self.with_db(move |conn| {
            for transition in &transitions {
                repo::transition_lease(
                    conn,
                    transition.id,
                    transition.to,
                    at_ms,
                    &transition.reason,
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Runs one ledger tick against the latest sample and persists what changed. Does
    /// nothing before the first sample.
    ///
    /// # Errors
    ///
    /// Fails when the transitions cannot be written.
    pub async fn tick(&self, ledger: &mut Ledger) -> anyhow::Result<()> {
        let Some(latest) = self.latest() else {
            return Ok(());
        };
        let now = now_ms();
        let transitions = ledger.tick(now, &latest.snapshot, &latest.processes);
        self.persist(&transitions, now).await?;
        if !transitions.is_empty() {
            self.notify_changes();
        }
        Ok(())
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
