//! Background upkeep: putting changes to the config file into effect, and pruning old rows.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use rh_shared::config::Config;
use tokio::time::{self, MissedTickBehavior};

use crate::{
    db::repo,
    state::{AppState, now_ms},
};

const CONFIG_POLL: Duration = Duration::from_secs(5);
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// What the upkeep task watches, and how often.
#[derive(Debug, Clone)]
pub struct Maintenance {
    /// The config file to reload on change, or `None` to keep the config the daemon started
    /// with.
    pub config_path: Option<PathBuf>,
    /// Time between checks of the config file.
    pub config_poll: Duration,
    /// Time between prunes; the first one runs at startup.
    pub prune_every: Duration,
}

impl Maintenance {
    /// Checks `config_path` every 5 s and prunes every hour.
    pub fn standard(config_path: PathBuf) -> Self {
        Self {
            config_path: Some(config_path),
            ..Self::default()
        }
    }
}

impl Default for Maintenance {
    /// Keeps the startup config and prunes every hour.
    fn default() -> Self {
        Self {
            config_path: None,
            config_poll: CONFIG_POLL,
            prune_every: PRUNE_EVERY,
        }
    }
}

/// Modification time and size of a file, or `None` when it cannot be read. A change in
/// either means the file was saved again.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Runs the upkeep until the daemon stops.
pub async fn run(state: AppState, maintenance: Maintenance) {
    let mut config_ticks = time::interval(maintenance.config_poll);
    config_ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut prune_ticks = time::interval(maintenance.prune_every);
    prune_ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let config_path = maintenance.config_path;
    let mut seen = config_path.as_deref().and_then(stamp);
    loop {
        tokio::select! {
            _ = config_ticks.tick(), if config_path.is_some() => {
                if let Some(path) = &config_path {
                    let current = stamp(path);
                    if current != seen {
                        seen = current;
                        reload(&state, path).await;
                    }
                }
            }
            _ = prune_ticks.tick() => prune(&state).await,
        }
    }
}

/// Loads the config file again. A valid file goes into effect at once; an invalid one is
/// reported and the config in effect stays.
async fn reload(state: &AppState, path: &Path) {
    let previous = state.settings();
    match Config::load(path) {
        Ok(config) if config == previous.config => state.set_config_error(None),
        Ok(config) => {
            let (old_port, new_port) = (previous.config.daemon.port, config.daemon.port);
            if new_port != old_port {
                tracing::warn!(
                    "daemon.port changed from {old_port} to {new_port}; rhd keeps listening \
                     on {old_port} until it restarts, while clients already use {new_port}"
                );
            }
            state.apply_config(config).await;
            tracing::info!("config reloaded from {}", path.display());
        }
        Err(error) => {
            let message = error.to_string();
            if state.config_error().as_deref() != Some(message.as_str()) {
                tracing::error!("config rejected, keeping the previous one: {message}");
            }
            state.set_config_error(Some(message));
        }
    }
}

async fn prune(state: &AppState) {
    let retention = state.settings().config.retention.clone();
    let now = now_ms();
    match state
        .with_db(move |conn| repo::prune(conn, &retention, now))
        .await
    {
        Ok(pruned) if pruned == repo::Pruned::default() => {}
        Ok(pruned) => tracing::info!(
            "pruned {} ended lease(s) and {} job run(s)",
            pruned.leases,
            pruned.job_runs
        ),
        Err(error) => tracing::error!("pruning failed: {error:#}"),
    }
}
