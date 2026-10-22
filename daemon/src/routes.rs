//! HTTP routes. Every route except `/health` requires the bearer token.

use std::time::{Duration, Instant};

use anyhow::anyhow;
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
};
use rh_core::{
    history::{self, ESTIMATE_RUNS, Estimate},
    ledger::{LedgerError, Transition},
    meter::MeteredRun,
};
use rh_shared::{
    api::{
        AcquireRequest, AcquireResponse, CanFitRequest, HealthResponse, HeartbeatResponse,
        HistoryResponse, HistoryRunsQuery, HistoryRunsResponse, LeaseOwner, LeaseQuery,
        LeasesResponse, ProfileSpec, RegisterSessionRequest, ReleaseResponse, SessionsResponse,
        Verdict, WaitRequest,
    },
    config::Config,
    types::{
        JobClass, JobProfile, JobRun, Lease, LeaseId, LeaseKind, LeaseState, SessionKey, Snapshot,
    },
};
use tokio::sync::watch;

use crate::{
    db::repo::{self, LeaseDraft},
    error::AppError,
    state::{AppState, now_ms},
};

// Longer waits risk client and proxy timeouts; the caller can heartbeat and ask again.
const MAX_ACQUIRE_WAIT: Duration = Duration::from_secs(120);

// Overlapped runs are skipped, so an estimate reads more rows than it uses.
const ESTIMATE_SCAN: u32 = ESTIMATE_RUNS as u32 * 5;

// Rows the history summary groups; the default retention cap keeps the table at or below it.
const SUMMARY_SCAN: u32 = 5_000;

const DEFAULT_HISTORY_RUNS: u32 = 20;
const MAX_HISTORY_RUNS: u32 = 500;

/// Every route, with the token check on all but `/health`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/snapshot", get(snapshot))
        .route("/can-fit", post(can_fit))
        .route("/leases", get(list_leases).post(acquire))
        .route("/leases/{id}", delete(release))
        .route("/leases/{id}/heartbeat", post(heartbeat))
        .route("/leases/{id}/wait", post(wait))
        .route("/sessions", get(sessions).post(register_session))
        .route("/history", get(job_history))
        .route("/history/runs", get(history_runs))
        .route("/shutdown", post(shutdown))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .route("/health", get(health))
        .with_state(state)
}

async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented != Some(state.token.as_str()) {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(request).await)
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        uptime_secs: state.started.elapsed().as_secs(),
        config_error: state.config_error(),
    })
}

/// Stops the daemon gracefully after this response; Windows has no signal another process
/// can send a detached daemon.
async fn shutdown(State(state): State<AppState>) -> StatusCode {
    tracing::info!("shutdown requested over HTTP");
    state.shutdown.send_replace(true);
    StatusCode::ACCEPTED
}

async fn snapshot(State(state): State<AppState>) -> Result<Json<Snapshot>, AppError> {
    let latest = state.latest().ok_or(AppError::Unavailable)?;
    Ok(Json(latest.snapshot.clone()))
}

async fn sessions(State(state): State<AppState>) -> Result<Json<SessionsResponse>, AppError> {
    let latest = state.latest().ok_or(AppError::Unavailable)?;
    Ok(Json(SessionsResponse {
        sessions: latest.snapshot.sessions.clone(),
    }))
}

async fn register_session(
    State(state): State<AppState>,
    Json(request): Json<RegisterSessionRequest>,
) -> StatusCode {
    state.register(request.holder);
    StatusCode::NO_CONTENT
}

async fn can_fit(
    State(state): State<AppState>,
    Json(request): Json<CanFitRequest>,
) -> Result<Json<Verdict>, AppError> {
    let (declared, _) = resolve_profile(&state.settings().config, &request.profile)?;
    let latest = state.latest().ok_or(AppError::Unavailable)?;
    let estimate = match &request.label {
        Some(label) => {
            let cwd = request.cwd.as_deref().unwrap_or_default();
            estimate(&state, declared, label, cwd).await?
        }
        None => Estimate {
            profile: declared,
            reasons: Vec::new(),
        },
    };
    let ledger = state.ledger.lock().await;
    let mut verdict = ledger.can_fit(&estimate.profile, request.kind, &latest.snapshot, now_ms());
    verdict.reasons.splice(0..0, estimate.reasons);
    Ok(Json(verdict))
}

/// `declared` raised by what similar past runs of `label` in `cwd` used. Before the first
/// sample there is no ceiling to cap at, so history is left out.
async fn estimate(
    state: &AppState,
    declared: JobProfile,
    label: &str,
    cwd: &str,
) -> Result<Estimate, AppError> {
    let unchanged = Estimate {
        profile: declared,
        reasons: Vec::new(),
    };
    let Some(latest) = state.latest() else {
        return Ok(unchanged);
    };
    let fingerprint = history::fingerprint(&state.settings().classifier, label, cwd);
    let runs = state
        .with_db(move |conn| repo::recent_job_runs(conn, &fingerprint, ESTIMATE_SCAN))
        .await?;
    let Some(learned) = history::learned(&runs) else {
        return Ok(unchanged);
    };
    let ceiling = state.ledger.lock().await.ceiling(&latest.snapshot);
    Ok(history::estimate(&declared, Some(&learned), &ceiling))
}

async fn job_history(State(state): State<AppState>) -> Result<Json<HistoryResponse>, AppError> {
    let runs = state
        .with_db(|conn| repo::latest_job_runs(conn, SUMMARY_SCAN))
        .await?;
    Ok(Json(HistoryResponse {
        jobs: history::summarize(&runs),
    }))
}

async fn history_runs(
    State(state): State<AppState>,
    Query(query): Query<HistoryRunsQuery>,
) -> Result<Json<HistoryRunsResponse>, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_RUNS)
        .min(MAX_HISTORY_RUNS);
    let runs = state
        .with_db(move |conn| repo::recent_job_runs(conn, &query.fingerprint, limit))
        .await?;
    Ok(Json(HistoryRunsResponse { runs }))
}

async fn list_leases(
    State(state): State<AppState>,
    Query(query): Query<LeaseQuery>,
) -> Result<Json<LeasesResponse>, AppError> {
    let leases = state
        .with_db(move |conn| repo::list_leases(conn, &query))
        .await?;
    Ok(Json(LeasesResponse { leases }))
}

async fn acquire(
    State(state): State<AppState>,
    Json(request): Json<AcquireRequest>,
) -> Result<Json<AcquireResponse>, AppError> {
    let (declared, class) = resolve_profile(&state.settings().config, &request.profile)?;
    let cwd = request.cwd.unwrap_or_default();
    let estimate = estimate(&state, declared, &request.label, &cwd).await?;
    state.register(request.holder.clone());
    let session = request.holder.session.clone();
    let now = now_ms();
    let ttl_ms = i64::try_from(state.heartbeat_ttl().as_millis()).unwrap_or(i64::MAX);
    let draft = LeaseDraft {
        holder: request.holder,
        kind: request.kind,
        class,
        profile: estimate.profile,
        label: request.label,
        cwd,
        requested_at_ms: now,
        expires_at_ms: now.saturating_add(ttl_ms),
    };

    let changes = state.changes.subscribe();
    let lease = state
        .with_db(move |conn| repo::insert_lease(conn, &draft))
        .await?;
    let id = lease.id;
    {
        let mut ledger = state.ledger.lock().await;
        ledger.enqueue(lease)?;
        state.tick(&mut ledger).await?;
    }
    let Json(mut response) =
        wait_for_grant(&state, id, &session, request.wait_timeout_secs, changes).await?;
    response.verdict.reasons.splice(0..0, estimate.reasons);
    Ok(Json(response))
}

/// Keeps waiting for a queued lease, which keeps its place. Answers at once for a lease
/// that is already active or has ended.
async fn wait(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(owner): Query<LeaseOwner>,
    Json(request): Json<WaitRequest>,
) -> Result<Json<AcquireResponse>, AppError> {
    let id = LeaseId(id);
    let changes = state.changes.subscribe();
    stored_lease(&state, id, &owner.session).await?;
    wait_for_grant(
        &state,
        id,
        &owner.session,
        request.wait_timeout_secs,
        changes,
    )
    .await
}

/// Holds the request open until lease `id` stops being queued or `wait_timeout_secs` pass,
/// then answers with the lease and why it runs or waits.
async fn wait_for_grant(
    state: &AppState,
    id: LeaseId,
    session: &SessionKey,
    wait_timeout_secs: u64,
    mut changes: watch::Receiver<u64>,
) -> Result<Json<AcquireResponse>, AppError> {
    let wait = Duration::from_secs(wait_timeout_secs).min(MAX_ACQUIRE_WAIT);
    let deadline = Instant::now() + wait;
    // The client can only heartbeat the lease once this response names it, and the open
    // request already proves the holder is alive, so the wait keeps the lease from expiring.
    let renew_every = state.heartbeat_ttl() / 3;
    let mut renew_at = Instant::now() + renew_every;
    let mut stopping = state.shutdown.subscribe();
    while is_queued(state, id).await {
        let now = Instant::now();
        if now >= deadline || *stopping.borrow() {
            break;
        }
        if now >= renew_at {
            renew(state, id, session).await?;
            renew_at = now + renew_every;
        }
        let wake = deadline.min(renew_at).saturating_duration_since(now);
        tokio::select! {
            changed = tokio::time::timeout(wake, changes.changed()) => {
                if let Ok(Err(_)) = changed {
                    break;
                }
            }
            _ = stopping.wait_for(|stop| *stop) => break,
        }
    }
    // A full TTL from the response gives the client time to start heartbeating.
    renew(state, id, session).await?;

    let lease = state
        .with_db(move |conn| repo::get_lease(conn, id))
        .await?
        .ok_or_else(|| anyhow!("lease {} vanished from the database", id.0))?;
    let mut verdict = lease_verdict(state, &lease).await;
    if lease.state == LeaseState::Queued && *stopping.borrow() {
        verdict.reasons.push(format!(
            "rhd is restarting; the lease keeps its place, so resume it with lease_id {} once \
             rhd is back",
            id.0
        ));
    }
    Ok(Json(AcquireResponse { lease, verdict }))
}

async fn heartbeat(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(owner): Query<LeaseOwner>,
) -> Result<Json<HeartbeatResponse>, AppError> {
    let id = LeaseId(id);
    let (lease_state, expires_at_ms) = match renew(&state, id, &owner.session).await? {
        Some(renewed) => renewed,
        None => {
            let lease = stored_lease(&state, id, &owner.session).await?;
            (lease.state, lease.expires_at_ms)
        }
    };
    Ok(Json(HeartbeatResponse {
        state: lease_state,
        expires_at_ms,
    }))
}

/// Moves a tracked lease's deadline one TTL from now, in the ledger and the database, and
/// returns its state and new deadline. `None` when the ledger no longer tracks the lease.
async fn renew(
    state: &AppState,
    id: LeaseId,
    session: &SessionKey,
) -> Result<Option<(LeaseState, i64)>, AppError> {
    let mut ledger = state.ledger.lock().await;
    let (lease_state, expires_at_ms) = match ledger.heartbeat(id, session, now_ms()) {
        Ok(lease) => (lease.state, lease.expires_at_ms),
        Err(LedgerError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    state
        .with_db(move |conn| repo::renew_lease(conn, id, expires_at_ms))
        .await?;
    Ok(Some((lease_state, expires_at_ms)))
}

async fn release(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(owner): Query<LeaseOwner>,
) -> Result<Json<ReleaseResponse>, AppError> {
    let id = LeaseId(id);
    let now = now_ms();
    let mut ledger = state.ledger.lock().await;
    let finished = match ledger.release(id, &owner.session, now) {
        Ok(released) => {
            let transition = Transition {
                id,
                to: LeaseState::Released,
                reason: "released by holder".to_owned(),
            };
            state.persist(&[transition], now).await?;
            state.notify_changes();
            // The freed room may let queued leases start right away.
            state.tick(&mut ledger).await?;
            released
                .run
                .map(|run| job_run(&state, &released.lease, &run, now))
        }
        Err(LedgerError::NotFound(_)) => None,
        Err(error) => return Err(error.into()),
    };
    drop(ledger);

    let noise = finished.as_ref().and_then(|job| job.noise);
    if let Some(job) = finished {
        // The release itself succeeded; losing the history row must not hide the noise
        // report from the holder.
        if let Err(error) = state
            .with_db(move |conn| repo::insert_job_run(conn, &job))
            .await
        {
            tracing::error!(lease = id.0, "storing the job run failed: {error:#}");
        }
    }
    let lease = stored_lease(&state, id, &owner.session).await?;
    Ok(Json(ReleaseResponse { lease, noise }))
}

/// The history row of a lease its holder released. Noise is kept for benchmarks only; other
/// leases share the machine on purpose.
fn job_run(state: &AppState, lease: &Lease, run: &MeteredRun, ended_at_ms: i64) -> JobRun {
    JobRun {
        lease_id: Some(lease.id),
        fingerprint: history::fingerprint(&state.settings().classifier, &lease.label, &lease.cwd),
        command: lease.label.clone(),
        cwd: lease.cwd.clone(),
        declared: lease.profile,
        peak: run.holder_peak,
        avg: run.holder_avg,
        started_at_ms: lease.granted_at_ms.unwrap_or(lease.requested_at_ms),
        ended_at_ms,
        noise: (lease.kind == LeaseKind::BenchmarkExclusive).then_some(run.noise),
        overlapped: run.overlapped,
    }
}

/// Resolves a class through the config and checks the values.
fn resolve_profile(
    config: &Config,
    spec: &ProfileSpec,
) -> Result<(JobProfile, Option<JobClass>), AppError> {
    let (profile, class) = match spec {
        ProfileSpec::Class(class) => (*config.classes.get(*class), Some(*class)),
        ProfileSpec::Custom(profile) => (*profile, None),
        _ => return Err(AppError::Validation("unsupported profile form".to_owned())),
    };
    if let Some(reason) = profile.invalid_reason() {
        return Err(AppError::Validation(format!("profile: {reason}")));
    }
    Ok((profile, class))
}

async fn is_queued(state: &AppState, id: LeaseId) -> bool {
    let ledger = state.ledger.lock().await;
    ledger
        .leases()
        .iter()
        .any(|lease| lease.id == id && lease.state == LeaseState::Queued)
}

/// Why `lease` runs or waits, from the ledger while it tracks the lease.
async fn lease_verdict(state: &AppState, lease: &Lease) -> Verdict {
    let explained = {
        let ledger = state.ledger.lock().await;
        state
            .latest()
            .and_then(|latest| ledger.verdict_for(lease.id, &latest.snapshot, now_ms()))
    };
    explained.unwrap_or_else(|| {
        let reason = if lease.state == LeaseState::Queued {
            "waiting for the first machine sample".to_owned()
        } else {
            format!("lease is {}", lease.state.as_str())
        };
        Verdict {
            fits: false,
            reasons: vec![reason],
            wait_estimate_secs: None,
            benchmark: None,
        }
    })
}

/// A lease from the database, refused when it belongs to another session.
async fn stored_lease(
    state: &AppState,
    id: LeaseId,
    session: &SessionKey,
) -> Result<Lease, AppError> {
    let lease = state
        .with_db(move |conn| repo::get_lease(conn, id))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("lease {} does not exist", id.0)))?;
    if &lease.holder.session != session {
        return Err(LedgerError::NotOwner(id).into());
    }
    Ok(lease)
}
