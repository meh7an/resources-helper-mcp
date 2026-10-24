//! The compact machine view `system_snapshot` returns, without per-core or per-process
//! detail.

use rh_shared::types::{MemStats, ResourceUsage, Rolling, SessionKey, Snapshot};
use serde::Serialize;

/// Machine load in a form small enough for a tool result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotSummary {
    /// Total CPU load in percent over 1, 10, and 60 s.
    pub cpu_pct: Rolling,
    /// Logical processors.
    pub logical_cores: u32,
    /// Physical memory and commit charge.
    pub memory: MemStats,
    /// Every GPU adapter.
    pub gpus: Vec<GpuSummary>,
    /// Every registered session.
    pub sessions: Vec<SessionSummary>,
    /// Load from processes outside every registered session.
    pub other: ResourceUsage,
}

/// One adapter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpuSummary {
    /// Adapter name.
    pub name: String,
    /// Load of its busiest engine group over the last second.
    pub busiest_engine_pct: f32,
    /// The same over the last 10 seconds.
    pub busiest_engine_pct_10s: f32,
    /// Dedicated video memory in use.
    pub vram_used_mb: u64,
    /// Dedicated video memory installed, when known.
    pub vram_total_mb: Option<u64>,
}

/// One session's totals.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionSummary {
    /// Session ID.
    pub session: SessionKey,
    /// Whether this is the session calling the tool.
    pub is_current: bool,
    /// Its processes, summed.
    pub usage: ResourceUsage,
    /// How many processes it runs.
    pub processes: usize,
}

impl SnapshotSummary {
    /// Summarizes `snapshot`, marking `current` as the caller's session.
    pub fn new(snapshot: &Snapshot, current: &SessionKey) -> Self {
        Self {
            cpu_pct: snapshot.cpu.usage_pct,
            logical_cores: snapshot.cpu.logical_cores,
            memory: snapshot.memory,
            gpus: snapshot
                .gpus
                .iter()
                .map(|gpu| GpuSummary {
                    name: gpu.name.clone(),
                    busiest_engine_pct: gpu.engines.busiest(|load| load.avg_1s),
                    busiest_engine_pct_10s: gpu.engines.busiest(|load| load.avg_10s),
                    vram_used_mb: gpu.dedicated_used_mb,
                    vram_total_mb: gpu.dedicated_total_mb,
                })
                .collect(),
            sessions: snapshot
                .sessions
                .iter()
                .map(|session| SessionSummary {
                    session: session.holder.session.clone(),
                    is_current: &session.holder.session == current,
                    usage: session.usage,
                    processes: session.processes.len(),
                })
                .collect(),
            other: snapshot.unattributed,
        }
    }
}
