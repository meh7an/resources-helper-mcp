//! What an active lease's session used while it ran, and how much load from outside that
//! session disturbed it.

use rh_shared::types::{LeaseId, NoiseReport, ResourceUsage, SessionKey, Snapshot};

/// What a finished lease measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeteredRun {
    /// Load from outside the holder's session; reported only for benchmark leases.
    pub noise: NoiseReport,
    /// Peak usage of the holder's session, per resource.
    pub holder_peak: ResourceUsage,
    /// Average usage of the holder's session.
    pub holder_avg: ResourceUsage,
    /// Whether the session held another active lease at some point during the run, so its
    /// usage mixes several jobs.
    pub overlapped: bool,
}

/// Accumulates load samples while a lease is active.
#[derive(Debug, Clone)]
pub struct RunMeter {
    lease_id: LeaseId,
    session: SessionKey,
    quiet_at_start: bool,
    overlapped: bool,
    last_taken_at_ms: i64,
    samples: u32,
    noise_cpu: Stat,
    noise_gpu: Stat,
    holder_sum: [f64; 4],
    holder_peak: ResourceUsage,
}

#[derive(Debug, Clone, Copy, Default)]
struct Stat {
    sum: f64,
    peak: f32,
}

impl Stat {
    fn add(&mut self, value: f32) {
        self.sum += f64::from(value);
        self.peak = self.peak.max(value);
    }

    fn mean(self, count: u32) -> f32 {
        (self.sum / f64::from(count)) as f32
    }
}

impl RunMeter {
    /// Starts measuring lease `lease_id` of `session`, granted when `granted_snapshot` was
    /// the latest sample. That sample predates the grant, so it is not counted.
    /// `quiet_at_start` only matters for benchmark leases.
    pub fn start(
        lease_id: LeaseId,
        session: SessionKey,
        quiet_at_start: bool,
        granted_snapshot: &Snapshot,
    ) -> Self {
        Self {
            lease_id,
            session,
            quiet_at_start,
            overlapped: false,
            last_taken_at_ms: granted_snapshot.taken_at_ms,
            samples: 0,
            noise_cpu: Stat::default(),
            noise_gpu: Stat::default(),
            holder_sum: [0.0; 4],
            holder_peak: ResourceUsage::default(),
        }
    }

    /// The lease being measured.
    pub fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    /// The session whose usage is measured.
    pub fn session(&self) -> &SessionKey {
        &self.session
    }

    /// Records that the session held another active lease during the run.
    pub fn mark_overlapped(&mut self) {
        self.overlapped = true;
    }

    /// Adds `snapshot` unless it was already counted.
    pub fn observe(&mut self, snapshot: &Snapshot) {
        if snapshot.taken_at_ms <= self.last_taken_at_ms {
            return;
        }
        self.last_taken_at_ms = snapshot.taken_at_ms;
        self.samples += 1;

        let mut others = snapshot.unattributed;
        let mut holder = ResourceUsage::default();
        for session in &snapshot.sessions {
            if session.holder.session == self.session {
                holder += session.usage;
            } else {
                others += session.usage;
            }
        }
        let logical_cores = snapshot.cpu.logical_cores.max(1) as f32;
        self.noise_cpu.add(others.cores / logical_cores * 100.0);
        self.noise_gpu.add(others.gpu_pct.min(100.0));

        let values = [
            f64::from(holder.cores),
            holder.ram_mb as f64,
            f64::from(holder.gpu_pct),
            holder.vram_mb as f64,
        ];
        for (sum, value) in self.holder_sum.iter_mut().zip(values) {
            *sum += value;
        }
        let peak = &mut self.holder_peak;
        peak.cores = peak.cores.max(holder.cores);
        peak.ram_mb = peak.ram_mb.max(holder.ram_mb);
        peak.gpu_pct = peak.gpu_pct.max(holder.gpu_pct);
        peak.vram_mb = peak.vram_mb.max(holder.vram_mb);
    }

    /// The measurements, or `None` when no sample arrived during the lease.
    pub fn finish(&self) -> Option<MeteredRun> {
        let count = self.samples;
        if count == 0 {
            return None;
        }
        let mean = |sum: f64| sum / f64::from(count);
        let [cores, ram_mb, gpu_pct, vram_mb] = self.holder_sum.map(mean);
        Some(MeteredRun {
            noise: NoiseReport {
                avg_cpu_pct: self.noise_cpu.mean(count),
                peak_cpu_pct: self.noise_cpu.peak,
                avg_gpu_pct: self.noise_gpu.mean(count),
                peak_gpu_pct: self.noise_gpu.peak,
                quiet_at_start: self.quiet_at_start,
            },
            holder_peak: self.holder_peak,
            holder_avg: ResourceUsage {
                cores: cores as f32,
                ram_mb: ram_mb.round() as u64,
                gpu_pct: gpu_pct as f32,
                vram_mb: vram_mb.round() as u64,
            },
            overlapped: self.overlapped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{session_usage, snapshot};

    fn at(taken_at_ms: i64, cpu_pct: f32) -> Snapshot {
        Snapshot {
            taken_at_ms,
            ..snapshot(cpu_pct, 0, None)
        }
    }

    #[test]
    fn test_run_meter_separates_holder_load_from_noise() {
        let granted = at(0, 0.0);
        let mut meter = RunMeter::start(LeaseId(4), SessionKey("bench".into()), true, &granted);
        meter.observe(&granted);
        assert_eq!(meter.finish(), None);

        for (second, other_cores) in [(1, 0.8), (2, 0.0), (2, 9.0)] {
            let mut sample = at(second * 1_000, 50.0);
            sample.sessions = vec![
                session_usage("bench", 8.0),
                session_usage("other", other_cores),
            ];
            sample.unattributed.cores = 0.16;
            meter.observe(&sample);
        }
        let run = meter.finish().unwrap();
        // 16 cores: (0.8 + 0.16) / 16 = 6%, then 0.16 / 16 = 1%; the repeat at 2 s is ignored.
        assert!((run.noise.avg_cpu_pct - 3.5).abs() < 1e-4, "{run:?}");
        assert!((run.noise.peak_cpu_pct - 6.0).abs() < 1e-4, "{run:?}");
        assert!(run.noise.quiet_at_start);
        assert_eq!(run.holder_peak.cores, 8.0);
        assert_eq!(run.holder_avg.cores, 8.0);
        assert!(!run.overlapped);
        assert_eq!(meter.lease_id(), LeaseId(4));
    }

    #[test]
    fn test_run_meter_peak_and_average_differ_for_a_varying_load() {
        let mut meter = RunMeter::start(LeaseId(1), SessionKey("s".into()), false, &at(0, 0.0));
        for (second, cores) in [(1, 2.0), (2, 14.0), (3, 2.0)] {
            let mut sample = at(second * 1_000, 50.0);
            sample.sessions = vec![session_usage("s", cores)];
            meter.observe(&sample);
        }
        meter.mark_overlapped();
        let run = meter.finish().unwrap();
        assert_eq!(run.holder_peak.cores, 14.0);
        assert!((run.holder_avg.cores - 6.0).abs() < 1e-4, "{run:?}");
        assert!(run.overlapped);
    }
}
