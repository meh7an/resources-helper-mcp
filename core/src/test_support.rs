use rh_shared::types::{
    CpuStats, GpuAdapter, GpuEngines, Holder, JobProfile, Lease, LeaseId, LeaseKind, LeaseState,
    MemStats, ResourceUsage, Rolling, SessionKey, SessionUsage, Snapshot,
};

pub(crate) const LOGICAL_CORES: u32 = 16;

fn steady(value: f32) -> Rolling {
    Rolling {
        avg_1s: value,
        avg_10s: value,
        avg_60s: value,
    }
}

/// A 16-core, 32 GB machine at a steady `cpu_pct`, with `ram_used_mb` in use.
pub(crate) fn snapshot(cpu_pct: f32, ram_used_mb: u64, gpu: Option<GpuAdapter>) -> Snapshot {
    Snapshot {
        taken_at_ms: 0,
        cpu: CpuStats {
            logical_cores: LOGICAL_CORES,
            usage_pct: steady(cpu_pct),
            per_core_pct: vec![cpu_pct; LOGICAL_CORES as usize],
        },
        memory: MemStats {
            total_mb: 32_768,
            used_mb: ram_used_mb,
            available_mb: 32_768 - ram_used_mb,
            commit_used_mb: ram_used_mb,
            commit_limit_mb: 65_536,
        },
        gpus: gpu.into_iter().collect(),
        sessions: Vec::new(),
        unattributed: ResourceUsage::default(),
    }
}

/// An adapter with a steady 3D load.
pub(crate) fn gpu(three_d_pct: f32, used_mb: u64, total_mb: Option<u64>) -> GpuAdapter {
    GpuAdapter {
        name: "Test GPU".into(),
        luid: "0x00000000_0x00000001".into(),
        engines: GpuEngines {
            three_d_pct: steady(three_d_pct),
            ..GpuEngines::default()
        },
        dedicated_used_mb: used_mb,
        dedicated_total_mb: total_mb,
        shared_used_mb: 0,
    }
}

/// A CPU-only profile expected to run for 300 s.
pub(crate) fn profile(cores: f32, ram_mb: u64) -> JobProfile {
    JobProfile {
        cores,
        ram_mb,
        gpu_pct: 0.0,
        vram_mb: 0,
        est_secs: 300,
    }
}

/// The holder of `session`; its PID is derived from the name so sessions differ.
pub(crate) fn holder(session: &str) -> Holder {
    Holder {
        session: SessionKey(session.into()),
        pid: 1_000 + session.bytes().map(u32::from).sum::<u32>(),
        pid_start_time: 1,
    }
}

pub(crate) fn session_usage(session: &str, cores: f32) -> SessionUsage {
    SessionUsage {
        holder: holder(session),
        usage: ResourceUsage {
            cores,
            ..ResourceUsage::default()
        },
        processes: Vec::new(),
    }
}

/// A queued lease requested at `requested_at_ms` with a 60 s heartbeat deadline.
pub(crate) fn queued_lease(
    id: i64,
    session: &str,
    profile: JobProfile,
    kind: LeaseKind,
    requested_at_ms: i64,
) -> Lease {
    Lease {
        id: LeaseId(id),
        holder: holder(session),
        kind,
        state: LeaseState::Queued,
        class: None,
        profile,
        label: format!("job {id}"),
        cwd: String::new(),
        requested_at_ms,
        granted_at_ms: None,
        ended_at_ms: None,
        expires_at_ms: requested_at_ms + 60_000,
    }
}

/// A normal lease granted at 0 ms.
pub(crate) fn active_lease(id: i64, session: &str, profile: JobProfile) -> Lease {
    Lease {
        state: LeaseState::Active,
        granted_at_ms: Some(0),
        ..queued_lease(id, session, profile, LeaseKind::Normal, 0)
    }
}
