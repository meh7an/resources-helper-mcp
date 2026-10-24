use std::{
    future::Future,
    sync::{Mutex, MutexGuard, PoisonError},
};

use rh_client::{ClientError, Result};
use rh_shared::{
    api::{
        AcquireRequest, AcquireResponse, CanFitRequest, HeartbeatResponse, LeaseQuery, ProfileSpec,
        ReleaseResponse, Verdict,
    },
    types::{
        CpuStats, GpuAdapter, GpuEngines, Holder, JobClass, JobProfile, Lease, LeaseId, LeaseKind,
        LeaseState, MemStats, NoiseReport, ProcessUsage, ResourceUsage, Rolling, SessionKey,
        SessionUsage, Snapshot,
    },
};

use crate::daemon::DaemonApi;

pub(crate) fn holder(session: &str) -> Holder {
    Holder {
        session: SessionKey(session.into()),
        pid: 60_012,
        pid_start_time: 1,
    }
}

fn steady(value: f32) -> Rolling {
    Rolling {
        avg_1s: value,
        avg_10s: value,
        avg_60s: value,
    }
}

/// A 16-core machine with one GPU and two sessions, `me` and `them`.
pub(crate) fn snapshot() -> Snapshot {
    let session = |name: &str, cores: f32| SessionUsage {
        holder: holder(name),
        usage: ResourceUsage {
            cores,
            ..ResourceUsage::default()
        },
        processes: vec![ProcessUsage {
            pid: 7,
            name: "cargo.exe".into(),
            exe: None,
            usage: ResourceUsage::default(),
        }],
    };
    Snapshot {
        taken_at_ms: 0,
        cpu: CpuStats {
            logical_cores: 16,
            usage_pct: steady(25.0),
            per_core_pct: vec![25.0; 16],
        },
        memory: MemStats {
            total_mb: 32_768,
            used_mb: 8_000,
            available_mb: 24_768,
            commit_used_mb: 9_000,
            commit_limit_mb: 65_536,
        },
        gpus: vec![GpuAdapter {
            name: "Test GPU".into(),
            luid: "0x00000000_0x00000001".into(),
            engines: GpuEngines {
                three_d_pct: steady(40.0),
                ..GpuEngines::default()
            },
            dedicated_used_mb: 2_048,
            dedicated_total_mb: Some(12_288),
            shared_used_mb: 0,
        }],
        sessions: vec![session("me", 2.0), session("them", 3.0)],
        unattributed: ResourceUsage::default(),
        drives: Vec::new(),
    }
}

/// An in-memory daemon. Every request is granted at once, except `gpu_heavy` ones, which
/// stay queued.
#[derive(Debug, Default)]
pub(crate) struct MockDaemon {
    unreachable: bool,
    interrupted: bool,
    state: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    leases: Vec<Lease>,
    next_id: i64,
    heartbeats: Vec<LeaseId>,
    releases: Vec<LeaseId>,
}

impl MockDaemon {
    /// A daemon every call fails to reach.
    pub(crate) fn unreachable() -> Self {
        Self {
            unreachable: true,
            ..Self::default()
        }
    }

    /// A daemon that receives every call but never answers, as when it crashes mid-request.
    pub(crate) fn interrupted() -> Self {
        Self {
            interrupted: true,
            ..Self::default()
        }
    }

    pub(crate) fn add_lease(&self, lease: Lease) {
        self.lock().leases.push(lease);
    }

    pub(crate) fn heartbeats(&self) -> Vec<LeaseId> {
        self.lock().heartbeats.clone()
    }

    pub(crate) fn releases(&self) -> Vec<LeaseId> {
        self.lock().releases.clone()
    }

    pub(crate) fn leases_count(&self) -> usize {
        self.lock().leases.len()
    }

    /// The working directory of every lease, in the order they were acquired.
    pub(crate) fn lease_cwds(&self) -> Vec<String> {
        self.lock()
            .leases
            .iter()
            .map(|lease| lease.cwd.clone())
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reply<T: Send>(&self, make: impl FnOnce(&mut MockState) -> Result<T>) -> Result<T> {
        if self.unreachable {
            return Err(ClientError::Unreachable {
                url: "http://127.0.0.1:47823".into(),
                reason: "connection refused".into(),
            });
        }
        if self.interrupted {
            return Err(ClientError::Interrupted {
                url: "http://127.0.0.1:47823".into(),
                reason: "connection closed before message completed".into(),
            });
        }
        make(&mut self.lock())
    }
}

fn verdict(fits: bool) -> Verdict {
    Verdict {
        fits,
        reasons: vec![],
        wait_estimate_secs: None,
        benchmark: None,
    }
}

fn not_found(id: LeaseId) -> ClientError {
    ClientError::Http {
        status: 404,
        message: format!("lease {} is not tracked", id.0),
    }
}

/// A lease for `holder`, numbered `id`.
pub(crate) fn lease(id: i64, holder: Holder, state: LeaseState) -> Lease {
    Lease {
        id: LeaseId(id),
        holder,
        kind: LeaseKind::Normal,
        state,
        class: Some(JobClass::CpuHeavy),
        profile: JobProfile {
            cores: 8.0,
            ram_mb: 4_096,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: 300,
        },
        label: "cargo build".into(),
        cwd: String::new(),
        requested_at_ms: 0,
        granted_at_ms: None,
        ended_at_ms: None,
        expires_at_ms: 60_000,
        end_cause: None,
        binding: None,
    }
}

impl DaemonApi for MockDaemon {
    fn snapshot(&self) -> impl Future<Output = Result<Snapshot>> + Send {
        let result = self.reply(|_| Ok(snapshot()));
        async move { result }
    }

    fn can_fit(&self, request: &CanFitRequest) -> impl Future<Output = Result<Verdict>> + Send {
        let fits = request.profile != ProfileSpec::Class(JobClass::GpuHeavy);
        // Echoes the history inputs so tests can see what the tool sent.
        let history = request.label.as_ref().map(|label| {
            format!(
                "history of {label} in {}",
                request.cwd.as_deref().unwrap_or("?")
            )
        });
        let result = self.reply(|_| {
            Ok(Verdict {
                fits,
                reasons: if fits {
                    history.into_iter().collect()
                } else {
                    vec!["GPU: busy".into()]
                },
                wait_estimate_secs: (!fits).then_some(30),
                benchmark: None,
            })
        });
        async move { result }
    }

    fn acquire(
        &self,
        request: &AcquireRequest,
    ) -> impl Future<Output = Result<AcquireResponse>> + Send {
        let request = request.clone();
        let result = self.reply(|state| {
            state.next_id += 1;
            let queued = request.profile == ProfileSpec::Class(JobClass::GpuHeavy);
            let mut lease = lease(
                state.next_id,
                request.holder,
                if queued {
                    LeaseState::Queued
                } else {
                    LeaseState::Active
                },
            );
            lease.kind = request.kind;
            lease.label = request.label;
            lease.cwd = request.cwd.unwrap_or_default();
            state.leases.push(lease.clone());
            Ok(AcquireResponse {
                lease,
                verdict: verdict(!queued),
            })
        });
        async move { result }
    }

    /// Grants the queued lease at once.
    fn wait(
        &self,
        id: LeaseId,
        _session: &SessionKey,
        _wait_timeout_secs: u64,
    ) -> impl Future<Output = Result<AcquireResponse>> + Send {
        let result = self.reply(|state| {
            let lease = state.leases.iter_mut().find(|lease| lease.id == id);
            let lease = lease.ok_or_else(|| not_found(id))?;
            if lease.state == LeaseState::Queued {
                lease.state = LeaseState::Active;
            }
            Ok(AcquireResponse {
                lease: lease.clone(),
                verdict: verdict(true),
            })
        });
        async move { result }
    }

    fn heartbeat(
        &self,
        id: LeaseId,
        _session: &SessionKey,
    ) -> impl Future<Output = Result<HeartbeatResponse>> + Send {
        let result = self.reply(|state| {
            state.heartbeats.push(id);
            let lease = state.leases.iter().find(|lease| lease.id == id);
            let lease = lease.ok_or_else(|| not_found(id))?;
            Ok(HeartbeatResponse {
                state: lease.state,
                expires_at_ms: lease.expires_at_ms,
            })
        });
        async move { result }
    }

    /// Benchmark leases come back with a small noise report.
    fn release(
        &self,
        id: LeaseId,
        _session: &SessionKey,
    ) -> impl Future<Output = Result<ReleaseResponse>> + Send {
        let result = self.reply(|state| {
            state.releases.push(id);
            let lease = state.leases.iter_mut().find(|lease| lease.id == id);
            let lease = lease.ok_or_else(|| not_found(id))?;
            lease.state = LeaseState::Released;
            let noise = (lease.kind == LeaseKind::BenchmarkExclusive).then_some(NoiseReport {
                avg_cpu_pct: 1.5,
                peak_cpu_pct: 4.0,
                avg_gpu_pct: 0.0,
                peak_gpu_pct: 0.0,
                quiet_at_start: true,
            });
            Ok(ReleaseResponse {
                lease: lease.clone(),
                noise,
            })
        });
        async move { result }
    }

    fn leases(&self, query: &LeaseQuery) -> impl Future<Output = Result<Vec<Lease>>> + Send {
        let result = self.reply(|state| {
            let matching = state.leases.iter().filter(|lease| {
                query
                    .session
                    .as_ref()
                    .is_none_or(|s| &lease.holder.session == s)
                    && query.state.is_none_or(|s| lease.state == s)
            });
            Ok(matching.cloned().collect())
        });
        async move { result }
    }

    fn register_session(&self, _holder: &Holder) -> impl Future<Output = Result<()>> + Send {
        let result = self.reply(|_| Ok(()));
        async move { result }
    }
}
