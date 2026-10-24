//! The tray flyout's view of the daemon, served whole by `GET /ui/state`.
//!
//! These types are the contract with the C# tray app: its types are generated from the JSON
//! Schema of [`UiState`], and the golden fixtures in `contract/` pin the wire shape.

use serde::{Deserialize, Serialize};

use crate::{
    api::{HealthResponse, Verdict},
    types::{
        DriveUsage, EndCause, JobProfile, JobRun, Lease, LeaseId, NoiseReport, ResourceUsage,
        SessionKey,
    },
};

/// Response of `GET /ui/state`: everything the flyout shows, in one poll.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    /// When the state was built, in milliseconds since the Unix epoch. Elapsed and waited
    /// times are measured against it, so they do not depend on the tray app's clock.
    pub generated_at_ms: i64,
    /// The daemon itself.
    pub service: HealthResponse,
    /// When a Claude session first registered with this install, or `None` when none ever
    /// has, which shows the first-run card.
    pub first_session_at_ms: Option<i64>,
    /// Every registered session, with or without leases.
    pub sessions: Vec<UiSession>,
    /// Active leases, oldest grant first.
    pub active: Vec<ActiveJob>,
    /// Queued leases, in queue order.
    pub queue: Vec<QueuedJob>,
    /// Machine meters for the Resources card.
    pub meters: Meters,
    /// Recently finished jobs, newest first.
    pub recent: Vec<RecentRun>,
    /// Kills and removals, with whether Claude has been told yet.
    pub notices: Vec<Notice>,
}

/// One registered Claude session.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiSession {
    /// Session ID.
    pub session: SessionKey,
    /// Working directory of the session's most recent lease, which names its project; `None`
    /// when no lease gave one.
    pub cwd: Option<String>,
    /// Live usage of the session's whole process subtree.
    pub usage: ResourceUsage,
}

/// An active lease and the process its command runs as.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveJob {
    /// The lease.
    pub lease: Lease,
    /// The command's process, or `None` when the session has no process to show.
    pub process: Option<JobProcess>,
}

/// The process shown for a job.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProcess {
    /// Process ID.
    pub pid: u32,
    /// Executable name, for example `cargo.exe`.
    pub name: String,
    /// Full path of the executable, for its icon; `None` when it could not be read.
    pub exe: Option<String>,
    /// Whether it belongs to the command's bound process tree, the only tree a kill ends: its
    /// busiest process, or its root while the tree is idle. `false` means it is the session's
    /// busiest process, shown for display only.
    pub bound: bool,
}

/// A queued lease and why it waits.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedJob {
    /// The lease.
    pub lease: Lease,
    /// Place in the queue, starting at 1.
    pub position: u32,
    /// Why it waits, the expected wait, and benchmark progress when it is a benchmark.
    pub verdict: Verdict,
}

/// Machine meters with the share that leases reserve.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meters {
    /// Percentage of each resource that leases never take, from `admission.headroom_pct`.
    pub headroom_pct: f32,
    /// Processor load.
    pub cpu: CpuMeter,
    /// Physical memory.
    pub ram: RamMeter,
    /// Every GPU adapter.
    pub gpus: Vec<GpuMeter>,
    /// Every fixed drive, by drive letter.
    pub drives: Vec<DriveUsage>,
}

/// Processor load.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuMeter {
    /// Number of logical processors.
    pub logical_cores: u32,
    /// Total load over the last second, as a percentage of all logical processors.
    pub usage_pct: f32,
    /// Recent once-a-second loads for the sparkline, oldest first, at most 24.
    pub history_pct: Vec<f32>,
    /// Logical cores reserved by active leases.
    pub reserved_cores: f32,
}

/// Physical memory.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RamMeter {
    /// Installed physical memory.
    pub total_mb: u64,
    /// Physical memory in use.
    pub used_mb: u64,
    /// Memory reserved by active leases.
    pub reserved_mb: u64,
}

/// One GPU adapter.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuMeter {
    /// Adapter name, for example `NVIDIA GeForce RTX 4070`.
    pub name: String,
    /// Load of the busiest engine group over the last second, in percent.
    pub busiest_pct: f32,
    /// Dedicated video memory in use.
    pub vram_used_mb: u64,
    /// Dedicated video memory installed, when the OS reports it.
    pub vram_total_mb: Option<u64>,
    /// Engine load reserved by active leases, in percent. Leases reserve on the adapter with
    /// the most video memory only, so it is 0 on every other adapter.
    pub reserved_pct: f32,
    /// Video memory reserved by active leases; 0 on every adapter but the reserving one.
    pub reserved_vram_mb: u64,
}

/// A finished job.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentRun {
    /// Lease the job ran under, if the lease is still stored.
    pub lease_id: Option<LeaseId>,
    /// Session that ran it; `None` for runs recorded before sessions were.
    pub session: Option<SessionKey>,
    /// The lease's label, usually the command as it was run.
    pub command: String,
    /// Working directory of the command.
    pub cwd: String,
    /// Profile the lease reserved.
    pub declared: JobProfile,
    /// Start of the run, in milliseconds since the Unix epoch.
    pub started_at_ms: i64,
    /// End of the run, in milliseconds since the Unix epoch.
    pub ended_at_ms: i64,
    /// Why it ended; `None` for runs recorded before causes were.
    pub end_cause: Option<EndCause>,
    /// Background load during the run; present only for benchmark leases.
    pub noise: Option<NoiseReport>,
}

impl From<JobRun> for RecentRun {
    fn from(run: JobRun) -> Self {
        Self {
            lease_id: run.lease_id,
            session: run.session,
            command: run.command,
            cwd: run.cwd,
            declared: run.declared,
            started_at_ms: run.started_at_ms,
            ended_at_ms: run.ended_at_ms,
            end_cause: run.end_cause,
            noise: run.noise,
        }
    }
}

/// A kill or removal that Claude must hear about once.
#[cfg_attr(any(feature = "schema", test), derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// Lease the notice is about. A lease ends once, so this also identifies the notice.
    pub lease_id: LeaseId,
    /// Session to tell.
    pub session: SessionKey,
    /// What the user did: [`EndCause::Killed`] or [`EndCause::Removed`].
    pub cause: EndCause,
    /// The lease's label, usually the command.
    pub command: String,
    /// When the lease ended, in milliseconds since the Unix epoch.
    pub ended_at_ms: i64,
    /// When a hook or tool result told the session, or `None` while it has not been told.
    pub delivered_at_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::assert_json_round_trip;

    #[test]
    fn test_notice_json_round_trip_writes_cause_and_null_delivery() {
        let notice = Notice {
            lease_id: LeaseId(44),
            session: SessionKey("3f2a9c1e-session".into()),
            cause: EndCause::Removed,
            command: "cargo test".into(),
            ended_at_ms: 1_793_000_090_000,
            delivered_at_ms: None,
        };
        assert_json_round_trip(&notice);
        let value = serde_json::to_value(&notice).unwrap();
        assert_eq!(value["lease_id"], 44);
        assert_eq!(value["cause"], "removed");
        assert!(value["delivered_at_ms"].is_null());
    }
}
