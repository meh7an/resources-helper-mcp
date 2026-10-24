//! Windows implementation of [`rh_core::sampler::Sampler`]: sysinfo for CPU, memory, and
//! processes; PDH for commit charge, GPU load, and drive activity; DXGI for adapter names and
//! VRAM sizes; the volume APIs for drive space. Also reads the current process's ancestry,
//! which the CLI and MCP server use to find their Claude session.

mod cpu;
mod drives;
mod dxgi;
mod exe;
mod gpu;
mod pdh;
mod process;

pub use crate::process::current_ancestry;

use std::{
    collections::HashMap,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rh_core::sampler::{RawGpu, RawProcess, RawSample, Sampler};
use rh_shared::{
    Result,
    types::{MemStats, ResourceUsage},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{
    cpu::{CpuTimes, ProcessTime},
    drives::Drives,
    exe::ExeCache,
};

const BYTES_PER_MB: u64 = 1024 * 1024;

/// Reads the local machine.
///
/// PDH handles are not `Send`, so create the sampler on the thread that samples with it.
#[derive(Debug)]
pub struct SystemSampler {
    system: System,
    query: pdh::Query,
    commit_used: pdh::Counter,
    commit_limit: pdh::Counter,
    gpu: Option<GpuCounters>,
    adapters: Vec<dxgi::AdapterInfo>,
    drives: Drives,
    cpu_times: CpuTimes,
    /// Logical cores per PID since the previous refresh.
    process_cores: HashMap<u32, f32>,
    exe_paths: ExeCache,
}

#[derive(Debug)]
struct GpuCounters {
    engine_load: pdh::Counter,
    process_dedicated: pdh::Counter,
    adapter_dedicated: pdh::Counter,
    adapter_shared: pdh::Counter,
}

impl GpuCounters {
    fn add(query: &pdh::Query) -> Result<Self> {
        Ok(Self {
            engine_load: query.add(r"\GPU Engine(*)\Utilization Percentage")?,
            process_dedicated: query.add(r"\GPU Process Memory(*)\Dedicated Usage")?,
            adapter_dedicated: query.add(r"\GPU Adapter Memory(*)\Dedicated Usage")?,
            adapter_shared: query.add(r"\GPU Adapter Memory(*)\Shared Usage")?,
        })
    }
}

#[derive(Debug, Default)]
struct GpuReading {
    adapters: Vec<RawGpu>,
    process_load: HashMap<u32, f32>,
    process_vram_mb: HashMap<u32, u64>,
}

impl SystemSampler {
    /// Opens the counters and takes a first reading, so the next [`Sampler::sample`] has a
    /// baseline for its rates.
    ///
    /// GPU data is best effort: on a machine without GPU counters (some VMs and CI runners)
    /// samples list no adapters, and without DXGI adapters are named by LUID. Drive activity
    /// is best effort too: without the disk counters drives read as idle.
    ///
    /// # Errors
    ///
    /// Returns [`rh_shared::Error::Sampler`] when the PDH query or the memory counters cannot
    /// be opened.
    pub fn new() -> Result<Self> {
        let query = pdh::Query::open()?;
        let commit_used = query.add(r"\Memory\Committed Bytes")?;
        let commit_limit = query.add(r"\Memory\Commit Limit")?;
        let gpu = GpuCounters::add(&query).ok();
        let adapters = dxgi::hardware_adapters().unwrap_or_default();
        let drives = Drives::new(&query);
        let mut sampler = Self {
            system: System::new(),
            query,
            commit_used,
            commit_limit,
            gpu,
            adapters,
            drives,
            cpu_times: CpuTimes::default(),
            process_cores: HashMap::new(),
            exe_paths: ExeCache::default(),
        };
        sampler.refresh()?;
        Ok(sampler)
    }

    fn refresh(&mut self) -> Result<()> {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        // The CPU refresh kind keeps each process's cumulative CPU time current.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let times: Vec<ProcessTime> = self
            .system
            .processes()
            .values()
            .map(|process| ProcessTime {
                pid: process.pid().as_u32(),
                start_time: process.start_time(),
                cpu_ms: process.accumulated_cpu_time(),
            })
            .collect();
        self.exe_paths.update(
            times.iter().map(|time| (time.pid, time.start_time)),
            exe::image_path,
        );
        let now = Instant::now();
        let logical_cores = self.system.cpus().len().max(1) as f32;
        self.process_cores = self.cpu_times.update(now, &times, logical_cores);
        self.drives.refresh(now);
        self.query.collect()
    }

    fn read_gpu(&self, counters: &GpuCounters) -> Result<GpuReading> {
        let load = gpu::group_engine_load(&self.query.values(&counters.engine_load)?);
        let dedicated = self.per_adapter_mb(&counters.adapter_dedicated)?;
        let shared = self.per_adapter_mb(&counters.adapter_shared)?;
        let mut process_vram_mb = HashMap::new();
        for (name, bytes) in self.query.values(&counters.process_dedicated)? {
            if let Some((pid, _)) = gpu::parse_process_memory(&name) {
                *process_vram_mb.entry(pid).or_default() += to_mb(bytes);
            }
        }

        let known: Vec<(String, String, Option<u64>)> = if self.adapters.is_empty() {
            let mut luids: Vec<&String> = dedicated.keys().collect();
            luids.sort();
            luids
                .into_iter()
                .map(|luid| (luid.clone(), luid.clone(), None))
                .collect()
        } else {
            self.adapters
                .iter()
                .map(|info| {
                    (
                        info.name.clone(),
                        info.luid.clone(),
                        Some(info.dedicated_total_mb),
                    )
                })
                .collect()
        };
        let adapters = known
            .into_iter()
            .map(|(name, luid, dedicated_total_mb)| RawGpu {
                engines: load.adapters.get(&luid).copied().unwrap_or_default(),
                dedicated_used_mb: dedicated.get(&luid).copied().unwrap_or_default(),
                shared_used_mb: shared.get(&luid).copied().unwrap_or_default(),
                dedicated_total_mb,
                name,
                luid,
            })
            .collect();
        Ok(GpuReading {
            adapters,
            process_load: load.processes,
            process_vram_mb,
        })
    }

    /// Sums an adapter memory counter per LUID; linked adapters report one instance per
    /// physical GPU.
    fn per_adapter_mb(&self, counter: &pdh::Counter) -> Result<HashMap<String, u64>> {
        let mut totals = HashMap::new();
        for (name, bytes) in self.query.values(counter)? {
            if let Some(luid) = gpu::parse_adapter_memory(&name) {
                *totals.entry(luid).or_default() += to_mb(bytes);
            }
        }
        Ok(totals)
    }

    fn counter_mb(&self, counter: &pdh::Counter) -> Result<u64> {
        Ok(self.query.value(counter)?.map(to_mb).unwrap_or_default())
    }
}

impl Sampler for SystemSampler {
    fn sample(&mut self) -> Result<RawSample> {
        self.refresh()?;
        let gpu = match &self.gpu {
            Some(counters) => self.read_gpu(counters)?,
            None => GpuReading::default(),
        };
        let processes = self
            .system
            .processes()
            .values()
            // PID 0 is the System Idle Process; its "usage" is idle time.
            .filter(|process| process.pid().as_u32() != 0)
            .map(|process| {
                let pid = process.pid().as_u32();
                let start_time = process.start_time();
                RawProcess {
                    pid,
                    parent_pid: process.parent().map(Pid::as_u32),
                    name: process.name().to_string_lossy().into_owned(),
                    exe: self.exe_paths.get(pid, start_time).map(str::to_owned),
                    start_time,
                    usage: ResourceUsage {
                        cores: self.process_cores.get(&pid).copied().unwrap_or_default(),
                        ram_mb: process.memory() / BYTES_PER_MB,
                        gpu_pct: gpu.process_load.get(&pid).copied().unwrap_or_default(),
                        vram_mb: gpu.process_vram_mb.get(&pid).copied().unwrap_or_default(),
                    },
                }
            })
            .collect();
        let taken_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or_default();
        Ok(RawSample {
            taken_at_ms,
            cpu_total_pct: self.system.global_cpu_usage(),
            per_core_pct: self
                .system
                .cpus()
                .iter()
                .map(|cpu| cpu.cpu_usage())
                .collect(),
            memory: MemStats {
                total_mb: self.system.total_memory() / BYTES_PER_MB,
                used_mb: self.system.used_memory() / BYTES_PER_MB,
                available_mb: self.system.available_memory() / BYTES_PER_MB,
                commit_used_mb: self.counter_mb(&self.commit_used)?,
                commit_limit_mb: self.counter_mb(&self.commit_limit)?,
            },
            processes,
            gpus: gpu.adapters,
            drives: self.drives.read(&self.query)?,
        })
    }
}

fn to_mb(bytes: f64) -> u64 {
    (bytes / BYTES_PER_MB as f64) as u64
}

#[cfg(test)]
mod tests {
    use std::{process, thread, time::Duration};

    use super::*;

    #[test]
    fn test_system_sampler_sample_reads_own_process_and_memory() {
        let mut sampler = SystemSampler::new().unwrap();
        thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let raw = sampler.sample().unwrap();

        let own = raw.processes.iter().find(|p| p.pid == process::id());
        let own = own.expect("own process is sampled");
        assert!(own.usage.ram_mb > 0);
        let exe = own.exe.as_deref().expect("own exe path is read");
        assert!(exe.to_ascii_lowercase().ends_with(".exe"), "{exe}");
        assert!(raw.processes.iter().all(|p| p.pid != 0));
        assert!(!raw.per_core_pct.is_empty());
        assert!(raw.memory.total_mb > 0 && raw.memory.available_mb <= raw.memory.total_mb);
        assert!(raw.memory.commit_used_mb > 0);
        assert!(raw.memory.commit_used_mb <= raw.memory.commit_limit_mb);
    }

    const BURN_MS_VAR: &str = "RH_SAMPLER_TEST_BURN_MS";
    /// CPU time a child must have used before it counts as busy; start-up alone stays below.
    const BUSY_CPU_MS: u64 = 100;

    /// Not a test: the child process of [`busy_child`]. Keeps one core busy for
    /// `RH_SAMPLER_TEST_BURN_MS` milliseconds and does nothing without it.
    #[test]
    #[ignore = "helper process for the CPU sampling tests"]
    fn burn_cpu_helper() {
        let Some(millis) = std::env::var(BURN_MS_VAR)
            .ok()
            .and_then(|ms| ms.parse().ok())
        else {
            return;
        };
        let end = Instant::now() + Duration::from_millis(millis);
        while Instant::now() < end {
            std::hint::spin_loop();
        }
    }

    /// This test binary running [`burn_cpu_helper`] for `millis`. Returns only once the child
    /// is burning, because start-up on a loaded CI machine can outlast a fixed sleep.
    fn busy_child(millis: u64) -> process::Child {
        let child = process::Command::new(std::env::current_exe().unwrap())
            .args(["tests::burn_cpu_helper", "--exact", "--ignored"])
            .env(BURN_MS_VAR, millis.to_string())
            .stdout(process::Stdio::null())
            .spawn()
            .unwrap();
        wait_until_busy(child.id());
        child
    }

    fn wait_until_busy(pid: u32) {
        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().with_cpu(),
            );
            let used = system.process(pid).map_or(0, |p| p.accumulated_cpu_time());
            if used >= BUSY_CPU_MS {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the burner used only {used} ms of CPU in 30 s"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn sampled_cores(raw: &RawSample, pid: u32) -> f32 {
        let child = raw.processes.iter().find(|p| p.pid == pid);
        child.expect("the child is sampled").usage.cores
    }

    #[test]
    fn test_system_sampler_counts_a_process_started_since_the_previous_sample() {
        let mut sampler = SystemSampler::new().unwrap();
        let mut burner = busy_child(3_000);
        thread::sleep(Duration::from_millis(1_500));
        let raw = sampler.sample().unwrap();
        burner.kill().ok();
        burner.wait().ok();

        let cores = sampled_cores(&raw, burner.id());
        assert!(cores > 0.3, "a busy new process read {cores} cores");
    }

    #[test]
    fn test_system_sampler_first_sample_reads_a_process_that_was_already_running() {
        let mut burner = busy_child(4_000);
        thread::sleep(Duration::from_millis(1_000));
        let mut sampler = SystemSampler::new().unwrap();
        thread::sleep(Duration::from_millis(1_000));
        let raw = sampler.sample().unwrap();
        burner.kill().ok();
        burner.wait().ok();

        let cores = sampled_cores(&raw, burner.id());
        assert!(
            cores > 0.5,
            "a busy running process read {cores} cores in the first sample"
        );
    }
}
