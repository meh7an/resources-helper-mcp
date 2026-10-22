use serde::{Deserialize, Serialize};

use crate::types::{
    Holder, JobClass, JobProfile, JobRun, Lease, LeaseKind, LeaseState, NoiseReport, SessionKey,
    SessionUsage,
};

/// Response of `GET /health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Daemon version, from its `Cargo.toml`.
    pub version: String,
    /// Daemon process ID.
    pub pid: u32,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Why the last change to `config.toml` was rejected; the daemon keeps the config it had
    /// until the file is fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
}

/// How a request states the resources a job needs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProfileSpec {
    /// A named class, resolved through the daemon's config.
    Class(JobClass),
    /// Explicit values.
    Custom(JobProfile),
}

/// Body of `POST /can-fit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanFitRequest {
    /// Resources the job needs.
    pub profile: ProfileSpec,
    /// How the job would share the machine.
    pub kind: LeaseKind,
    /// The command, so the daemon can raise the profile from similar past runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Working directory of the command; part of what makes runs similar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The admission engine's answer. Response of `POST /can-fit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether the job could start now.
    pub fits: bool,
    /// Human-readable explanations, one per limiting or notable factor.
    pub reasons: Vec<String>,
    /// Expected wait before the job fits, when the daemon can estimate it.
    pub wait_estimate_secs: Option<u64>,
    /// Where a benchmark at the head of the queue stands; `None` for other requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark: Option<BenchmarkProgress>,
}

/// Where a waiting benchmark lease stands, so its holder can report progress.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BenchmarkProgress {
    /// Other leases are still active; nothing new starts until they end.
    Draining {
        /// Active leases left.
        active_leases: u32,
    },
    /// The machine has drained and its background load is being averaged.
    Measuring {
        /// Seconds averaged so far.
        measured_secs: u64,
        /// Seconds the average must cover.
        needed_secs: u64,
        /// Average total CPU load so far, in percent.
        cpu_avg_pct: f32,
        /// Average load of the busiest GPU engine so far, in percent.
        gpu_avg_pct: f32,
    },
    /// The average is above the quiet limits. The lease is granted anyway, marked as not
    /// quiet, once `grant_anyway_in_secs` have passed.
    Noisy {
        /// Average total CPU load over the quiet window, in percent.
        cpu_avg_pct: f32,
        /// Average load of the busiest GPU engine over the quiet window, in percent.
        gpu_avg_pct: f32,
        /// CPU limit of a quiet machine.
        cpu_limit_pct: f32,
        /// GPU limit of a quiet machine.
        gpu_limit_pct: f32,
        /// Seconds until the lease is granted without quiet.
        grant_anyway_in_secs: u64,
    },
}

/// Body of `POST /leases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcquireRequest {
    /// Session asking for the lease.
    pub holder: Holder,
    /// Resources to reserve.
    pub profile: ProfileSpec,
    /// How the lease shares the machine.
    pub kind: LeaseKind,
    /// What the lease is for, usually the command that will run.
    pub label: String,
    /// Working directory of the command; part of what makes runs similar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// How long the daemon may hold the request open waiting for the lease to become active.
    pub wait_timeout_secs: u64,
}

/// Response of `POST /leases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcquireResponse {
    /// The lease, `Active` if it was granted within the wait timeout and `Queued` otherwise.
    pub lease: Lease,
    /// Why the lease was granted or is still waiting.
    pub verdict: Verdict,
}

/// Body of `POST /leases/{id}/wait`: keep waiting for a queued lease without losing its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitRequest {
    /// How long the daemon may hold the request open waiting for the lease to become active.
    pub wait_timeout_secs: u64,
}

/// Query string of `POST /leases/{id}/heartbeat`, `POST /leases/{id}/wait`, and
/// `DELETE /leases/{id}`.
///
/// The daemon refuses either call when the lease belongs to another session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseOwner {
    /// Session that claims to own the lease.
    pub session: SessionKey,
}

/// Response of `POST /leases/{id}/heartbeat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Current state, so a holder of a queued lease learns when it becomes active.
    pub state: LeaseState,
    /// New expiry, in milliseconds since the Unix epoch.
    pub expires_at_ms: i64,
}

/// Response of `DELETE /leases/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseResponse {
    /// The lease in its final state.
    pub lease: Lease,
    /// Background load while a benchmark lease was active. Only the release that ends the
    /// lease carries it, and only when at least one sample was taken during the lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise: Option<NoiseReport>,
}

/// Query string of `GET /leases`. Each filter is optional; an empty query lists every lease.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LeaseQuery {
    /// Only leases held by this session.
    pub session: Option<SessionKey>,
    /// Only leases in this state.
    pub state: Option<LeaseState>,
}

/// Response of `GET /leases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeasesResponse {
    /// Matching leases, oldest first.
    pub leases: Vec<Lease>,
}

/// Response of `GET /sessions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionsResponse {
    /// Every registered session with its current usage.
    pub sessions: Vec<SessionUsage>,
}

/// Body of `POST /sessions`. Registering the same session again is harmless; the daemon
/// forgets a session once its holder process is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterSessionRequest {
    /// The session and its `claude.exe`.
    pub holder: Holder,
}

/// Response of `GET /history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryResponse {
    /// One entry per fingerprint, most recently run first.
    pub jobs: Vec<JobSummary>,
}

/// What the history knows about one fingerprint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    /// Normalized command plus working directory.
    pub fingerprint: String,
    /// Label of the most recent run.
    pub command: String,
    /// Working directory of the most recent run.
    pub cwd: String,
    /// Stored runs.
    pub runs: u32,
    /// Runs the estimate is based on: the most recent ones without an overlapping lease.
    pub estimate_runs: u32,
    /// End of the most recent run, in milliseconds since the Unix epoch.
    pub last_ended_at_ms: i64,
    /// What the runs needed: p90 of peak memory, of average CPU and GPU load, and of run
    /// time. `None` when every run overlapped another lease.
    pub learned: Option<JobProfile>,
}

/// Query string of `GET /history/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRunsQuery {
    /// Fingerprint whose runs to list.
    pub fingerprint: String,
    /// Most runs to return; the daemon's default when absent.
    pub limit: Option<u32>,
}

/// Response of `GET /history/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRunsResponse {
    /// Runs with the fingerprint, newest first.
    pub runs: Vec<JobRun>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        assert_json_round_trip, sample_holder, sample_lease, sample_session_usage,
    };

    fn sample_verdict() -> Verdict {
        Verdict {
            fits: false,
            reasons: vec!["RAM: 6144 MB requested, 4096 MB free after headroom".into()],
            wait_estimate_secs: Some(90),
            benchmark: None,
        }
    }

    fn custom_profile() -> ProfileSpec {
        ProfileSpec::Custom(JobProfile {
            cores: 3.5,
            ram_mb: 6144,
            gpu_pct: 40.0,
            vram_mb: 2048,
            est_secs: 120,
        })
    }

    #[test]
    fn test_health_response_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&HealthResponse {
            version: "0.1.0".into(),
            pid: 4242,
            uptime_secs: 3600,
            config_error: Some("config.toml: invalid configuration: x".into()),
        });
    }

    #[test]
    fn test_can_fit_request_json_round_trip_preserves_both_profile_forms() {
        assert_json_round_trip(&CanFitRequest {
            profile: ProfileSpec::Class(JobClass::GpuHeavy),
            kind: LeaseKind::Normal,
            label: None,
            cwd: None,
        });
        assert_json_round_trip(&CanFitRequest {
            profile: custom_profile(),
            kind: LeaseKind::BenchmarkExclusive,
            label: Some("cargo bench".into()),
            cwd: Some(r"E:\work\app".into()),
        });
    }

    #[test]
    fn test_can_fit_request_without_label_and_cwd_deserializes_as_none() {
        let request: CanFitRequest =
            serde_json::from_str(r#"{"profile":{"class":"light"},"kind":"normal"}"#).unwrap();
        assert_eq!((request.label, request.cwd), (None, None));
    }

    #[test]
    fn test_profile_spec_json_is_externally_tagged() {
        let class = serde_json::to_value(ProfileSpec::Class(JobClass::CpuHeavy)).unwrap();
        assert_eq!(class, serde_json::json!({ "class": "cpu_heavy" }));
        let custom = serde_json::to_value(custom_profile()).unwrap();
        assert_eq!(custom["custom"]["ram_mb"], 6144);
    }

    #[test]
    fn test_verdict_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&sample_verdict());
        assert_json_round_trip(&Verdict {
            fits: true,
            reasons: vec![],
            wait_estimate_secs: None,
            benchmark: None,
        });
    }

    #[test]
    fn test_verdict_benchmark_progress_round_trips_every_phase() {
        let phases = [
            BenchmarkProgress::Draining { active_leases: 2 },
            BenchmarkProgress::Measuring {
                measured_secs: 4,
                needed_secs: 10,
                cpu_avg_pct: 6.5,
                gpu_avg_pct: 0.0,
            },
            BenchmarkProgress::Noisy {
                cpu_avg_pct: 12.5,
                gpu_avg_pct: 1.0,
                cpu_limit_pct: 10.0,
                gpu_limit_pct: 10.0,
                grant_anyway_in_secs: 35,
            },
        ];
        for phase in phases {
            assert_json_round_trip(&Verdict {
                benchmark: Some(phase),
                ..sample_verdict()
            });
        }
        let value = serde_json::to_value(phases[0]).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "phase": "draining", "active_leases": 2 })
        );
    }

    #[test]
    fn test_verdict_without_benchmark_field_deserializes_as_none() {
        let verdict: Verdict =
            serde_json::from_str(r#"{"fits":true,"reasons":[],"wait_estimate_secs":null}"#)
                .unwrap();
        assert_eq!(verdict.benchmark, None);
    }

    #[test]
    fn test_acquire_request_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&AcquireRequest {
            holder: sample_holder(),
            profile: custom_profile(),
            kind: LeaseKind::Normal,
            label: "cargo build --release".into(),
            cwd: Some(r"E:\work\app".into()),
            wait_timeout_secs: 30,
        });
    }

    #[test]
    fn test_acquire_response_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&AcquireResponse {
            lease: sample_lease(),
            verdict: sample_verdict(),
        });
    }

    #[test]
    fn test_lease_owner_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&LeaseOwner {
            session: sample_holder().session,
        });
    }

    #[test]
    fn test_heartbeat_response_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&HeartbeatResponse {
            state: LeaseState::Queued,
            expires_at_ms: 1_793_000_060_000,
        });
    }

    #[test]
    fn test_release_response_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&ReleaseResponse {
            lease: sample_lease(),
            noise: None,
        });
        assert_json_round_trip(&ReleaseResponse {
            lease: sample_lease(),
            noise: Some(NoiseReport {
                avg_cpu_pct: 1.5,
                peak_cpu_pct: 4.0,
                avg_gpu_pct: 0.0,
                peak_gpu_pct: 2.0,
                quiet_at_start: true,
            }),
        });
    }

    #[test]
    fn test_wait_request_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&WaitRequest {
            wait_timeout_secs: 90,
        });
    }

    #[test]
    fn test_lease_query_json_round_trip_preserves_empty_and_full_filters() {
        assert_json_round_trip(&LeaseQuery::default());
        assert_json_round_trip(&LeaseQuery {
            session: Some(sample_holder().session),
            state: Some(LeaseState::Active),
        });
    }

    #[test]
    fn test_lease_query_missing_fields_deserialize_as_none() {
        let query: LeaseQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(query, LeaseQuery::default());
    }

    #[test]
    fn test_leases_response_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&LeasesResponse {
            leases: vec![sample_lease()],
        });
    }

    #[test]
    fn test_sessions_response_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&SessionsResponse {
            sessions: vec![sample_session_usage()],
        });
    }

    #[test]
    fn test_history_dtos_json_round_trip_preserve_every_field() {
        let summary = JobSummary {
            fingerprint: r"cargo build|e:\work\app".into(),
            command: "cargo build".into(),
            cwd: r"E:\work\app".into(),
            runs: 5,
            estimate_runs: 4,
            last_ended_at_ms: 1_793_000_095_000,
            learned: match custom_profile() {
                ProfileSpec::Custom(profile) => Some(profile),
                _ => None,
            },
        };
        assert_json_round_trip(&HistoryResponse {
            jobs: vec![
                summary.clone(),
                JobSummary {
                    learned: None,
                    ..summary
                },
            ],
        });
        assert_json_round_trip(&HistoryRunsQuery {
            fingerprint: "cargo build|".into(),
            limit: Some(10),
        });
        assert_json_round_trip(&HistoryRunsResponse { runs: Vec::new() });
    }

    #[test]
    fn test_register_session_request_json_round_trip_preserves_every_field() {
        assert_json_round_trip(&RegisterSessionRequest {
            holder: sample_holder(),
        });
    }
}
