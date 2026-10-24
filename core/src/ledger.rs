//! The lease ledger: the queue, grants, heartbeats, and reaping.
//!
//! Pure and deterministic: every call takes the current time, nothing touches the OS or the
//! database. The caller persists the [`Transition`]s it returns.
//!
//! The queue is first in, first out with EASY backfill. When the oldest waiting lease (the
//! head) does not fit, the ledger works out when it will, from the expected ends of the
//! running leases. A later request may start before it only if it fits now and either ends
//! by then or leaves the head enough room at that time.

use rh_shared::{
    api::{BenchmarkProgress, Verdict},
    config::Config,
    types::{Holder, JobProfile, Lease, LeaseId, LeaseKind, LeaseState, SessionKey, Snapshot},
};

use crate::{
    admission::{self, Room, Running},
    benchmark::{Quiet, QuietLimits, QuietWindow},
    meter::{MeteredRun, RunMeter},
};

/// The config values the ledger works with, with times in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LedgerSettings {
    /// Share of every resource never handed out.
    pub headroom_pct: f32,
    /// A lease without a heartbeat for this long expires.
    pub heartbeat_ttl_ms: i64,
    /// When a drained machine is quiet enough for a benchmark.
    pub quiet: QuietLimits,
}

impl LedgerSettings {
    /// Takes the admission, benchmark, and lease sections of `config`.
    pub fn from_config(config: &Config) -> Self {
        Self {
            headroom_pct: config.admission.headroom_pct,
            heartbeat_ttl_ms: secs_to_ms(config.leases.heartbeat_ttl_secs),
            quiet: QuietLimits {
                cpu_pct: config.benchmark.quiet_cpu_pct,
                gpu_pct: config.benchmark.quiet_gpu_pct,
                window_ms: secs_to_ms(config.benchmark.quiet_secs),
                max_wait_ms: secs_to_ms(config.benchmark.max_quiet_wait_secs),
            },
        }
    }
}

fn secs_to_ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

/// Whether the process holding a lease is still running.
pub trait ProcessLiveness {
    /// `true` when a process with the holder's PID and start time exists.
    fn is_alive(&self, holder: &Holder) -> bool;
}

/// A state change the caller must persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// The lease that changed.
    pub id: LeaseId,
    /// Its new state.
    pub to: LeaseState,
    /// Why, for the audit trail.
    pub reason: String,
}

/// Requests the ledger refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LedgerError {
    /// No tracked lease has this ID; it may have ended already.
    #[error("lease {} is not tracked", .0.0)]
    NotFound(LeaseId),
    /// The lease belongs to another session.
    #[error("lease {} belongs to another session", .0.0)]
    NotOwner(LeaseId),
    /// Only queued leases can be enqueued.
    #[error("lease {} is not queued", .0.0)]
    NotQueued(LeaseId),
    /// A lease with this ID is already tracked.
    #[error("lease {} is already tracked", .0.0)]
    AlreadyTracked(LeaseId),
    /// Only queued and active leases can be restored.
    #[error("lease {} has already ended", .0.0)]
    Ended(LeaseId),
}

/// A lease its holder ended, with what it measured while it was active.
#[derive(Debug, Clone, PartialEq)]
pub struct Released {
    /// The lease in its final state.
    pub lease: Lease,
    /// Holder usage and noise of the run; `None` for a lease released while queued and when
    /// no sample arrived while it was active.
    pub run: Option<MeteredRun>,
}

/// Queued and active leases, in the order they were enqueued.
#[derive(Debug, Clone)]
pub struct Ledger {
    settings: LedgerSettings,
    leases: Vec<Lease>,
    quiet: QuietWindow,
    /// One per active lease.
    meters: Vec<RunMeter>,
}

#[derive(Debug, Clone)]
struct Candidate<'a> {
    profile: &'a JobProfile,
    kind: LeaseKind,
    label: String,
    requested_at_ms: i64,
}

#[derive(Debug, Default)]
struct Decision {
    granted: bool,
    reasons: Vec<String>,
    /// Where a waiting benchmark stands.
    progress: Option<BenchmarkProgress>,
    /// The quiet check a benchmark candidate passed or failed.
    quiet: Option<Quiet>,
}

/// What the first blocked candidate of a pass protects from later ones.
#[derive(Debug)]
enum Hold {
    Free,
    /// A normal head that fits at `at_ms`, with `spare` room left beside it then.
    Shadow {
        label: String,
        at_ms: i64,
        spare: Room,
    },
    /// A benchmark head: the machine must drain, so nothing else starts.
    Exclusive(String),
}

#[derive(Debug)]
struct Pass {
    room: Room,
    running: Vec<Running>,
    active_count: usize,
    exclusive: Option<String>,
    hold: Hold,
}

impl Ledger {
    /// An empty ledger.
    pub fn new(settings: LedgerSettings) -> Self {
        Self {
            settings,
            leases: Vec::new(),
            quiet: QuietWindow::default(),
            meters: Vec::new(),
        }
    }

    /// Queued and active leases, oldest first.
    pub fn leases(&self) -> &[Lease] {
        &self.leases
    }

    /// Adds a newly stored lease to the back of the queue. It is granted on a later
    /// [`Ledger::tick`].
    ///
    /// # Errors
    ///
    /// [`LedgerError::NotQueued`] when the lease is not `Queued`, and
    /// [`LedgerError::AlreadyTracked`] when its ID is already in the ledger.
    pub fn enqueue(&mut self, lease: Lease) -> Result<(), LedgerError> {
        if lease.state != LeaseState::Queued {
            return Err(LedgerError::NotQueued(lease.id));
        }
        if self.leases.iter().any(|tracked| tracked.id == lease.id) {
            return Err(LedgerError::AlreadyTracked(lease.id));
        }
        self.leases.push(lease);
        Ok(())
    }

    /// Tracks a lease that a previous daemon run left queued or active, in its state and at
    /// the back of the queue, so restore leases oldest first. Its heartbeat deadline moves to
    /// at least one TTL from `now_ms`, which gives the holder time to reach the new daemon; a
    /// dead holder is reaped on the next tick. An active lease gets no meter: the samples from
    /// before the restart are gone, so its run is not recorded.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Ended`] when the lease is neither queued nor active, and
    /// [`LedgerError::AlreadyTracked`] when its ID is already in the ledger.
    pub fn restore(&mut self, mut lease: Lease, now_ms: i64) -> Result<&Lease, LedgerError> {
        if !matches!(lease.state, LeaseState::Queued | LeaseState::Active) {
            return Err(LedgerError::Ended(lease.id));
        }
        if self.leases.iter().any(|tracked| tracked.id == lease.id) {
            return Err(LedgerError::AlreadyTracked(lease.id));
        }
        let fresh_deadline = now_ms.saturating_add(self.settings.heartbeat_ttl_ms);
        lease.expires_at_ms = lease.expires_at_ms.max(fresh_deadline);
        self.leases.push(lease);
        Ok(&self.leases[self.leases.len() - 1])
    }

    /// Replaces the settings after a config reload. Deadlines already set keep their old TTL
    /// until the next heartbeat.
    pub fn set_settings(&mut self, settings: LedgerSettings) {
        self.settings = settings;
    }

    /// Moves a lease's heartbeat deadline to one TTL from `now_ms`.
    ///
    /// # Errors
    ///
    /// [`LedgerError::NotFound`] or [`LedgerError::NotOwner`].
    pub fn heartbeat(
        &mut self,
        id: LeaseId,
        session: &SessionKey,
        now_ms: i64,
    ) -> Result<&Lease, LedgerError> {
        let index = self.owned_index(id, session)?;
        let lease = &mut self.leases[index];
        lease.expires_at_ms = now_ms.saturating_add(self.settings.heartbeat_ttl_ms);
        Ok(lease)
    }

    /// Ends a queued or active lease at its holder's request and stops tracking it.
    ///
    /// # Errors
    ///
    /// [`LedgerError::NotFound`] or [`LedgerError::NotOwner`].
    pub fn release(
        &mut self,
        id: LeaseId,
        session: &SessionKey,
        now_ms: i64,
    ) -> Result<Released, LedgerError> {
        let index = self.owned_index(id, session)?;
        let mut lease = self.leases.remove(index);
        lease.state = LeaseState::Released;
        lease.ended_at_ms = Some(now_ms);
        let run = self
            .meters
            .iter()
            .position(|meter| meter.lease_id() == id)
            .and_then(|index| self.meters.remove(index).finish());
        Ok(Released { lease, run })
    }

    /// Expires leases whose holder died or whose heartbeat stopped, feeds `snapshot` to the
    /// quiet window and to the meters of active leases, and grants every queued lease that
    /// may start now.
    pub fn tick(
        &mut self,
        now_ms: i64,
        snapshot: &Snapshot,
        liveness: &dyn ProcessLiveness,
    ) -> Vec<Transition> {
        let mut transitions = self.reap(now_ms, liveness);
        let drained = self.active().is_empty();
        self.quiet
            .observe(now_ms, snapshot, drained, self.settings.quiet.window_ms);
        for meter in &mut self.meters {
            meter.observe(snapshot);
        }

        let candidates = self.queued_candidates();
        let decisions = self.plan(snapshot, now_ms, &candidates);
        let granted: Vec<(LeaseId, Option<Quiet>)> = self
            .leases
            .iter()
            .filter(|lease| lease.state == LeaseState::Queued)
            .zip(decisions)
            .filter(|(_, decision)| decision.granted)
            .map(|(lease, decision)| (lease.id, decision.quiet))
            .collect();

        for (id, quiet) in granted {
            let Some(lease) = self.leases.iter_mut().find(|lease| lease.id == id) else {
                continue;
            };
            lease.state = LeaseState::Active;
            lease.granted_at_ms = Some(now_ms);
            let is_benchmark = lease.kind == LeaseKind::BenchmarkExclusive;
            let quiet_at_start = matches!(quiet, Some(Quiet::Quiet(_)));
            self.meters.push(RunMeter::start(
                id,
                lease.holder.session.clone(),
                quiet_at_start,
                snapshot,
            ));
            let reason = if is_benchmark {
                grant_reason(&self.settings.quiet, quiet)
            } else {
                "granted".to_owned()
            };
            transitions.push(Transition {
                id,
                to: LeaseState::Active,
                reason,
            });
        }
        self.mark_overlaps();
        transitions
    }

    /// The most a single request could get once every active lease ended: usable capacity
    /// minus the background load.
    pub fn ceiling(&self, snapshot: &Snapshot) -> Room {
        let active = self.active();
        let mut room = Room::measure(snapshot, &active, self.settings.headroom_pct);
        for lease in &active {
            room.release(&lease.profile);
        }
        room
    }

    /// Whether a new request for `profile` would start now if it joined the back of the
    /// queue, with the reasons when it would not.
    ///
    /// The wait estimate assumes active leases end when expected. It is `None` when the wait
    /// depends on the queue rather than on resources, or the request can never fit.
    pub fn can_fit(
        &self,
        profile: &JobProfile,
        kind: LeaseKind,
        snapshot: &Snapshot,
        now_ms: i64,
    ) -> Verdict {
        let mut candidates = self.queued_candidates();
        let queued_ahead = candidates.len();
        candidates.push(Candidate {
            profile,
            kind,
            label: "this request".to_owned(),
            requested_at_ms: now_ms,
        });
        let decision = self
            .plan(snapshot, now_ms, &candidates)
            .pop()
            .unwrap_or_default();
        if decision.granted {
            return fitting(decision.reasons);
        }
        let mut reasons = decision.reasons;
        if queued_ahead > 0 {
            reasons.push(format!("{queued_ahead} lease(s) queued ahead"));
        }
        Verdict {
            fits: false,
            reasons,
            wait_estimate_secs: self.wait_estimate(profile, kind, now_ms, snapshot, now_ms),
            benchmark: decision.progress,
        }
    }

    /// Whether the tracked lease `id` would start now, with the reasons it waits. An active
    /// lease always fits. `None` when the ledger does not track `id`.
    pub fn verdict_for(&self, id: LeaseId, snapshot: &Snapshot, now_ms: i64) -> Option<Verdict> {
        let lease = self.leases.iter().find(|lease| lease.id == id)?;
        if lease.state != LeaseState::Queued {
            return Some(fitting(Vec::new()));
        }
        let position = self
            .leases
            .iter()
            .filter(|lease| lease.state == LeaseState::Queued)
            .position(|lease| lease.id == id)?;
        let candidates = self.queued_candidates();
        let decision = self
            .plan(snapshot, now_ms, &candidates)
            .into_iter()
            .nth(position)?;
        if decision.granted {
            return Some(fitting(decision.reasons));
        }
        let mut reasons = decision.reasons;
        if position > 0 {
            reasons.push(format!("{position} lease(s) queued ahead"));
        }
        let wait_estimate_secs = self.wait_estimate(
            &lease.profile,
            lease.kind,
            lease.requested_at_ms,
            snapshot,
            now_ms,
        );
        Some(Verdict {
            fits: false,
            reasons,
            wait_estimate_secs,
            benchmark: decision.progress,
        })
    }

    fn owned_index(&self, id: LeaseId, session: &SessionKey) -> Result<usize, LedgerError> {
        let index = self
            .leases
            .iter()
            .position(|lease| lease.id == id)
            .ok_or(LedgerError::NotFound(id))?;
        if &self.leases[index].holder.session != session {
            return Err(LedgerError::NotOwner(id));
        }
        Ok(index)
    }

    fn reap(&mut self, now_ms: i64, liveness: &dyn ProcessLiveness) -> Vec<Transition> {
        let ttl_secs = self.settings.heartbeat_ttl_ms / 1_000;
        let mut transitions = Vec::new();
        self.leases.retain(|lease| {
            let reason = if !liveness.is_alive(&lease.holder) {
                "holder process exited".to_owned()
            } else if now_ms >= lease.expires_at_ms {
                format!("no heartbeat for {ttl_secs}s")
            } else {
                return true;
            };
            transitions.push(Transition {
                id: lease.id,
                to: LeaseState::Expired,
                reason,
            });
            false
        });
        let leases = &self.leases;
        self.meters
            .retain(|meter| leases.iter().any(|lease| lease.id == meter.lease_id()));
        transitions
    }

    /// Flags the meters of sessions that hold more than one active lease: their usage is
    /// measured per session, so it no longer belongs to one job.
    fn mark_overlaps(&mut self) {
        let active = self.active();
        let shared: Vec<SessionKey> = active
            .iter()
            .filter(|lease| {
                active
                    .iter()
                    .filter(|other| other.holder.session == lease.holder.session)
                    .count()
                    > 1
            })
            .map(|lease| lease.holder.session.clone())
            .collect();
        for meter in &mut self.meters {
            if shared.contains(meter.session()) {
                meter.mark_overlapped();
            }
        }
    }

    fn active(&self) -> Vec<&Lease> {
        self.leases
            .iter()
            .filter(|lease| lease.state == LeaseState::Active)
            .collect()
    }

    fn queued_candidates(&self) -> Vec<Candidate<'_>> {
        self.leases
            .iter()
            .filter(|lease| lease.state == LeaseState::Queued)
            .map(|lease| Candidate {
                profile: &lease.profile,
                kind: lease.kind,
                label: format!("lease {}", lease.id.0),
                requested_at_ms: lease.requested_at_ms,
            })
            .collect()
    }

    /// Decides every candidate in order, as if each grant happened immediately.
    fn plan(
        &self,
        snapshot: &Snapshot,
        now_ms: i64,
        candidates: &[Candidate<'_>],
    ) -> Vec<Decision> {
        let active = self.active();
        let mut pass = Pass {
            room: Room::measure(snapshot, &active, self.settings.headroom_pct),
            running: active.iter().map(|lease| Running::of(lease)).collect(),
            active_count: active.len(),
            exclusive: active
                .iter()
                .find(|lease| lease.kind == LeaseKind::BenchmarkExclusive)
                .map(|lease| format!("lease {}", lease.id.0)),
            hold: Hold::Free,
        };
        candidates
            .iter()
            .map(|candidate| self.decide(candidate, &mut pass, now_ms))
            .collect()
    }

    fn decide(&self, candidate: &Candidate<'_>, pass: &mut Pass, now_ms: i64) -> Decision {
        let blocked = |reasons: Vec<String>| Decision {
            reasons,
            ..Decision::default()
        };
        if let Some(holder) = &pass.exclusive {
            return blocked(vec![format!("benchmark {holder} holds the machine")]);
        }
        let is_benchmark = candidate.kind == LeaseKind::BenchmarkExclusive;
        match &pass.hold {
            Hold::Exclusive(label) => {
                return blocked(vec![format!(
                    "benchmark {label} is waiting for the machine to drain"
                )]);
            }
            Hold::Shadow { label, .. } if is_benchmark => {
                return blocked(vec![format!("queued behind {label}")]);
            }
            _ => {}
        }

        let mut reasons = Vec::new();
        let mut progress = None;
        let mut quiet = None;
        if is_benchmark {
            if pass.active_count > 0 {
                reasons.push(format!("{} other lease(s) still active", pass.active_count));
                progress = Some(BenchmarkProgress::Draining {
                    active_leases: u32::try_from(pass.active_count).unwrap_or(u32::MAX),
                });
            } else {
                let status =
                    self.quiet
                        .status(now_ms, candidate.requested_at_ms, &self.settings.quiet);
                if let Some((reason, waiting)) = quiet_wait(&self.settings.quiet, status) {
                    reasons.push(reason);
                    progress = Some(waiting);
                }
                quiet = Some(status);
            }
        }
        reasons.extend(pass.room.shortfalls(candidate.profile));

        if reasons.is_empty() {
            if let Hold::Shadow {
                label,
                at_ms,
                spare,
            } = &mut pass.hold
            {
                let ends_ms = now_ms.saturating_add(admission::est_ms(candidate.profile));
                if ends_ms > *at_ms {
                    if !spare.shortfalls(candidate.profile).is_empty() {
                        return blocked(vec![format!(
                            "starting now would delay {label}, queued ahead"
                        )]);
                    }
                    spare.reserve(candidate.profile);
                }
            }
            pass.room.reserve(candidate.profile);
            pass.running
                .push(Running::starting(now_ms, candidate.profile));
            pass.active_count += 1;
            if is_benchmark {
                pass.exclusive = Some(candidate.label.clone());
            }
            return Decision {
                granted: true,
                reasons,
                progress: None,
                quiet,
            };
        }

        if matches!(pass.hold, Hold::Free) {
            pass.hold = if is_benchmark {
                Hold::Exclusive(candidate.label.clone())
            } else {
                match admission::first_fit(pass.room, &pass.running, candidate.profile, now_ms) {
                    Some((at_ms, mut spare)) => {
                        spare.reserve(candidate.profile);
                        Hold::Shadow {
                            label: candidate.label.clone(),
                            at_ms,
                            spare,
                        }
                    }
                    None => {
                        reasons.push("more than the machine can free".to_owned());
                        Hold::Free
                    }
                }
            };
        }
        Decision {
            reasons,
            progress,
            ..Decision::default()
        }
    }

    /// Lower bound of the wait. A benchmark waits for the active leases to end, then for a
    /// full quiet window, then at most until the longest quiet wait runs out.
    fn wait_estimate(
        &self,
        profile: &JobProfile,
        kind: LeaseKind,
        requested_at_ms: i64,
        snapshot: &Snapshot,
        now_ms: i64,
    ) -> Option<u64> {
        let active = self.active();
        let running: Vec<Running> = active.iter().map(|lease| Running::of(lease)).collect();
        if kind == LeaseKind::BenchmarkExclusive {
            let quiet = &self.settings.quiet;
            let wait_ms = if running.is_empty() {
                match self.quiet.status(now_ms, requested_at_ms, quiet) {
                    Quiet::Measuring { left_ms, .. } | Quiet::Noisy { left_ms, .. } => left_ms,
                    Quiet::Quiet(_) | Quiet::Overdue(_) => 0,
                }
            } else {
                let drain_ms = running
                    .iter()
                    .map(|reservation| reservation.end_ms - now_ms)
                    .max()
                    .unwrap_or(0)
                    .max(0);
                drain_ms.saturating_add(quiet.window_ms)
            };
            return Some(admission::ms_to_secs_ceil(wait_ms)).filter(|&secs| secs > 0);
        }
        let room = Room::measure(snapshot, &active, self.settings.headroom_pct);
        admission::wait_estimate_secs(room, &running, profile, now_ms).filter(|&secs| secs > 0)
    }
}

/// A verdict for a request that starts now.
fn fitting(reasons: Vec<String>) -> Verdict {
    Verdict {
        fits: true,
        reasons,
        wait_estimate_secs: None,
        benchmark: None,
    }
}

/// Why a drained machine still holds a benchmark back, or `None` when it may start.
fn quiet_wait(limits: &QuietLimits, status: Quiet) -> Option<(String, BenchmarkProgress)> {
    match status {
        Quiet::Measuring {
            measured_ms, load, ..
        } => {
            let measured_secs = u64::try_from(measured_ms / 1_000).unwrap_or(0);
            let needed_secs = admission::ms_to_secs_ceil(limits.window_ms);
            let reason = format!(
                "measuring background load: {measured_secs}s of {needed_secs}s so far \
                 (CPU {:.1}%, GPU {:.1}%)",
                load.cpu_pct, load.gpu_pct
            );
            let progress = BenchmarkProgress::Measuring {
                measured_secs,
                needed_secs,
                cpu_avg_pct: load.cpu_pct,
                gpu_avg_pct: load.gpu_pct,
            };
            Some((reason, progress))
        }
        Quiet::Noisy { load, left_ms } => {
            let grant_anyway_in_secs = admission::ms_to_secs_ceil(left_ms);
            let reason = format!(
                "background load is above the quiet limits (CPU {:.1}% of {:.0}%, GPU {:.1}% \
                 of {:.0}%); granted anyway in {grant_anyway_in_secs}s",
                load.cpu_pct, limits.cpu_pct, load.gpu_pct, limits.gpu_pct
            );
            let progress = BenchmarkProgress::Noisy {
                cpu_avg_pct: load.cpu_pct,
                gpu_avg_pct: load.gpu_pct,
                cpu_limit_pct: limits.cpu_pct,
                gpu_limit_pct: limits.gpu_pct,
                grant_anyway_in_secs,
            };
            Some((reason, progress))
        }
        Quiet::Quiet(_) | Quiet::Overdue(_) => None,
    }
}

/// The audit-trail reason of a benchmark grant.
fn grant_reason(limits: &QuietLimits, quiet: Option<Quiet>) -> String {
    let window_secs = admission::ms_to_secs_ceil(limits.window_ms);
    match quiet {
        Some(Quiet::Quiet(load)) => format!(
            "granted after {window_secs}s of quiet (CPU {:.1}%, GPU {:.1}%)",
            load.cpu_pct, load.gpu_pct
        ),
        Some(Quiet::Overdue(load)) => format!(
            "granted without quiet after {}s: CPU {:.1}%, GPU {:.1}% over the last \
             {window_secs}s",
            admission::ms_to_secs_ceil(limits.max_wait_ms),
            load.cpu_pct,
            load.gpu_pct
        ),
        _ => "granted".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rh_shared::types::{BoundProcess, LeaseBinding};

    use super::*;
    use crate::test_support::{holder, profile, queued_lease, session_usage, snapshot};

    const TTL_MS: i64 = 60_000;

    fn settings() -> LedgerSettings {
        LedgerSettings {
            headroom_pct: 10.0,
            heartbeat_ttl_ms: TTL_MS,
            quiet: QuietLimits {
                cpu_pct: 10.0,
                gpu_pct: 10.0,
                window_ms: 10_000,
                max_wait_ms: 60_000,
            },
        }
    }

    /// Every holder is alive except the listed PIDs.
    struct Dead(HashSet<u32>);

    impl ProcessLiveness for Dead {
        fn is_alive(&self, holder: &Holder) -> bool {
            !self.0.contains(&holder.pid)
        }
    }

    fn alive() -> Dead {
        Dead(HashSet::new())
    }

    fn idle() -> Snapshot {
        snapshot(0.0, 0, None)
    }

    fn busy() -> Snapshot {
        snapshot(50.0, 0, None)
    }

    /// A sample taken at `now_ms` with total CPU at `cpu_pct`; the quiet window and the
    /// benchmark meter count each sample once, by its timestamp.
    fn sample(now_ms: i64, cpu_pct: f32) -> Snapshot {
        Snapshot {
            taken_at_ms: now_ms,
            ..snapshot(cpu_pct, 0, None)
        }
    }

    /// Ticks once per second from `from_s` to `to_s` inclusive at `cpu_pct`, renewing every
    /// lease, and returns the leases granted.
    fn tick_seconds(ledger: &mut Ledger, from_s: i64, to_s: i64, cpu_pct: f32) -> Vec<i64> {
        let mut granted_ids = Vec::new();
        for second in from_s..=to_s {
            let now_ms = second * 1_000;
            renew_all(ledger, now_ms);
            granted_ids.extend(granted(&ledger.tick(
                now_ms,
                &sample(now_ms, cpu_pct),
                &alive(),
            )));
        }
        granted_ids
    }

    fn session(name: &str) -> SessionKey {
        holder(name).session
    }

    fn timed(cores: f32, est_secs: u64) -> JobProfile {
        JobProfile {
            est_secs,
            ..profile(cores, 1_000)
        }
    }

    fn enqueue(ledger: &mut Ledger, id: i64, cores: f32, est_secs: u64, now_ms: i64) {
        let lease = queued_lease(id, "s", timed(cores, est_secs), LeaseKind::Normal, now_ms);
        ledger.enqueue(lease).unwrap();
    }

    fn enqueue_benchmark(ledger: &mut Ledger, id: i64, now_ms: i64) {
        let lease = queued_lease(
            id,
            "bench",
            profile(1.0, 1_000),
            LeaseKind::BenchmarkExclusive,
            now_ms,
        );
        ledger.enqueue(lease).unwrap();
    }

    fn can_fit_benchmark(ledger: &Ledger, now_ms: i64) -> Verdict {
        let kind = LeaseKind::BenchmarkExclusive;
        ledger.can_fit(&profile(1.0, 1_000), kind, &idle(), now_ms)
    }

    fn granted(transitions: &[Transition]) -> Vec<i64> {
        transitions
            .iter()
            .filter(|transition| transition.to == LeaseState::Active)
            .map(|transition| transition.id.0)
            .collect()
    }

    fn states(ledger: &Ledger) -> Vec<(i64, LeaseState)> {
        ledger
            .leases()
            .iter()
            .map(|lease| (lease.id.0, lease.state))
            .collect()
    }

    fn has_reason(verdict: &Verdict, needle: &str) -> bool {
        verdict.reasons.iter().any(|reason| reason.contains(needle))
    }

    fn renew_all(ledger: &mut Ledger, now_ms: i64) {
        let ids: Vec<(LeaseId, SessionKey)> = ledger
            .leases()
            .iter()
            .map(|lease| (lease.id, lease.holder.session.clone()))
            .collect();
        for (id, session) in ids {
            ledger.heartbeat(id, &session, now_ms).unwrap();
        }
    }

    #[test]
    fn test_tick_grants_in_order_while_room_lasts() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        enqueue(&mut ledger, 2, 4.0, 300, 0);
        enqueue(&mut ledger, 3, 4.0, 300, 0);

        let transitions = ledger.tick(1_000, &idle(), &alive());
        assert_eq!(granted(&transitions), [1, 2]);
        assert_eq!(ledger.leases()[0].granted_at_ms, Some(1_000));
        assert_eq!(states(&ledger)[2], (3, LeaseState::Queued));
    }

    #[test]
    fn test_tick_background_load_blocks_until_it_drops() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        assert!(granted(&ledger.tick(1_000, &busy(), &alive())).is_empty());
        assert_eq!(granted(&ledger.tick(2_000, &idle(), &alive())), [1]);
    }

    #[test]
    fn test_release_frees_room_for_the_queued_lease() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        enqueue(&mut ledger, 2, 8.0, 300, 0);
        assert_eq!(granted(&ledger.tick(1_000, &idle(), &alive())), [1]);

        let released = ledger.release(LeaseId(1), &session("s"), 5_000).unwrap();
        assert_eq!(released.lease.state, LeaseState::Released);
        assert_eq!(released.lease.ended_at_ms, Some(5_000));
        assert_eq!(released.run, None);
        assert_eq!(granted(&ledger.tick(6_000, &idle(), &alive())), [2]);
        assert_eq!(states(&ledger), [(2, LeaseState::Active)]);
    }

    #[test]
    fn test_short_request_backfills_ahead_of_a_blocked_head() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        // The head fits once lease 1 ends at 300 s; a 60 s job ends before then.
        enqueue(&mut ledger, 2, 10.0, 300, 0);
        enqueue(&mut ledger, 3, 4.0, 60, 0);
        assert_eq!(granted(&ledger.tick(1_000, &idle(), &alive())), [3]);
    }

    #[test]
    fn test_long_request_backfills_only_into_room_the_head_leaves() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        // At 300 s the head takes 10 of 14.4 cores, leaving 4.4 for jobs still running then.
        enqueue(&mut ledger, 2, 10.0, 300, 0);
        enqueue(&mut ledger, 3, 2.0, 600, 0);
        enqueue(&mut ledger, 4, 2.0, 600, 0);
        enqueue(&mut ledger, 5, 2.0, 600, 0);
        assert_eq!(granted(&ledger.tick(1_000, &idle(), &alive())), [3, 4]);

        let verdict = ledger.can_fit(&timed(2.0, 600), LeaseKind::Normal, &idle(), 1_000);
        assert!(!verdict.fits);
        assert!(has_reason(&verdict, "delay lease 2"), "{verdict:?}");
    }

    #[test]
    fn test_large_request_is_not_starved_by_a_stream_of_small_ones() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        enqueue(&mut ledger, 2, 12.0, 300, 0);

        // A long two-core job arrives every 10 s while lease 1 runs.
        for (offset, second) in (10..300).step_by(10).enumerate() {
            let now_ms = second * 1_000;
            enqueue(&mut ledger, 10 + offset as i64, 2.0, 600, now_ms);
            renew_all(&mut ledger, now_ms);
            ledger.tick(now_ms, &idle(), &alive());
        }

        ledger.release(LeaseId(1), &session("s"), 300_000).unwrap();
        renew_all(&mut ledger, 300_000);
        let transitions = ledger.tick(300_000, &idle(), &alive());
        assert_eq!(granted(&transitions).first(), Some(&2));
    }

    #[test]
    fn test_benchmark_waits_for_a_full_quiet_window() {
        let mut ledger = Ledger::new(settings());
        enqueue_benchmark(&mut ledger, 1, 0);
        assert!(tick_seconds(&mut ledger, 0, 0, 50.0).is_empty());
        assert!(tick_seconds(&mut ledger, 1, 6, 0.0).is_empty());

        let waiting = ledger.verdict_for(LeaseId(1), &idle(), 6_000).unwrap();
        assert_eq!(waiting.wait_estimate_secs, Some(4));
        assert!(
            has_reason(&waiting, "measuring background load: 6s of 10s"),
            "{waiting:?}"
        );
        let Some(BenchmarkProgress::Measuring {
            measured_secs: 6,
            needed_secs: 10,
            cpu_avg_pct,
            ..
        }) = waiting.benchmark
        else {
            panic!("{waiting:?}");
        };
        assert!((cpu_avg_pct - 50.0 / 7.0).abs() < 1e-3, "{cpu_avg_pct}");

        assert!(tick_seconds(&mut ledger, 7, 9, 0.0).is_empty());
        renew_all(&mut ledger, 10_000);
        let transitions = ledger.tick(10_000, &sample(10_000, 0.0), &alive());
        assert_eq!(granted(&transitions), [1]);
        assert_eq!(
            transitions[0].reason,
            "granted after 10s of quiet (CPU 0.0%, GPU 0.0%)"
        );
    }

    #[test]
    fn test_benchmark_one_second_spike_does_not_restart_the_quiet_window() {
        let mut ledger = Ledger::new(settings());
        enqueue_benchmark(&mut ledger, 1, 0);
        assert!(tick_seconds(&mut ledger, 0, 4, 3.0).is_empty());
        assert!(tick_seconds(&mut ledger, 5, 5, 50.0).is_empty());
        // The window (1 to 10 s) averages 7.7%, under the 10% limit.
        assert_eq!(tick_seconds(&mut ledger, 6, 10, 3.0), [1]);
    }

    #[test]
    fn test_benchmark_on_a_noisy_machine_is_granted_without_quiet_after_the_longest_wait() {
        let mut ledger = Ledger::new(settings());
        enqueue_benchmark(&mut ledger, 1, 0);
        assert!(tick_seconds(&mut ledger, 0, 30, 15.0).is_empty());

        let noisy = ledger.verdict_for(LeaseId(1), &idle(), 30_000).unwrap();
        assert_eq!(noisy.wait_estimate_secs, Some(30));
        assert!(
            matches!(
                noisy.benchmark,
                Some(BenchmarkProgress::Noisy {
                    grant_anyway_in_secs: 30,
                    ..
                })
            ),
            "{noisy:?}"
        );
        assert!(has_reason(&noisy, "above the quiet limits"), "{noisy:?}");

        assert!(tick_seconds(&mut ledger, 31, 59, 15.0).is_empty());
        renew_all(&mut ledger, 60_000);
        let transitions = ledger.tick(60_000, &sample(60_000, 15.0), &alive());
        assert_eq!(granted(&transitions), [1]);
        assert!(
            transitions[0]
                .reason
                .starts_with("granted without quiet after 60s: CPU 15.0%"),
            "{transitions:?}"
        );

        tick_seconds(&mut ledger, 61, 62, 15.0);
        let released = ledger
            .release(LeaseId(1), &session("bench"), 63_000)
            .unwrap();
        assert!(!released.run.unwrap().noise.quiet_at_start);
    }

    #[test]
    fn test_benchmark_release_reports_noise_and_holder_usage() {
        let mut ledger = Ledger::new(settings());
        enqueue_benchmark(&mut ledger, 1, 0);
        assert_eq!(tick_seconds(&mut ledger, 0, 10, 0.0), [1]);
        for second in 11..=12 {
            let mut busy_sample = sample(second * 1_000, 60.0);
            busy_sample.sessions = vec![session_usage("bench", 8.0), session_usage("other", 0.32)];
            ledger.tick(second * 1_000, &busy_sample, &alive());
        }

        let released = ledger
            .release(LeaseId(1), &session("bench"), 13_000)
            .unwrap();
        let run = released.run.unwrap();
        // 0.32 of 16 cores is 2%; the holder's own 8 cores are not noise.
        assert!((run.noise.avg_cpu_pct - 2.0).abs() < 1e-4, "{run:?}");
        assert!(run.noise.quiet_at_start);
        assert_eq!(run.holder_peak.cores, 8.0);
        assert_eq!(run.holder_avg.cores, 8.0);
    }

    #[test]
    fn test_normal_release_reports_holder_usage() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        assert_eq!(tick_seconds(&mut ledger, 0, 0, 0.0), [1]);
        for (second, cores) in [(1, 4.0), (2, 12.0)] {
            let mut busy_sample = sample(second * 1_000, 50.0);
            busy_sample.sessions = vec![session_usage("s", cores)];
            ledger.tick(second * 1_000, &busy_sample, &alive());
        }

        let run = ledger
            .release(LeaseId(1), &session("s"), 3_000)
            .unwrap()
            .run
            .unwrap();
        assert_eq!(run.holder_peak.cores, 12.0);
        assert_eq!(run.holder_avg.cores, 8.0);
        assert!(!run.overlapped);
    }

    #[test]
    fn test_second_active_lease_of_a_session_marks_both_runs_overlapped() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 2.0, 300, 0);
        let other = queued_lease(2, "other", timed(2.0, 300), LeaseKind::Normal, 0);
        ledger.enqueue(other).unwrap();
        assert_eq!(tick_seconds(&mut ledger, 0, 1, 0.0), [1, 2]);
        enqueue(&mut ledger, 3, 2.0, 300, 2_000);
        assert_eq!(tick_seconds(&mut ledger, 2, 3, 0.0), [3]);

        // Lease 3 ended before lease 1, but lease 1 still shared the session with it.
        let third = ledger.release(LeaseId(3), &session("s"), 4_000).unwrap();
        tick_seconds(&mut ledger, 4, 5, 0.0);
        let first = ledger.release(LeaseId(1), &session("s"), 6_000).unwrap();
        let alone = ledger
            .release(LeaseId(2), &session("other"), 6_000)
            .unwrap();
        assert!(third.run.unwrap().overlapped);
        assert!(first.run.unwrap().overlapped);
        assert!(!alone.run.unwrap().overlapped);
    }

    #[test]
    fn test_expired_lease_drops_its_meter() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 2.0, 300, 0);
        assert_eq!(tick_seconds(&mut ledger, 0, 1, 0.0), [1]);
        let dead = Dead(HashSet::from([holder("s").pid]));
        ledger.tick(2_000, &sample(2_000, 0.0), &dead);
        assert!(ledger.meters.is_empty());
    }

    #[test]
    fn test_ceiling_is_the_room_with_every_active_lease_ended() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        // 16 cores less 10% headroom; lease 1's reservation does not count.
        let ceiling = ledger.ceiling(&idle());
        assert!((ceiling.cores - 14.4).abs() < 1e-6, "{ceiling:?}");
        let busy_ceiling = ledger.ceiling(&busy());
        assert!((busy_ceiling.cores - 6.4).abs() < 1e-6, "{busy_ceiling:?}");
    }

    #[test]
    fn test_benchmark_drains_the_machine_and_then_runs_alone() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 4.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        enqueue_benchmark(&mut ledger, 2, 0);
        enqueue(&mut ledger, 3, 1.0, 10, 0);

        assert!(granted(&ledger.tick(20_000, &idle(), &alive())).is_empty());
        let waiting = ledger.can_fit(&profile(1.0, 1_000), LeaseKind::Normal, &idle(), 20_000);
        assert!(
            has_reason(&waiting, "waiting for the machine to drain"),
            "{waiting:?}"
        );
        let draining = ledger.verdict_for(LeaseId(2), &idle(), 20_000).unwrap();
        assert_eq!(
            draining.benchmark,
            Some(BenchmarkProgress::Draining { active_leases: 1 })
        );

        ledger.release(LeaseId(1), &session("s"), 30_000).unwrap();
        // The quiet window starts at the drain, not at the earlier idle ticks.
        assert!(tick_seconds(&mut ledger, 30, 39, 1.0).is_empty());
        assert_eq!(tick_seconds(&mut ledger, 40, 40, 1.0), [2]);

        let held = ledger.can_fit(&profile(1.0, 1_000), LeaseKind::Normal, &idle(), 41_000);
        assert!(!held.fits);
        assert!(
            has_reason(&held, "benchmark lease 2 holds the machine"),
            "{held:?}"
        );
        assert_eq!(states(&ledger)[1], (3, LeaseState::Queued));
    }

    #[test]
    fn test_benchmark_wait_estimate_is_drain_plus_quiet() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 4.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        let verdict = can_fit_benchmark(&ledger, 100_000);
        assert!(!verdict.fits);
        assert_eq!(verdict.wait_estimate_secs, Some(210));
    }

    #[test]
    fn test_can_fit_reports_shortfall_queue_and_wait() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        enqueue(&mut ledger, 2, 12.0, 300, 0);

        let verdict = ledger.can_fit(&profile(8.0, 1_000), LeaseKind::Normal, &idle(), 100_000);
        assert!(!verdict.fits);
        assert!(has_reason(&verdict, "cores:"), "{verdict:?}");
        assert!(
            has_reason(&verdict, "1 lease(s) queued ahead"),
            "{verdict:?}"
        );
        assert_eq!(verdict.wait_estimate_secs, Some(200));

        let small = ledger.can_fit(&timed(1.0, 30), LeaseKind::Normal, &idle(), 100_000);
        assert_eq!((small.fits, small.wait_estimate_secs), (true, None));
    }

    #[test]
    fn test_can_fit_never_fitting_request_has_no_estimate() {
        let ledger = Ledger::new(settings());
        let verdict = ledger.can_fit(&profile(64.0, 1_000), LeaseKind::Normal, &idle(), 0);
        assert!(!verdict.fits);
        assert!(
            has_reason(&verdict, "more than the machine can free"),
            "{verdict:?}"
        );
        assert_eq!(verdict.wait_estimate_secs, None);
    }

    #[test]
    fn test_missing_heartbeat_expires_queued_and_active_leases() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        enqueue(&mut ledger, 2, 12.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        assert!(ledger.tick(TTL_MS - 1, &idle(), &alive()).is_empty());

        let transitions = ledger.tick(TTL_MS, &idle(), &alive());
        let expired = [1, 2].map(|id| Transition {
            id: LeaseId(id),
            to: LeaseState::Expired,
            reason: "no heartbeat for 60s".into(),
        });
        assert_eq!(transitions, expired);
        assert!(ledger.leases().is_empty());
    }

    #[test]
    fn test_heartbeat_extends_the_deadline() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        let lease = ledger.heartbeat(LeaseId(1), &session("s"), 30_000).unwrap();
        assert_eq!(lease.expires_at_ms, 30_000 + TTL_MS);
        assert_eq!(granted(&ledger.tick(TTL_MS, &idle(), &alive())), [1]);
        let transitions = ledger.tick(30_000 + TTL_MS, &idle(), &alive());
        assert_eq!(transitions[0].to, LeaseState::Expired);
    }

    #[test]
    fn test_dead_holder_is_reaped_and_frees_room() {
        let mut ledger = Ledger::new(settings());
        let doomed = queued_lease(1, "doomed", profile(8.0, 1_000), LeaseKind::Normal, 0);
        let waiting = queued_lease(2, "alive", profile(8.0, 1_000), LeaseKind::Normal, 0);
        ledger.enqueue(doomed).unwrap();
        ledger.enqueue(waiting).unwrap();
        assert_eq!(granted(&ledger.tick(0, &idle(), &alive())), [1]);

        let dead = Dead(HashSet::from([holder("doomed").pid]));
        let transitions = ledger.tick(1_000, &idle(), &dead);
        let reaped = Transition {
            id: LeaseId(1),
            to: LeaseState::Expired,
            reason: "holder process exited".into(),
        };
        assert_eq!(transitions[0], reaped);
        assert_eq!(granted(&transitions), [2]);
    }

    #[test]
    fn test_ledger_refuses_bad_requests() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 1.0, 300, 0);

        let duplicate = queued_lease(1, "s", profile(1.0, 1), LeaseKind::Normal, 0);
        let error = ledger.enqueue(duplicate);
        assert_eq!(error, Err(LedgerError::AlreadyTracked(LeaseId(1))));
        let mut active = queued_lease(2, "s", profile(1.0, 1), LeaseKind::Normal, 0);
        active.state = LeaseState::Active;
        assert_eq!(
            ledger.enqueue(active),
            Err(LedgerError::NotQueued(LeaseId(2)))
        );

        let error = ledger.release(LeaseId(1), &session("other"), 0);
        assert_eq!(error, Err(LedgerError::NotOwner(LeaseId(1))));
        let error = ledger
            .heartbeat(LeaseId(9), &session("s"), 0)
            .map(|lease| lease.id);
        assert_eq!(error, Err(LedgerError::NotFound(LeaseId(9))));
        assert_eq!(
            LedgerError::NotOwner(LeaseId(1)).to_string(),
            "lease 1 belongs to another session"
        );
    }

    #[test]
    fn test_verdict_for_explains_each_tracked_lease() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 8.0, 300, 0);
        ledger.tick(0, &idle(), &alive());
        enqueue(&mut ledger, 2, 12.0, 300, 0);
        enqueue(&mut ledger, 3, 12.0, 300, 0);

        let running = ledger.verdict_for(LeaseId(1), &idle(), 1_000).unwrap();
        assert!(running.fits && running.reasons.is_empty());
        let head = ledger.verdict_for(LeaseId(2), &idle(), 1_000).unwrap();
        assert!(!head.fits);
        assert!(has_reason(&head, "cores:"), "{head:?}");
        assert_eq!(head.wait_estimate_secs, Some(299));
        let second = ledger.verdict_for(LeaseId(3), &idle(), 1_000).unwrap();
        assert!(has_reason(&second, "1 lease(s) queued ahead"), "{second:?}");
        assert_eq!(ledger.verdict_for(LeaseId(9), &idle(), 1_000), None);
    }

    #[test]
    fn test_restore_keeps_state_and_order_and_gives_a_fresh_deadline() {
        let mut ledger = Ledger::new(settings());
        let mut active = queued_lease(1, "s", timed(8.0, 300), LeaseKind::Normal, 0);
        active.state = LeaseState::Active;
        active.granted_at_ms = Some(1_000);
        active.expires_at_ms = 5_000;
        let mut late = queued_lease(2, "s", timed(12.0, 300), LeaseKind::Normal, 0);
        late.expires_at_ms = 500_000;

        let restored = ledger.restore(active, 200_000).unwrap();
        assert_eq!(restored.expires_at_ms, 200_000 + TTL_MS);
        let restored = ledger.restore(late, 200_000).unwrap();
        assert_eq!(restored.expires_at_ms, 500_000);
        assert_eq!(
            states(&ledger),
            [(1, LeaseState::Active), (2, LeaseState::Queued)]
        );
        // The restored active lease still holds its cores, so lease 2 waits for it.
        assert!(granted(&ledger.tick(201_000, &idle(), &alive())).is_empty());
    }

    #[test]
    fn test_restore_keeps_the_lease_binding() {
        let mut ledger = Ledger::new(settings());
        let mut active = queued_lease(1, "s", timed(2.0, 300), LeaseKind::Normal, 0);
        active.state = LeaseState::Active;
        active.binding = Some(LeaseBinding {
            tool_use_id: "toolu_01AbC".into(),
            bound_at_ms: 900,
            root: Some(BoundProcess {
                pid: 30_112,
                start_time: 1,
            }),
        });
        let binding = active.binding.clone();
        assert_eq!(ledger.restore(active, 10_000).unwrap().binding, binding);
        assert_eq!(ledger.leases()[0].binding, binding);
    }

    #[test]
    fn test_restored_active_lease_is_released_without_a_run() {
        let mut ledger = Ledger::new(settings());
        let mut active = queued_lease(1, "s", timed(2.0, 300), LeaseKind::Normal, 0);
        active.state = LeaseState::Active;
        active.granted_at_ms = Some(0);
        ledger.restore(active, 10_000).unwrap();
        tick_seconds(&mut ledger, 11, 12, 0.0);

        let released = ledger.release(LeaseId(1), &session("s"), 13_000).unwrap();
        assert_eq!(released.lease.state, LeaseState::Released);
        assert_eq!(released.run, None);
    }

    #[test]
    fn test_restored_lease_of_a_dead_holder_is_reaped_on_the_first_tick() {
        let mut ledger = Ledger::new(settings());
        let mut active = queued_lease(1, "gone", timed(2.0, 300), LeaseKind::Normal, 0);
        active.state = LeaseState::Active;
        ledger.restore(active, 10_000).unwrap();

        let dead = Dead(HashSet::from([holder("gone").pid]));
        let transitions = ledger.tick(11_000, &idle(), &dead);
        assert_eq!(transitions[0].reason, "holder process exited");
        assert!(ledger.leases().is_empty());
    }

    #[test]
    fn test_restore_refuses_ended_and_tracked_leases() {
        let mut ledger = Ledger::new(settings());
        enqueue(&mut ledger, 1, 1.0, 300, 0);
        let duplicate = queued_lease(1, "s", profile(1.0, 1), LeaseKind::Normal, 0);
        assert_eq!(
            ledger.restore(duplicate, 0).map(|lease| lease.id),
            Err(LedgerError::AlreadyTracked(LeaseId(1)))
        );
        let mut ended = queued_lease(2, "s", profile(1.0, 1), LeaseKind::Normal, 0);
        ended.state = LeaseState::Expired;
        assert_eq!(
            ledger.restore(ended, 0).map(|lease| lease.id),
            Err(LedgerError::Ended(LeaseId(2)))
        );
    }

    #[test]
    fn test_settings_from_config_converts_seconds() {
        let settings = LedgerSettings::from_config(&Config::default());
        assert_eq!(settings.heartbeat_ttl_ms, 60_000);
        assert_eq!(settings.quiet.window_ms, 10_000);
        assert_eq!(settings.quiet.max_wait_ms, 60_000);
        assert_eq!(settings.headroom_pct, 15.0);
    }
}
