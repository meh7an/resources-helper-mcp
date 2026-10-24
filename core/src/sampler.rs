//! Turns raw machine readings into [`Snapshot`]s: rolling averages and per-session usage.
//!
//! The OS-specific reading lives behind the [`Sampler`] trait, so everything here runs on
//! synthetic samples in tests.

use std::collections::{HashMap, HashSet, VecDeque};

use rh_shared::{
    Result,
    types::{
        CpuStats, GpuAdapter, GpuEngines, Holder, MemStats, ProcessUsage, ResourceUsage, Rolling,
        SessionUsage, Snapshot,
    },
};

/// Samples kept per rolling window; at one sample per second this covers the 60 s average.
const WINDOW_CAPACITY: usize = 60;

/// Source of raw machine readings.
pub trait Sampler {
    /// Reads the machine once. Rates such as CPU load cover the time since the previous call,
    /// so callers sample at a fixed interval, normally one second.
    ///
    /// # Errors
    ///
    /// Returns [`rh_shared::Error::Sampler`] when the OS counters cannot be read.
    fn sample(&mut self) -> Result<RawSample>;
}

/// One reading of the machine, before averaging and session attribution.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSample {
    /// When the reading was taken, in milliseconds since the Unix epoch.
    pub taken_at_ms: i64,
    /// Total CPU load as a percentage of all logical processors.
    pub cpu_total_pct: f32,
    /// Load of each logical processor.
    pub per_core_pct: Vec<f32>,
    /// Memory use.
    pub memory: MemStats,
    /// Every process with its usage, GPU included.
    pub processes: Vec<RawProcess>,
    /// Every GPU adapter.
    pub gpus: Vec<RawGpu>,
}

/// One process in a [`RawSample`].
#[derive(Debug, Clone, PartialEq)]
pub struct RawProcess {
    /// Process ID.
    pub pid: u32,
    /// Parent process ID as recorded by the OS, if any.
    pub parent_pid: Option<u32>,
    /// Executable name.
    pub name: String,
    /// Start time in seconds since the Unix epoch.
    pub start_time: u64,
    /// Resources the process uses.
    pub usage: ResourceUsage,
}

/// One GPU adapter in a [`RawSample`].
#[derive(Debug, Clone, PartialEq)]
pub struct RawGpu {
    /// Adapter name.
    pub name: String,
    /// Adapter LUID, for example `0x00000000_0x0000d1a2`.
    pub luid: String,
    /// Current engine load.
    pub engines: EngineLoad,
    /// Dedicated video memory in use.
    pub dedicated_used_mb: u64,
    /// Dedicated video memory installed, when known.
    pub dedicated_total_mb: Option<u64>,
    /// Shared system memory in use by the adapter.
    pub shared_used_mb: u64,
}

/// Load of each engine group of one adapter, as percentages.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EngineLoad {
    /// 3D (graphics) engines.
    pub three_d_pct: f32,
    /// Compute engines.
    pub compute_pct: f32,
    /// Copy engines.
    pub copy_pct: f32,
    /// Video encode and decode engines.
    pub video_pct: f32,
}

/// The most recent values of one measurement, newest last.
#[derive(Debug, Clone, Default)]
pub struct RollingWindow {
    values: VecDeque<f32>,
}

impl RollingWindow {
    /// Adds the newest value, dropping the oldest once the window is full.
    pub fn push(&mut self, value: f32) {
        if self.values.len() == WINDOW_CAPACITY {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    /// Averages over the last 1, 10, and 60 values, or over fewer while the window fills.
    pub fn rolling(&self) -> Rolling {
        Rolling {
            avg_1s: self.average_of_last(1),
            avg_10s: self.average_of_last(10),
            avg_60s: self.average_of_last(60),
        }
    }

    fn average_of_last(&self, count: usize) -> f32 {
        let taken = count.min(self.values.len());
        if taken == 0 {
            return 0.0;
        }
        let sum: f32 = self.values.iter().rev().take(taken).sum();
        sum / taken as f32
    }
}

#[derive(Debug, Default)]
struct EngineWindows {
    three_d: RollingWindow,
    compute: RollingWindow,
    copy: RollingWindow,
    video: RollingWindow,
}

impl EngineWindows {
    fn push(&mut self, load: EngineLoad) -> GpuEngines {
        self.three_d.push(load.three_d_pct);
        self.compute.push(load.compute_pct);
        self.copy.push(load.copy_pct);
        self.video.push(load.video_pct);
        GpuEngines {
            three_d_pct: self.three_d.rolling(),
            compute_pct: self.compute.rolling(),
            copy_pct: self.copy.rolling(),
            video_pct: self.video.rolling(),
        }
    }
}

/// Builds [`Snapshot`]s from consecutive [`RawSample`]s, keeping the rolling windows.
#[derive(Debug, Default)]
pub struct SnapshotBuilder {
    cpu: RollingWindow,
    gpus: HashMap<String, EngineWindows>,
}

impl SnapshotBuilder {
    /// A builder with empty windows.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds `raw` into the rolling windows and attributes its processes to `sessions`.
    pub fn build(&mut self, raw: RawSample, sessions: &[Holder]) -> Snapshot {
        self.cpu.push(raw.cpu_total_pct);
        self.gpus
            .retain(|luid, _| raw.gpus.iter().any(|gpu| &gpu.luid == luid));
        let (mut sessions, mut unattributed) = attribute(&raw.processes, sessions);
        reconcile_gpu(&raw.gpus, &mut sessions, &mut unattributed);
        let gpus = raw
            .gpus
            .into_iter()
            .map(|gpu| GpuAdapter {
                engines: self
                    .gpus
                    .entry(gpu.luid.clone())
                    .or_default()
                    .push(gpu.engines),
                name: gpu.name,
                luid: gpu.luid,
                dedicated_used_mb: gpu.dedicated_used_mb,
                dedicated_total_mb: gpu.dedicated_total_mb,
                shared_used_mb: gpu.shared_used_mb,
            })
            .collect();
        Snapshot {
            taken_at_ms: raw.taken_at_ms,
            cpu: CpuStats {
                logical_cores: u32::try_from(raw.per_core_pct.len()).unwrap_or(u32::MAX),
                usage_pct: self.cpu.rolling(),
                per_core_pct: raw.per_core_pct,
            },
            memory: raw.memory,
            gpus,
            sessions,
            unattributed,
            drives: Vec::new(),
        }
    }
}

/// Splits process usage between `sessions` and everything else.
///
/// A process belongs to the session whose root is its nearest ancestor (or itself). Roots
/// are matched on PID and start time, and the walk up stops at a parent that started after
/// its child: Windows reuses PIDs, so such a parent is an unrelated process. A session whose
/// root is not running is still listed, with zero usage.
pub fn attribute(
    processes: &[RawProcess],
    sessions: &[Holder],
) -> (Vec<SessionUsage>, ResourceUsage) {
    let table: HashMap<u32, &RawProcess> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    let roots: HashMap<u32, usize> = sessions
        .iter()
        .enumerate()
        .filter(|(_, holder)| {
            table
                .get(&holder.pid)
                .is_some_and(|root| root.start_time == holder.pid_start_time)
        })
        .map(|(index, holder)| (holder.pid, index))
        .collect();

    let mut usages: Vec<SessionUsage> = sessions
        .iter()
        .map(|holder| SessionUsage {
            holder: holder.clone(),
            usage: ResourceUsage::default(),
            processes: Vec::new(),
        })
        .collect();
    let mut unattributed = ResourceUsage::default();
    for process in processes {
        match owning_session(process, &table, &roots) {
            Some(index) => {
                let session = &mut usages[index];
                session.usage += process.usage;
                session.processes.push(ProcessUsage {
                    pid: process.pid,
                    name: process.name.clone(),
                    exe: None,
                    usage: process.usage,
                });
            }
            None => unattributed += process.usage,
        }
    }
    for session in &mut usages {
        session
            .processes
            .sort_by(|a, b| b.usage.cores.total_cmp(&a.usage.cores));
    }
    (usages, unattributed)
}

/// Bounds session and unattributed GPU figures by what the adapters actually report.
///
/// Per-process PDH counters count shared GPU allocations once per process that maps them, so
/// summed process VRAM can exceed the adapter's total. Each session is capped at the adapter
/// totals, and the unattributed share is the remainder rather than its own over-counted sum.
fn reconcile_gpu(gpus: &[RawGpu], sessions: &mut [SessionUsage], unattributed: &mut ResourceUsage) {
    let vram_used_mb: u64 = gpus.iter().map(|gpu| gpu.dedicated_used_mb).sum();
    let busiest_pct = gpus
        .iter()
        .map(|gpu| {
            let load = gpu.engines;
            load.three_d_pct
                .max(load.compute_pct)
                .max(load.copy_pct)
                .max(load.video_pct)
        })
        .fold(0.0, f32::max);

    let mut session_vram_mb = 0;
    let mut session_gpu_pct = 0.0;
    for session in sessions.iter_mut() {
        session.usage.vram_mb = session.usage.vram_mb.min(vram_used_mb);
        session.usage.gpu_pct = session.usage.gpu_pct.min(busiest_pct);
        session_vram_mb += session.usage.vram_mb;
        session_gpu_pct += session.usage.gpu_pct;
    }
    unattributed.vram_mb = vram_used_mb.saturating_sub(session_vram_mb);
    unattributed.gpu_pct = (busiest_pct - session_gpu_pct).max(0.0);
}

fn owning_session(
    process: &RawProcess,
    table: &HashMap<u32, &RawProcess>,
    roots: &HashMap<u32, usize>,
) -> Option<usize> {
    let mut seen = HashSet::new();
    let mut current = process;
    loop {
        if let Some(&index) = roots.get(&current.pid) {
            return Some(index);
        }
        if !seen.insert(current.pid) {
            return None;
        }
        current = current
            .parent_pid
            .and_then(|pid| table.get(&pid).copied())
            .filter(|parent| parent.start_time <= current.start_time)?;
    }
}

#[cfg(test)]
mod tests {
    use rh_shared::types::SessionKey;

    use super::*;

    fn usage(cores: f32, ram_mb: u64) -> ResourceUsage {
        ResourceUsage {
            cores,
            ram_mb,
            gpu_pct: 0.0,
            vram_mb: 0,
        }
    }

    fn process(pid: u32, parent_pid: Option<u32>, start_time: u64, cores: f32) -> RawProcess {
        RawProcess {
            pid,
            parent_pid,
            name: format!("p{pid}"),
            start_time,
            usage: usage(cores, 100),
        }
    }

    fn holder(session: &str, pid: u32, pid_start_time: u64) -> Holder {
        Holder {
            session: SessionKey(session.into()),
            pid,
            pid_start_time,
        }
    }

    fn pids(session: &SessionUsage) -> Vec<u32> {
        session
            .processes
            .iter()
            .map(|process| process.pid)
            .collect()
    }

    fn raw(cpu_total_pct: f32, gpus: Vec<RawGpu>) -> RawSample {
        RawSample {
            taken_at_ms: 1_000,
            cpu_total_pct,
            per_core_pct: vec![cpu_total_pct; 4],
            memory: MemStats {
                total_mb: 32_768,
                used_mb: 8_000,
                available_mb: 24_768,
                commit_used_mb: 10_000,
                commit_limit_mb: 40_000,
            },
            processes: vec![process(10, None, 1, 0.5)],
            gpus,
        }
    }

    fn gpu(luid: &str, three_d_pct: f32) -> RawGpu {
        RawGpu {
            name: "Test GPU".into(),
            luid: luid.into(),
            engines: EngineLoad {
                three_d_pct,
                ..EngineLoad::default()
            },
            dedicated_used_mb: 512,
            dedicated_total_mb: Some(8_192),
            shared_used_mb: 64,
        }
    }

    #[test]
    fn test_rolling_window_empty_is_zero() {
        assert_eq!(RollingWindow::default().rolling(), Rolling::default());
    }

    #[test]
    fn test_rolling_window_partial_fill_averages_available_values() {
        let mut window = RollingWindow::default();
        for value in [10.0, 20.0, 30.0] {
            window.push(value);
        }
        let rolling = window.rolling();
        assert_eq!(rolling.avg_1s, 30.0);
        assert_eq!(rolling.avg_10s, 20.0);
        assert_eq!(rolling.avg_60s, 20.0);
    }

    #[test]
    fn test_rolling_window_full_drops_oldest_values() {
        let mut window = RollingWindow::default();
        for _ in 0..50 {
            window.push(100.0);
        }
        for _ in 0..60 {
            window.push(0.0);
        }
        window.push(60.0);
        let rolling = window.rolling();
        assert_eq!(rolling.avg_1s, 60.0);
        assert_eq!(rolling.avg_10s, 6.0);
        assert_eq!(rolling.avg_60s, 1.0);
    }

    #[test]
    fn test_attribute_assigns_descendants_to_their_session() {
        let processes = [
            process(1, None, 1, 0.1),
            process(100, Some(1), 10, 0.5),
            process(101, Some(100), 11, 2.0),
            process(102, Some(101), 12, 3.0),
            process(200, Some(1), 20, 0.25),
            process(201, Some(200), 21, 1.0),
            process(300, Some(1), 30, 4.0),
        ];
        let sessions = [holder("a", 100, 10), holder("b", 200, 20)];
        let (usages, unattributed) = attribute(&processes, &sessions);

        assert_eq!(pids(&usages[0]), [102, 101, 100]);
        assert_eq!(usages[0].usage, usage(5.5, 300));
        assert_eq!(pids(&usages[1]), [201, 200]);
        assert_eq!(usages[1].usage, usage(1.25, 200));
        assert_eq!(unattributed, usage(4.1, 200));
    }

    #[test]
    fn test_attribute_nested_session_takes_nearest_root() {
        let processes = [
            process(100, None, 10, 1.0),
            process(150, Some(100), 15, 1.0),
            process(151, Some(150), 16, 1.0),
        ];
        let sessions = [holder("outer", 100, 10), holder("inner", 150, 15)];
        let (usages, _) = attribute(&processes, &sessions);
        assert_eq!(pids(&usages[0]), [100]);
        assert_eq!(pids(&usages[1]), [150, 151]);
    }

    #[test]
    fn test_attribute_reused_root_pid_is_not_the_session() {
        let processes = [
            process(100, None, 99, 1.0),
            process(101, Some(100), 100, 1.0),
        ];
        let (usages, unattributed) = attribute(&processes, &[holder("a", 100, 10)]);
        assert!(usages[0].processes.is_empty());
        assert_eq!(usages[0].usage, ResourceUsage::default());
        assert_eq!(unattributed, usage(2.0, 200));
    }

    #[test]
    fn test_attribute_parent_younger_than_child_stops_walk() {
        let processes = [process(100, None, 10, 1.0), process(5, Some(100), 5, 1.0)];
        let (usages, unattributed) = attribute(&processes, &[holder("a", 100, 10)]);
        assert_eq!(pids(&usages[0]), [100]);
        assert_eq!(unattributed, usage(1.0, 100));
    }

    #[test]
    fn test_attribute_parent_cycle_terminates() {
        let processes = [process(1, Some(2), 10, 1.0), process(2, Some(1), 10, 1.0)];
        let (usages, unattributed) = attribute(&processes, &[holder("a", 9, 9)]);
        assert!(usages[0].processes.is_empty());
        assert_eq!(unattributed, usage(2.0, 200));
    }

    #[test]
    fn test_snapshot_builder_rolls_cpu_and_gpu_across_samples() {
        let mut builder = SnapshotBuilder::new();
        builder.build(raw(10.0, vec![gpu("0x0_0x1", 40.0)]), &[]);
        let snapshot = builder.build(raw(30.0, vec![gpu("0x0_0x1", 80.0)]), &[]);

        assert_eq!(snapshot.cpu.logical_cores, 4);
        assert_eq!(snapshot.cpu.usage_pct.avg_1s, 30.0);
        assert_eq!(snapshot.cpu.usage_pct.avg_10s, 20.0);
        assert_eq!(snapshot.gpus[0].engines.three_d_pct.avg_10s, 60.0);
        assert_eq!(snapshot.gpus[0].dedicated_total_mb, Some(8_192));
        assert_eq!(snapshot.unattributed.cores, 0.5);
        assert_eq!(snapshot.unattributed.vram_mb, 512);
        assert_eq!(snapshot.unattributed.gpu_pct, 80.0);
    }

    fn gpu_process(pid: u32, parent_pid: Option<u32>, gpu_pct: f32, vram_mb: u64) -> RawProcess {
        RawProcess {
            usage: ResourceUsage {
                gpu_pct,
                vram_mb,
                ..usage(0.0, 0)
            },
            ..process(pid, parent_pid, 10, 0.0)
        }
    }

    #[test]
    fn test_snapshot_builder_overcounted_process_vram_is_bounded_by_adapter() {
        let mut sample = raw(0.0, vec![gpu("0x0_0x1", 50.0)]);
        sample.processes = vec![
            gpu_process(100, None, 30.0, 300),
            gpu_process(101, Some(100), 10.0, 200),
            gpu_process(200, None, 45.0, 900),
            gpu_process(300, None, 20.0, 700),
        ];
        let snapshot = SnapshotBuilder::new().build(sample, &[holder("a", 100, 10)]);

        let session = snapshot.sessions[0].usage;
        assert_eq!((session.vram_mb, session.gpu_pct), (500, 40.0));
        assert_eq!(snapshot.unattributed.vram_mb, 12);
        assert_eq!(snapshot.unattributed.gpu_pct, 10.0);
    }

    #[test]
    fn test_snapshot_builder_session_beyond_adapter_totals_is_capped() {
        let mut sample = raw(0.0, vec![gpu("0x0_0x1", 20.0)]);
        sample.processes = vec![gpu_process(100, None, 35.0, 4_000)];
        let snapshot = SnapshotBuilder::new().build(sample, &[holder("a", 100, 10)]);

        let session = snapshot.sessions[0].usage;
        assert_eq!((session.vram_mb, session.gpu_pct), (512, 20.0));
        assert_eq!(snapshot.unattributed.vram_mb, 0);
        assert_eq!(snapshot.unattributed.gpu_pct, 0.0);
        assert_eq!(snapshot.sessions[0].processes[0].usage.vram_mb, 4_000);
    }

    #[test]
    fn test_snapshot_builder_without_gpus_reports_no_gpu_usage() {
        let mut sample = raw(0.0, vec![]);
        sample.processes = vec![gpu_process(100, None, 35.0, 4_000)];
        let snapshot = SnapshotBuilder::new().build(sample, &[]);
        assert_eq!(snapshot.unattributed.vram_mb, 0);
        assert_eq!(snapshot.unattributed.gpu_pct, 0.0);
    }

    #[test]
    fn test_snapshot_builder_removed_adapter_restarts_its_window() {
        let mut builder = SnapshotBuilder::new();
        builder.build(raw(0.0, vec![gpu("0x0_0x1", 90.0)]), &[]);
        builder.build(raw(0.0, vec![]), &[]);
        let snapshot = builder.build(raw(0.0, vec![gpu("0x0_0x1", 10.0)]), &[]);
        assert_eq!(snapshot.gpus[0].engines.three_d_pct.avg_10s, 10.0);
    }
}
