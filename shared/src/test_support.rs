use std::fmt::Debug;

use serde::{Serialize, de::DeserializeOwned};

use crate::types::{
    CpuStats, GpuAdapter, GpuEngines, Holder, JobClass, JobProfile, Lease, LeaseId, LeaseKind,
    LeaseState, MemStats, ProcessUsage, ResourceUsage, Rolling, SessionKey, SessionUsage, Snapshot,
};

/// Serializes `value` to JSON, parses it back, and asserts nothing changed.
pub(crate) fn assert_json_round_trip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let parsed: T = serde_json::from_str(&json).unwrap();
    assert_eq!(
        &parsed, value,
        "round trip changed the value; JSON was {json}"
    );
}

pub(crate) fn sample_holder() -> Holder {
    Holder {
        session: SessionKey("3f2a9c1e-session".into()),
        pid: 18_240,
        pid_start_time: 1_793_000_000,
    }
}

fn rolling(avg_1s: f32, avg_10s: f32, avg_60s: f32) -> Rolling {
    Rolling {
        avg_1s,
        avg_10s,
        avg_60s,
    }
}

pub(crate) fn sample_session_usage() -> SessionUsage {
    let cargo = ResourceUsage {
        cores: 5.5,
        ram_mb: 2300,
        gpu_pct: 0.0,
        vram_mb: 0,
    };
    SessionUsage {
        holder: sample_holder(),
        usage: ResourceUsage {
            cores: 6.25,
            ram_mb: 2900,
            gpu_pct: 1.5,
            vram_mb: 64,
        },
        processes: vec![ProcessUsage {
            pid: 20_112,
            name: "cargo.exe".into(),
            usage: cargo,
        }],
    }
}

pub(crate) fn sample_snapshot() -> Snapshot {
    Snapshot {
        taken_at_ms: 1_793_000_001_250,
        cpu: CpuStats {
            logical_cores: 4,
            usage_pct: rolling(62.5, 48.0, 31.25),
            per_core_pct: vec![90.0, 75.5, 40.0, 44.5],
        },
        memory: MemStats {
            total_mb: 32_768,
            used_mb: 14_200,
            available_mb: 18_568,
            commit_used_mb: 20_100,
            commit_limit_mb: 37_632,
        },
        gpus: vec![GpuAdapter {
            name: "NVIDIA GeForce RTX 4070".into(),
            luid: "0x00000000_0x0000d1a2".into(),
            engines: GpuEngines {
                three_d_pct: rolling(12.0, 10.5, 8.0),
                compute_pct: rolling(0.0, 0.0, 0.0),
                copy_pct: rolling(1.0, 0.5, 0.25),
                video_pct: rolling(0.0, 0.0, 0.0),
            },
            dedicated_used_mb: 1800,
            dedicated_total_mb: Some(12_288),
            shared_used_mb: 300,
        }],
        sessions: vec![sample_session_usage()],
        unattributed: ResourceUsage {
            cores: 0.75,
            ram_mb: 9000,
            gpu_pct: 10.5,
            vram_mb: 1700,
        },
    }
}

pub(crate) fn sample_lease() -> Lease {
    Lease {
        id: LeaseId(7),
        holder: sample_holder(),
        kind: LeaseKind::BenchmarkExclusive,
        state: LeaseState::Active,
        class: Some(JobClass::CpuHeavy),
        profile: JobProfile {
            cores: 8.0,
            ram_mb: 4096,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: 300,
        },
        label: "cargo bench".into(),
        cwd: r"E:\work\app".into(),
        requested_at_ms: 1_793_000_000_500,
        granted_at_ms: Some(1_793_000_012_000),
        ended_at_ms: None,
        expires_at_ms: 1_793_000_072_000,
    }
}
