//! The resources-helper daemon: system sampling, the lease ledger, their persistence, and the
//! HTTP API that serves them.

pub mod db;
mod error;
pub mod logfile;
mod maintenance;
mod routes;
mod sampling;
pub mod startup;
mod state;
mod ui;

use std::{future::Future, sync::Arc, time::Duration};

use rh_shared::{api::LeaseQuery, config::Config, types::LeaseState};
use tokio::net::TcpListener;

use crate::{
    db::{DbPool, repo},
    state::{Shared, now_ms},
};
pub use crate::{maintenance::Maintenance, sampling::SamplerFactory};

/// What the daemon needs to serve, prepared by `main` or by a test.
pub struct DaemonOptions {
    /// The loaded configuration.
    pub config: Config,
    /// The bearer token clients must send.
    pub token: String,
    /// The opened, migrated database.
    pub pool: DbPool,
    /// Builds the sampler on the sampling thread.
    pub sampler: SamplerFactory,
    /// Time between samples; one second in production, since the rolling windows count
    /// samples.
    pub sample_interval: Duration,
    /// Config reloading and pruning.
    pub maintenance: Maintenance,
}

/// Serves the API on `listener` until `shutdown` resolves or a client calls
/// `POST /shutdown`.
///
/// Leases left queued or active by a previous run are tracked again first, in their order;
/// the first sample then expires those whose holder is gone.
///
/// # Errors
///
/// Fails when the database cannot be read, the sampling thread cannot start, or the server
/// stops with an I/O error.
pub async fn serve(
    listener: TcpListener,
    options: DaemonOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let state = Arc::new(Shared::new(options.config, options.token, options.pool));
    restore_leftovers(&state).await?;
    let samples = sampling::spawn_sampler(options.sampler, options.sample_interval)?;
    let sampling = tokio::spawn(sampling::run(state.clone(), samples));
    let upkeep = tokio::spawn(maintenance::run(state.clone(), options.maintenance));

    let mut requested = state.shutdown.subscribe();
    let stopping = state.clone();
    let stop = async move {
        tokio::select! {
            () = shutdown => {}
            _ = requested.wait_for(|stop| *stop) => {}
        }
        // Open acquire waits answer once this is set; graceful shutdown would otherwise wait
        // them out, for up to their full timeout.
        stopping.shutdown.send_replace(true);
    };

    tracing::info!("listening on {}", listener.local_addr()?);
    let served = axum::serve(listener, routes::router(state))
        .with_graceful_shutdown(stop)
        .await;
    sampling.abort();
    upkeep.abort();
    tracing::info!("stopped");
    Ok(served?)
}

/// Puts the leases a previous run left queued or active back into the ledger, oldest first,
/// with their holders registered so their usage is attributed again.
async fn restore_leftovers(state: &Shared) -> anyhow::Result<()> {
    let leftovers = state
        .with_db(|conn| {
            let mut leftovers = Vec::new();
            for leftover_state in [LeaseState::Queued, LeaseState::Active] {
                let query = LeaseQuery {
                    session: None,
                    state: Some(leftover_state),
                };
                leftovers.extend(repo::list_leases(conn, &query)?);
            }
            leftovers.sort_by_key(|lease| lease.id);
            Ok(leftovers)
        })
        .await?;
    if leftovers.is_empty() {
        return Ok(());
    }

    let now = now_ms();
    let mut ledger = state.ledger.lock().await;
    let mut deadlines = Vec::with_capacity(leftovers.len());
    for lease in leftovers {
        state.register(lease.holder.clone());
        let restored = ledger.restore(lease, now)?;
        deadlines.push((restored.id, restored.expires_at_ms));
    }
    let count = deadlines.len();
    state
        .with_db(move |conn| {
            for (id, expires_at_ms) in deadlines {
                repo::restore_lease(conn, id, expires_at_ms, now)?;
            }
            Ok(())
        })
        .await?;
    tracing::info!("restored {count} lease(s) left over from the previous run");
    Ok(())
}
