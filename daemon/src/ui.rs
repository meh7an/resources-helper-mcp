//! The tray flyout's whole view, built from the ledger, the latest sample, and the database.

use std::collections::HashMap;

use rh_core::{admission, ancestry::ProcessNode, binding};
use rh_shared::{
    api::{HealthResponse, Verdict},
    types::{Holder, Lease, LeaseId, LeaseState, SessionKey, Snapshot},
    ui::{
        ActiveJob, CpuMeter, GpuMeter, JobProcess, Meters, Notice, QueuedJob, RamMeter, RecentRun,
        UiSession, UiState,
    },
};

use crate::{
    db::repo,
    error::AppError,
    state::{AppState, Latest, now_ms},
};

/// Finished runs the idle flyout lists.
const RECENT_RUNS: u32 = 5;

/// Kills and removals the flyout keeps, delivered or not.
const RECENT_NOTICES: u32 = 20;

/// What `GET /ui/state` reads from the database.
struct Stored {
    first_session_at_ms: Option<i64>,
    cwds: HashMap<SessionKey, String>,
    recent: Vec<RecentRun>,
    notices: Vec<Notice>,
}

/// Builds the flyout's state with `service` describing the daemon.
///
/// # Errors
///
/// [`AppError::Unavailable`] before the first sample, and an internal error when the database
/// cannot be read.
pub async fn ui_state(state: &AppState, service: HealthResponse) -> Result<UiState, AppError> {
    let latest = state.latest().ok_or(AppError::Unavailable)?;
    let now = now_ms();
    let (tracked, mut verdicts) = {
        let ledger = state.ledger.lock().await;
        let verdicts: HashMap<LeaseId, Verdict> = ledger
            .leases()
            .iter()
            .filter(|lease| lease.state == LeaseState::Queued)
            .filter_map(|lease| {
                let verdict = ledger.verdict_for(lease.id, &latest.snapshot, now)?;
                Some((lease.id, verdict))
            })
            .collect();
        (ledger.leases().to_vec(), verdicts)
    };

    let sessions: Vec<SessionKey> = latest
        .snapshot
        .sessions
        .iter()
        .map(|usage| usage.holder.session.clone())
        .collect();
    let mut stored = state
        .with_db(move |conn| {
            let mut cwds = HashMap::new();
            for session in sessions {
                if let Some(cwd) = repo::latest_cwd(conn, &session)? {
                    cwds.insert(session, cwd);
                }
            }
            Ok(Stored {
                first_session_at_ms: repo::first_session_at_ms(conn)?,
                cwds,
                recent: repo::recent_runs(conn, RECENT_RUNS)?,
                notices: repo::recent_notices(conn, RECENT_NOTICES)?,
            })
        })
        .await?;

    let mut active: Vec<&Lease> = tracked
        .iter()
        .filter(|lease| lease.state == LeaseState::Active)
        .collect();
    active.sort_by_key(|lease| (lease.granted_at_ms, lease.id));
    let queue = tracked
        .iter()
        .filter(|lease| lease.state == LeaseState::Queued)
        .zip(1..)
        .map(|(lease, position)| QueuedJob {
            lease: lease.clone(),
            position,
            verdict: verdicts.remove(&lease.id).unwrap_or_else(unexplained),
        })
        .collect();
    let headroom_pct = state.settings().config.admission.headroom_pct;

    Ok(UiState {
        generated_at_ms: now,
        service,
        first_session_at_ms: stored.first_session_at_ms,
        sessions: latest
            .snapshot
            .sessions
            .iter()
            .map(|usage| UiSession {
                session: usage.holder.session.clone(),
                cwd: stored.cwds.remove(&usage.holder.session),
                usage: usage.usage,
            })
            .collect(),
        active: active
            .iter()
            .map(|lease| ActiveJob {
                lease: (*lease).clone(),
                process: job_process(lease, &latest),
            })
            .collect(),
        queue,
        meters: meters(&latest, &active, headroom_pct),
        recent: stored.recent,
        notices: stored.notices,
    })
}

/// The process the flyout shows for an active lease: the busiest process of its bound tree,
/// or the root while the tree is idle; without a running bound tree, the session's busiest
/// process, which a kill never targets.
fn job_process(lease: &Lease, latest: &Latest) -> Option<JobProcess> {
    let table = &latest.processes;
    let cores: HashMap<u32, f32> = latest
        .snapshot
        .sessions
        .iter()
        .filter(|usage| usage.holder.session == lease.holder.session)
        .flat_map(|usage| &usage.processes)
        .map(|process| (process.pid, process.usage.cores))
        .collect();
    let load = |node: &ProcessNode| cores.get(&node.pid).copied().unwrap_or(0.0);

    let root = lease
        .binding
        .as_ref()
        .and_then(|binding| binding.root)
        .filter(|root| table.is_running(root.pid, root.start_time))
        .and_then(|root| table.get(root.pid));
    if let Some(root) = root {
        let tree = Holder {
            session: lease.holder.session.clone(),
            pid: root.pid,
            pid_start_time: root.start_time,
        };
        let shown = binding::busiest_descendant(&tree, table, load).unwrap_or(root);
        return Some(job_process_of(shown, true));
    }
    binding::busiest_descendant(&lease.holder, table, load).map(|node| job_process_of(node, false))
}

fn job_process_of(node: &ProcessNode, bound: bool) -> JobProcess {
    JobProcess {
        pid: node.pid,
        name: node.name.clone(),
        exe: node.exe.clone(),
        bound,
    }
}

/// A queued lease the ledger stopped tracking between the two reads.
fn unexplained() -> Verdict {
    Verdict {
        fits: false,
        reasons: Vec::new(),
        wait_estimate_secs: None,
        benchmark: None,
    }
}

fn meters(latest: &Latest, active: &[&Lease], headroom_pct: f32) -> Meters {
    let snapshot: &Snapshot = &latest.snapshot;
    let reserved_cores = active.iter().map(|lease| lease.profile.cores).sum();
    let reserved_mb = active.iter().map(|lease| lease.profile.ram_mb).sum();
    let reserved_gpu_pct = active.iter().map(|lease| lease.profile.gpu_pct).sum();
    let reserved_vram_mb = active.iter().map(|lease| lease.profile.vram_mb).sum();
    // Leases reserve on the adapter the admission engine checks; the others show no share.
    let primary = admission::primary_adapter(&snapshot.gpus).map(|gpu| gpu.luid.as_str());
    Meters {
        headroom_pct,
        cpu: CpuMeter {
            logical_cores: snapshot.cpu.logical_cores,
            usage_pct: snapshot.cpu.usage_pct.avg_1s,
            history_pct: latest.cpu_history_pct.clone(),
            reserved_cores,
        },
        ram: RamMeter {
            total_mb: snapshot.memory.total_mb,
            used_mb: snapshot.memory.used_mb,
            reserved_mb,
        },
        gpus: snapshot
            .gpus
            .iter()
            .map(|gpu| {
                let reserving = Some(gpu.luid.as_str()) == primary;
                GpuMeter {
                    name: gpu.name.clone(),
                    busiest_pct: gpu.engines.busiest(|load| load.avg_1s),
                    vram_used_mb: gpu.dedicated_used_mb,
                    vram_total_mb: gpu.dedicated_total_mb,
                    reserved_pct: if reserving { reserved_gpu_pct } else { 0.0 },
                    reserved_vram_mb: if reserving { reserved_vram_mb } else { 0 },
                }
            })
            .collect(),
        drives: snapshot.drives.clone(),
    }
}
