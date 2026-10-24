//! The sampling thread and the task that turns its samples into snapshots and ledger ticks.

use std::{
    collections::VecDeque,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use rh_core::{
    binding::{self, BindOutcome},
    ledger::Ledger,
    process::ProcessTable,
    sampler::{RawSample, Sampler, SnapshotBuilder},
};
use rh_shared::types::{BoundProcess, LeaseBinding, LeaseId, LeaseState, SessionKey};
use tokio::sync::mpsc;

use crate::{
    db::repo,
    state::{AppState, Latest, now_ms},
};

/// Once-a-second CPU loads the flyout's sparkline shows.
const CPU_HISTORY: usize = 24;

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

/// Folds samples into snapshots, ticks the ledger, and finds the processes of bound commands
/// after each one, until the sampling thread stops.
pub async fn run(state: AppState, mut samples: mpsc::Receiver<SampleResult>) {
    let mut builder = SnapshotBuilder::new();
    let mut cpu_history = VecDeque::with_capacity(CPU_HISTORY);
    while let Some(sample) = samples.recv().await {
        match sample {
            Ok(raw) => ingest(&state, &mut builder, &mut cpu_history, raw).await,
            Err(error) => tracing::warn!("sampling failed: {error}"),
        }
    }
    tracing::warn!("sampling stopped");
}

async fn ingest(
    state: &AppState,
    builder: &mut SnapshotBuilder,
    cpu_history: &mut VecDeque<f32>,
    raw: RawSample,
) {
    let processes = ProcessTable::new(&raw.processes);
    let sessions = state.live_sessions(&processes);
    let snapshot = builder.build(raw, &sessions);
    if cpu_history.len() == CPU_HISTORY {
        cpu_history.pop_front();
    }
    cpu_history.push_back(snapshot.cpu.usage_pct.avg_1s);
    state.set_latest(Latest {
        snapshot,
        processes,
        cpu_history_pct: cpu_history.iter().copied().collect(),
    });
    let mut ledger = state.ledger.lock().await;
    if let Err(error) = state.tick(&mut ledger).await {
        tracing::error!("persisting lease transitions failed: {error:#}");
    }
    if let Err(error) = rebind(state, &mut ledger).await {
        tracing::error!("persisting command bindings failed: {error:#}");
    }
}

/// Stores the root process of every active lease's bound command that the latest sample shows
/// for the first time, or anew after the earlier root exited.
async fn rebind(state: &AppState, ledger: &mut Ledger) -> anyhow::Result<()> {
    let Some(latest) = state.latest() else {
        return Ok(());
    };
    let now = now_ms();
    let found: Vec<(LeaseId, SessionKey, BoundProcess, LeaseBinding)> = ledger
        .leases()
        .iter()
        .filter(|lease| lease.state == LeaseState::Active)
        .filter_map(|lease| {
            let current = lease.binding.as_ref()?;
            match binding::resolve(current, &lease.holder, &latest.processes, now) {
                BindOutcome::Bound(root) if current.root != Some(root) => Some((
                    lease.id,
                    lease.holder.session.clone(),
                    root,
                    LeaseBinding {
                        root: Some(root),
                        ..current.clone()
                    },
                )),
                _ => None,
            }
        })
        .collect();
    for (id, session, root, found_binding) in found {
        ledger.bind(id, &session, found_binding.clone())?;
        tracing::info!(lease = id.0, pid = root.pid, "command process found");
        state
            .with_db(move |conn| repo::bind_lease(conn, id, &found_binding))
            .await?;
    }
    Ok(())
}
