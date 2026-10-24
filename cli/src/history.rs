//! `rh history`: what past jobs used, and the estimates the daemon learned from them.

use rh_client::BlockingClient;
use rh_shared::{
    api::JobSummary,
    types::{BusyUsage, JobProfile, JobRun},
};

use crate::status::{duration, gb, now_ms};

/// Prints one entry per job fingerprint, or with `fingerprint` the runs of that job.
///
/// # Errors
///
/// Fails when rhd cannot be reached or started.
pub fn history(fingerprint: Option<&str>, limit: Option<u32>) -> anyhow::Result<()> {
    let client = BlockingClient::discover(true)?;
    let now = now_ms();
    let text = match fingerprint {
        Some(fingerprint) => {
            format_runs(fingerprint, &client.history_runs(fingerprint, limit)?, now)
        }
        None => format_jobs(&client.history()?, now),
    };
    print!("{text}");
    Ok(())
}

fn format_jobs(jobs: &[JobSummary], now_ms: i64) -> String {
    if jobs.is_empty() {
        return "no job history yet\n".to_owned();
    }
    let mut text = String::from(
        "learned needs: p90 of peak memory and of the load while busy over the newest runs \
         without an overlapping lease\n",
    );
    for job in jobs {
        text.push_str(&format!(
            "\n{}\n  {} run(s), last ended {} ago: {}\n",
            job.fingerprint,
            job.runs,
            duration(now_ms - job.last_ended_at_ms),
            job.command
        ));
        match &job.learned {
            Some(profile) => text.push_str(&format!(
                "  needs {} (from {} run(s))\n",
                format_profile(profile),
                job.estimate_runs
            )),
            None => text.push_str("  no estimate: every run overlapped another lease\n"),
        }
    }
    text
}

fn format_profile(profile: &JobProfile) -> String {
    let mut text = format!("{:.1} cores, {}", profile.cores, gb(profile.ram_mb));
    if profile.gpu_pct > 0.0 || profile.vram_mb > 0 {
        text.push_str(&format!(
            ", gpu {:.0}%, vram {}",
            profile.gpu_pct,
            gb(profile.vram_mb)
        ));
    }
    text.push_str(&format!(", {}", duration(secs_to_ms(profile.est_secs))));
    text
}

fn format_runs(fingerprint: &str, runs: &[JobRun], now_ms: i64) -> String {
    if runs.is_empty() {
        return format!("no runs of {fingerprint}\n");
    }
    let mut text = format!("{fingerprint}\n");
    for run in runs {
        let lease = run
            .lease_id
            .map_or_else(|| "-".to_owned(), |id| id.0.to_string());
        let busy = |value: fn(&BusyUsage) -> f32, digits: usize| {
            run.busy.as_ref().map_or_else(
                || "-".to_owned(),
                |busy| format!("{:.digits$}", value(busy)),
            )
        };
        text.push_str(&format!(
            "  lease {lease:<5} {:>8} ago  took {:<8} cores avg {:.1} busy {} peak {:.1}, \
             ram peak {}",
            duration(now_ms - run.ended_at_ms),
            duration(run.ended_at_ms - run.started_at_ms),
            run.avg.cores,
            busy(|busy| busy.cores, 1),
            run.peak.cores,
            gb(run.peak.ram_mb)
        ));
        if run.peak.gpu_pct > 0.0 || run.peak.vram_mb > 0 {
            text.push_str(&format!(
                ", gpu avg {:.0}% busy {}%, vram peak {}",
                run.avg.gpu_pct,
                busy(|busy| busy.gpu_pct, 0),
                gb(run.peak.vram_mb)
            ));
        }
        if run.overlapped {
            text.push_str("  (overlapped)");
        }
        text.push_str(&format!("  {}\n", run.command));
    }
    text
}

fn secs_to_ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use rh_shared::types::{LeaseId, ResourceUsage};

    use super::*;

    fn profile() -> JobProfile {
        JobProfile {
            cores: 6.25,
            ram_mb: 2_458,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: 95,
        }
    }

    fn run(overlapped: bool) -> JobRun {
        let usage = |cores, ram_mb| ResourceUsage {
            cores,
            ram_mb,
            gpu_pct: 0.0,
            vram_mb: 0,
        };
        JobRun {
            lease_id: Some(LeaseId(41)),
            session: None,
            end_cause: None,
            fingerprint: "cargo build|e:\\app".into(),
            command: "cargo build 2>&1".into(),
            cwd: "E:\\app".into(),
            declared: profile(),
            peak: usage(14.0, 2_458),
            avg: usage(6.2, 1_800),
            busy: Some(BusyUsage {
                cores: 9.3,
                gpu_pct: 0.0,
            }),
            started_at_ms: 0,
            ended_at_ms: 95_000,
            noise: None,
            overlapped,
        }
    }

    #[test]
    fn test_format_jobs_shows_the_learned_needs_or_why_there_are_none() {
        let job = JobSummary {
            fingerprint: "cargo build|e:\\app".into(),
            command: "cargo build 2>&1".into(),
            cwd: "E:\\app".into(),
            runs: 3,
            estimate_runs: 2,
            last_ended_at_ms: 95_000,
            learned: Some(profile()),
        };
        let unknown = JobSummary {
            learned: None,
            estimate_runs: 0,
            ..job.clone()
        };
        let text = format_jobs(&[job, unknown], 397_000);
        assert!(text.contains("\ncargo build|e:\\app\n"), "{text}");
        assert!(
            text.contains("  3 run(s), last ended 5m 02s ago: cargo build 2>&1\n"),
            "{text}"
        );
        assert!(
            text.contains("  needs 6.2 cores, 2.4 GB, 1m 35s (from 2 run(s))\n"),
            "{text}"
        );
        assert!(text.contains("no estimate: every run overlapped"), "{text}");
        assert_eq!(format_jobs(&[], 0), "no job history yet\n");
    }

    #[test]
    fn test_format_runs_marks_overlapped_runs() {
        let older = JobRun {
            busy: None,
            ..run(false)
        };
        let text = format_runs(
            "cargo build|e:\\app",
            &[run(false), run(true), older],
            155_000,
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "cargo build|e:\\app");
        assert_eq!(
            lines[1],
            "  lease 41      1m 00s ago  took 1m 35s   cores avg 6.2 busy 9.3 peak 14.0, \
             ram peak 2.4 GB  cargo build 2>&1"
        );
        assert!(lines[2].contains("(overlapped)  cargo build"), "{text}");
        assert!(
            lines[3].contains("cores avg 6.2 busy - peak 14.0"),
            "{text}"
        );
        assert_eq!(format_runs("x|", &[], 0), "no runs of x|\n");
    }
}
