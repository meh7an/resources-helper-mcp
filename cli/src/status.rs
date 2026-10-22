//! `rh status` and `rh leases`: a human view of the machine, its sessions, and their leases.

use std::{
    collections::HashSet,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rh_client::BlockingClient;
use rh_core::admission::busiest_engine_pct;
use rh_shared::{
    api::{HealthResponse, LeaseQuery},
    types::{JobClass, Lease, LeaseState, ResourceUsage, SessionKey, Snapshot},
};

const SNAPSHOT_WAIT: Duration = Duration::from_secs(5);
const SNAPSHOT_POLL: Duration = Duration::from_millis(200);
const HISTORY_SHOWN: usize = 20;
const SESSION_ID_SHOWN: usize = 8;

/// Prints machine usage, every session with its leases, and the rest. Starts rhd when it is
/// not running.
///
/// # Errors
///
/// Fails when rhd cannot be reached or started.
pub fn status() -> anyhow::Result<()> {
    let client = BlockingClient::discover(true)?;
    let snapshot = snapshot_when_ready(&client)?;
    let health = client.health()?;
    let leases = current_leases(&client)?;
    print!("{}", format_status(&health, &snapshot, &leases, now_ms()));
    Ok(())
}

/// Prints active and queued leases, or with `all` the most recent ones in any state.
///
/// # Errors
///
/// Fails when rhd cannot be reached or started.
pub fn leases(all: bool) -> anyhow::Result<()> {
    let client = BlockingClient::discover(true)?;
    let leases = if all {
        let every = client.leases(&LeaseQuery::default())?;
        let skip = every.len().saturating_sub(HISTORY_SHOWN);
        every.into_iter().skip(skip).collect()
    } else {
        current_leases(&client)?
    };
    if leases.is_empty() {
        println!("no leases");
    }
    let now = now_ms();
    for lease in &leases {
        println!(
            "{}  {}",
            short(&lease.holder.session),
            format_lease(lease, now)
        );
    }
    Ok(())
}

/// A freshly started rhd answers 503 until its first sample; wait that out briefly.
fn snapshot_when_ready(client: &BlockingClient) -> anyhow::Result<Snapshot> {
    let deadline = Instant::now() + SNAPSHOT_WAIT;
    loop {
        match client.snapshot() {
            Err(error) if error.status() == Some(503) && Instant::now() < deadline => {
                thread::sleep(SNAPSHOT_POLL);
            }
            other => return Ok(other?),
        }
    }
}

fn current_leases(client: &BlockingClient) -> anyhow::Result<Vec<Lease>> {
    let mut leases = Vec::new();
    for state in [LeaseState::Active, LeaseState::Queued] {
        let query = LeaseQuery {
            session: None,
            state: Some(state),
        };
        leases.extend(client.leases(&query)?);
    }
    Ok(leases)
}

fn format_status(
    health: &HealthResponse,
    snapshot: &Snapshot,
    leases: &[Lease],
    now_ms: i64,
) -> String {
    let cpu = snapshot.cpu.usage_pct;
    let memory = &snapshot.memory;
    let mut text = format!(
        "rhd {} (pid {}, up {})\n",
        health.version,
        health.pid,
        duration(i64::try_from(health.uptime_secs).unwrap_or(i64::MAX) * 1_000)
    );
    if let Some(error) = &health.config_error {
        text.push_str(&format!(
            "config  REJECTED, still using the previous one: {error}\n"
        ));
    }
    text.push_str(&format!(
        "cpu     {:5.1}% (10s {:.1}%, 60s {:.1}%) of {} logical cores\n",
        cpu.avg_1s, cpu.avg_10s, cpu.avg_60s, snapshot.cpu.logical_cores
    ));
    text.push_str(&format!(
        "ram     {} / {}, commit {} / {}\n",
        gb(memory.used_mb),
        gb(memory.total_mb),
        gb(memory.commit_used_mb),
        gb(memory.commit_limit_mb)
    ));
    for (index, gpu) in snapshot.gpus.iter().enumerate() {
        text.push_str(&format!(
            "gpu{index}    {}: busiest engine {:.0}%, vram {} / {}\n",
            gpu.name,
            busiest_engine_pct(&gpu.engines, |load| load.avg_1s),
            gb(gpu.dedicated_used_mb),
            gpu.dedicated_total_mb.map_or_else(|| "?".to_owned(), gb)
        ));
    }

    text.push_str("\nsessions\n");
    if snapshot.sessions.is_empty() {
        text.push_str("  none registered\n");
    }
    let mut shown: HashSet<&SessionKey> = HashSet::new();
    for session in &snapshot.sessions {
        shown.insert(&session.holder.session);
        text.push_str(&format!(
            "  {}  pid {:<6} {}\n",
            short(&session.holder.session),
            session.holder.pid,
            format_usage(&session.usage)
        ));
        for lease in leases
            .iter()
            .filter(|l| l.holder.session == session.holder.session)
        {
            text.push_str(&format!("    {}\n", format_lease(lease, now_ms)));
        }
    }
    for lease in leases.iter().filter(|l| !shown.contains(&l.holder.session)) {
        text.push_str(&format!(
            "  {}  (not sampled) {}\n",
            short(&lease.holder.session),
            format_lease(lease, now_ms)
        ));
    }
    text.push_str(&format!(
        "other              {}\n",
        format_usage(&snapshot.unattributed)
    ));
    text
}

fn format_usage(usage: &ResourceUsage) -> String {
    format!(
        "{:5.2} cores  ram {}  gpu {:3.0}%  vram {}",
        usage.cores,
        gb(usage.ram_mb),
        usage.gpu_pct,
        gb(usage.vram_mb)
    )
}

fn format_lease(lease: &Lease, now_ms: i64) -> String {
    let profile = &lease.profile;
    let class = lease.class.map_or("custom", JobClass::as_str);
    let since = match lease.state {
        LeaseState::Active => lease.granted_at_ms,
        LeaseState::Queued => Some(lease.requested_at_ms),
        _ => lease.ended_at_ms,
    };
    let age = since.map_or_else(String::new, |at| format!(" for {}", duration(now_ms - at)));
    let mut text = format!(
        "lease {:<4} {:<8} {:<9} {:.1} cores, {}",
        lease.id.0,
        lease.state.as_str(),
        class,
        profile.cores,
        gb(profile.ram_mb)
    );
    if profile.gpu_pct > 0.0 || profile.vram_mb > 0 {
        text.push_str(&format!(
            ", gpu {:.0}%, vram {}",
            profile.gpu_pct,
            gb(profile.vram_mb)
        ));
    }
    text.push_str(&format!("{age}  {}", lease.label));
    text
}

fn short(session: &SessionKey) -> String {
    let id: String = session.0.chars().take(SESSION_ID_SHOWN).collect();
    format!("{id:<SESSION_ID_SHOWN$}")
}

pub(crate) fn gb(mb: u64) -> String {
    format!("{:.1} GB", mb as f64 / 1024.0)
}

/// `42s`, `12m 05s`, or `2h 03m`.
pub(crate) fn duration(ms: i64) -> String {
    let secs = ms.max(0) / 1_000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3_600, secs % 3_600 / 60),
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use rh_shared::types::{
        CpuStats, Holder, JobProfile, LeaseId, LeaseKind, MemStats, Rolling, SessionUsage,
    };

    use super::*;

    fn holder(session: &str) -> Holder {
        Holder {
            session: SessionKey(session.into()),
            pid: 60_012,
            pid_start_time: 1,
        }
    }

    fn lease(id: i64, session: &str, state: LeaseState) -> Lease {
        Lease {
            id: LeaseId(id),
            holder: holder(session),
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
            requested_at_ms: 1_000,
            granted_at_ms: Some(2_000),
            ended_at_ms: None,
            expires_at_ms: 60_000,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            taken_at_ms: 0,
            cpu: CpuStats {
                logical_cores: 16,
                usage_pct: Rolling {
                    avg_1s: 23.4,
                    avg_10s: 20.0,
                    avg_60s: 15.0,
                },
                per_core_pct: vec![],
            },
            memory: MemStats {
                total_mb: 32_768,
                used_mb: 21_504,
                available_mb: 11_264,
                commit_used_mb: 40_960,
                commit_limit_mb: 56_320,
            },
            gpus: vec![],
            sessions: vec![SessionUsage {
                holder: holder("3f2a9c1e-aaaa-bbbb"),
                usage: ResourceUsage {
                    cores: 2.3,
                    ram_mb: 410,
                    gpu_pct: 0.0,
                    vram_mb: 0,
                },
                processes: vec![],
            }],
            unattributed: ResourceUsage::default(),
        }
    }

    #[test]
    fn test_format_status_groups_leases_under_their_session() {
        let health = HealthResponse {
            version: "0.1.0".into(),
            pid: 1_234,
            uptime_secs: 3_725,
            config_error: None,
        };
        let leases = [
            lease(12, "3f2a9c1e-aaaa-bbbb", LeaseState::Active),
            lease(13, "77777777-cccc", LeaseState::Queued),
        ];
        let text = format_status(&health, &snapshot(), &leases, 62_000);
        assert!(
            text.starts_with("rhd 0.1.0 (pid 1234, up 1h 02m)\n"),
            "{text}"
        );
        assert!(text.contains("cpu      23.4% (10s 20.0%, 60s 15.0%) of 16 logical cores"));
        assert!(text.contains("ram     21.0 GB / 32.0 GB, commit 40.0 GB / 55.0 GB"));
        assert!(
            text.contains("  3f2a9c1e  pid 60012   2.30 cores"),
            "{text}"
        );
        assert!(
            text.contains(
                "    lease 12   active   cpu_heavy 8.0 cores, 4.0 GB for 1m 00s  cargo build"
            ),
            "{text}"
        );
        assert!(
            text.contains("  77777777  (not sampled) lease 13"),
            "{text}"
        );
        assert!(!text.contains("config"), "{text}");
    }

    #[test]
    fn test_format_status_shows_a_rejected_config() {
        let health = HealthResponse {
            version: "0.1.0".into(),
            pid: 1_234,
            uptime_secs: 5,
            config_error: Some("config.toml: invalid configuration: x".into()),
        };
        let text = format_status(&health, &snapshot(), &[], 0);
        assert!(
            text.contains("config  REJECTED, still using the previous one: config.toml:"),
            "{text}"
        );
    }

    #[test]
    fn test_format_lease_shows_gpu_only_when_reserved() {
        let mut gpu_lease = lease(1, "s", LeaseState::Queued);
        gpu_lease.profile.gpu_pct = 80.0;
        gpu_lease.profile.vram_mb = 4_096;
        let text = format_lease(&gpu_lease, 31_000);
        assert!(text.contains(", gpu 80%, vram 4.0 GB for 30s"), "{text}");
        assert!(!format_lease(&lease(2, "s", LeaseState::Active), 0).contains("gpu"));
    }

    #[test]
    fn test_duration_formats() {
        assert_eq!(duration(-5), "0s");
        assert_eq!(duration(42_000), "42s");
        assert_eq!(duration(725_000), "12m 05s");
        assert_eq!(duration(7_380_000), "2h 03m");
    }
}
