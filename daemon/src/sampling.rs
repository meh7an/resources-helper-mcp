//! The sampling thread and the task that turns its samples into snapshots and ledger ticks.

use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use rh_core::{
    process::ProcessTable,
    sampler::{RawSample, Sampler, SnapshotBuilder},
};
use tokio::sync::mpsc;

use crate::state::{AppState, Latest};

/// Creates the sampler on the sampling thread. The Windows sampler holds PDH handles that
/// cannot move between threads, so it must be built where it is used.
pub type SamplerFactory = Box<dyn FnOnce() -> rh_shared::Result<Box<dyn Sampler>> + Send>;

type SampleResult = rh_shared::Result<RawSample>;

/// Starts the sampling thread. It samples every `interval` until the receiver is dropped.
///
/// # Errors
///
/// Fails when the OS refuses to start the thread.
pub fn spawn_sampler(
    factory: SamplerFactory,
    interval: Duration,
) -> anyhow::Result<mpsc::Receiver<SampleResult>> {
    let (sender, receiver) = mpsc::channel(4);
    thread::Builder::new()
        .name("rh-sampler".into())
        .spawn(move || {
            let mut sampler = match factory() {
                Ok(sampler) => sampler,
                Err(error) => {
                    // If the receiver is gone the daemon is shutting down anyway.
                    let _ = sender.blocking_send(Err(error));
                    return;
                }
            };
            let mut next = Instant::now();
            loop {
                next += interval;
                thread::sleep(next.saturating_duration_since(Instant::now()));
                if sender.blocking_send(sampler.sample()).is_err() {
                    return;
                }
            }
        })
        .context("starting the sampling thread")?;
    Ok(receiver)
}

/// Folds samples into snapshots and ticks the ledger after each one, until the sampling
/// thread stops.
pub async fn run(state: AppState, mut samples: mpsc::Receiver<SampleResult>) {
    let mut builder = SnapshotBuilder::new();
    while let Some(sample) = samples.recv().await {
        match sample {
            Ok(raw) => ingest(&state, &mut builder, raw).await,
            Err(error) => tracing::warn!("sampling failed: {error}"),
        }
    }
    tracing::warn!("sampling stopped");
}

async fn ingest(state: &AppState, builder: &mut SnapshotBuilder, raw: RawSample) {
    let processes = ProcessTable::new(&raw.processes);
    let sessions = state.live_sessions(&processes);
    let snapshot = builder.build(raw, &sessions);
    state.set_latest(Latest {
        snapshot,
        processes,
    });
    let mut ledger = state.ledger.lock().await;
    if let Err(error) = state.tick(&mut ledger).await {
        tracing::error!("persisting lease transitions failed: {error:#}");
    }
}
