use diesel::{Connection, sqlite::SqliteConnection};
use rh_shared::types::{Holder, JobClass, JobProfile, LeaseKind, SessionKey};

use super::{configure, repo::LeaseDraft, run_migrations};

/// A migrated in-memory database inside a test transaction that is never committed.
pub(crate) fn memory_conn() -> SqliteConnection {
    let mut conn = SqliteConnection::establish(":memory:").unwrap();
    configure(&mut conn).unwrap();
    run_migrations(&mut conn).unwrap();
    conn.begin_test_transaction().unwrap();
    conn
}

pub(crate) fn sample_draft(session: &str, requested_at_ms: i64) -> LeaseDraft {
    LeaseDraft {
        holder: Holder {
            session: SessionKey(session.into()),
            pid: 18_240,
            pid_start_time: 1_793_000_000,
        },
        kind: LeaseKind::Normal,
        class: Some(JobClass::CpuHeavy),
        profile: JobProfile {
            cores: 8.0,
            ram_mb: 4096,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: 300,
        },
        label: "cargo build --release".into(),
        cwd: r"E:\work\app".into(),
        requested_at_ms,
        expires_at_ms: requested_at_ms + 60_000,
    }
}
