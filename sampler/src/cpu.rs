//! Per-process CPU load from cumulative CPU time.
//!
//! sysinfo's own per-process rate on Windows reads zero for the first two refreshes of a
//! process: the first comes too soon after the process appears, and the second divides by the
//! machine's CPU time since boot. Builds run as many processes that live a few seconds each, so
//! their load went missing from every session. Deltas of cumulative CPU time count a process
//! that appeared since the previous sample from its start instead.

use std::{collections::HashMap, time::Instant};

/// A process's cumulative CPU time at one refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessTime {
    pub(crate) pid: u32,
    /// Start time in seconds since the Unix epoch; tells a reused PID apart.
    pub(crate) start_time: u64,
    /// CPU time used since the process started, in CPU milliseconds.
    pub(crate) cpu_ms: u64,
}

/// Remembers the previous refresh's CPU times to turn them into rates.
#[derive(Debug, Default)]
pub(crate) struct CpuTimes {
    previous: HashMap<u32, ProcessTime>,
    previous_at: Option<Instant>,
}

impl CpuTimes {
    /// Logical cores each process kept busy since the previous call, capped at `max_cores`.
    /// The first call only records a baseline and returns no rates.
    pub(crate) fn update(
        &mut self,
        now: Instant,
        processes: &[ProcessTime],
        max_cores: f32,
    ) -> HashMap<u32, f32> {
        let interval_ms = self
            .previous_at
            .map(|at| now.saturating_duration_since(at).as_secs_f64() * 1_000.0);
        let mut rates = HashMap::new();
        if let Some(interval_ms) = interval_ms.filter(|&ms| ms > 0.0) {
            for process in processes {
                let used_ms = match self.previous.get(&process.pid) {
                    Some(before) if before.start_time == process.start_time => {
                        process.cpu_ms.saturating_sub(before.cpu_ms)
                    }
                    // Started since the previous refresh: all of its time is new.
                    _ => process.cpu_ms,
                };
                let cores = (used_ms as f64 / interval_ms) as f32;
                rates.insert(process.pid, cores.min(max_cores));
            }
        }
        self.previous = processes
            .iter()
            .map(|process| (process.pid, *process))
            .collect();
        self.previous_at = Some(now);
        rates
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn time(pid: u32, start_time: u64, cpu_ms: u64) -> ProcessTime {
        ProcessTime {
            pid,
            start_time,
            cpu_ms,
        }
    }

    #[test]
    fn test_first_update_is_only_a_baseline() {
        let mut times = CpuTimes::default();
        let rates = times.update(Instant::now(), &[time(1, 10, 5_000)], 16.0);
        assert!(rates.is_empty());
    }

    #[test]
    fn test_running_process_rate_is_its_cpu_time_delta_over_the_interval() {
        let mut times = CpuTimes::default();
        let start = Instant::now();
        times.update(start, &[time(1, 10, 5_000)], 16.0);
        let rates = times.update(start + Duration::from_secs(1), &[time(1, 10, 7_500)], 16.0);
        assert!((rates[&1] - 2.5).abs() < 1e-4, "{rates:?}");
    }

    #[test]
    fn test_new_process_counts_from_its_start() {
        let mut times = CpuTimes::default();
        let start = Instant::now();
        times.update(start, &[time(1, 10, 0)], 16.0);
        // A compiler that appeared during the second and already used 3.2 CPU seconds.
        let rates = times.update(
            start + Duration::from_secs(1),
            &[time(1, 10, 0), time(2, 11, 3_200)],
            16.0,
        );
        assert!((rates[&2] - 3.2).abs() < 1e-4, "{rates:?}");
        assert_eq!(rates[&1], 0.0);
    }

    #[test]
    fn test_reused_pid_is_a_new_process() {
        let mut times = CpuTimes::default();
        let start = Instant::now();
        times.update(start, &[time(7, 10, 90_000)], 16.0);
        let rates = times.update(start + Duration::from_secs(2), &[time(7, 12, 1_000)], 16.0);
        assert!((rates[&7] - 0.5).abs() < 1e-4, "{rates:?}");
    }

    #[test]
    fn test_rate_is_capped_at_the_machine() {
        let mut times = CpuTimes::default();
        let start = Instant::now();
        times.update(start, &[], 16.0);
        let rates = times.update(
            start + Duration::from_millis(500),
            &[time(3, 1, 60_000)],
            16.0,
        );
        assert_eq!(rates[&3], 16.0);
    }
}
