//! Fixed drives: space from the volume APIs, refreshed every few seconds, and read and write
//! rates from the PDH `LogicalDisk` counters.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use rh_shared::{Result, types::DriveUsage};
use windows::{
    Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives},
    core::HSTRING,
};

use crate::{BYTES_PER_MB, pdh};

/// How often the drive list and free space are read again. Space changes slowly, and the
/// volume calls can block for a moment on a spun-down disk.
const SPACE_REFRESH: Duration = Duration::from_secs(10);

/// `GetDriveTypeW` result for a fixed disk. The windows crate defines it only under the large
/// `Win32_System_WindowsProgramming` feature, which nothing else here needs.
const DRIVE_FIXED: u32 = 3;

/// Says when a periodic job is due: on the first check, then once per period.
#[derive(Debug)]
pub(crate) struct Cadence {
    period: Duration,
    last: Option<Instant>,
}

impl Cadence {
    pub(crate) fn new(period: Duration) -> Self {
        Self { period, last: None }
    }

    /// Whether the job is due at `now`. A `true` answer starts the next period.
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        let due = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.period);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// Size and use of one fixed drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriveSpace {
    name: String,
    total_mb: u64,
    used_mb: u64,
}

#[derive(Debug)]
struct DiskCounters {
    read: pdh::Counter,
    write: pdh::Counter,
}

impl DiskCounters {
    fn add(query: &pdh::Query) -> Result<Self> {
        Ok(Self {
            read: query.add(r"\LogicalDisk(*)\Disk Read Bytes/sec")?,
            write: query.add(r"\LogicalDisk(*)\Disk Write Bytes/sec")?,
        })
    }
}

/// The fixed drives and their activity counters.
#[derive(Debug)]
pub(crate) struct Drives {
    counters: Option<DiskCounters>,
    refresh: Cadence,
    spaces: Vec<DriveSpace>,
}

impl Drives {
    /// Adds the activity counters to `query`. They are best effort: without them (disk
    /// performance counters disabled) drives are listed with no activity.
    pub(crate) fn new(query: &pdh::Query) -> Self {
        Self {
            counters: DiskCounters::add(query).ok(),
            refresh: Cadence::new(SPACE_REFRESH),
            spaces: Vec::new(),
        }
    }

    /// Reads the drive list and free space again when they are due at `now`.
    pub(crate) fn refresh(&mut self, now: Instant) {
        if self.refresh.due(now) {
            self.spaces = fixed_drives();
        }
    }

    /// Every fixed drive with its read and write rates since the previous collection.
    pub(crate) fn read(&self, query: &pdh::Query) -> Result<Vec<DriveUsage>> {
        let (reads, writes) = match &self.counters {
            Some(counters) => (
                per_drive(query.values(&counters.read)?),
                per_drive(query.values(&counters.write)?),
            ),
            None => Default::default(),
        };
        Ok(usage(&self.spaces, &reads, &writes))
    }
}

/// Every fixed drive whose size the OS reports. A drive that refuses (a locked BitLocker
/// volume, for example) is left out until a later refresh succeeds.
fn fixed_drives() -> Vec<DriveSpace> {
    // SAFETY: no arguments; returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    letters(mask).filter_map(fixed_drive_space).collect()
}

fn fixed_drive_space(letter: char) -> Option<DriveSpace> {
    let root = HSTRING::from(format!(r"{letter}:\"));
    // SAFETY: `root` is a NUL-terminated path that outlives the call.
    if unsafe { GetDriveTypeW(&root) } != DRIVE_FIXED {
        return None;
    }
    let mut total = 0u64;
    let mut free = 0u64;
    // SAFETY: `root` and both out-pointers outlive the call.
    unsafe { GetDiskFreeSpaceExW(&root, None, Some(&mut total), Some(&mut free)) }.ok()?;
    Some(DriveSpace {
        name: format!("{letter}:"),
        total_mb: total / BYTES_PER_MB,
        used_mb: total.saturating_sub(free) / BYTES_PER_MB,
    })
}

/// The drive letters set in a `GetLogicalDrives` mask, bit 0 being `A`.
fn letters(mask: u32) -> impl Iterator<Item = char> {
    ('A'..='Z')
        .enumerate()
        .filter(move |(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, letter)| letter)
}

/// The drive name of a `LogicalDisk` instance such as `C:`. `_Total`, volumes without a
/// letter (`HarddiskVolume3`) and folder mount points (`C:\mnt\data`) are not drives.
fn drive_name(instance: &str) -> Option<String> {
    match instance.as_bytes() {
        [letter, b':'] if letter.is_ascii_alphabetic() => {
            Some(format!("{}:", letter.to_ascii_uppercase() as char))
        }
        _ => None,
    }
}

/// Rates by drive name, from one `LogicalDisk` wildcard counter.
fn per_drive(values: Vec<(String, f64)>) -> HashMap<String, u64> {
    values
        .into_iter()
        .filter_map(|(instance, rate)| Some((drive_name(&instance)?, rate.max(0.0) as u64)))
        .collect()
}

/// Joins space and activity by drive name. A fixed drive with no counter instance reads as
/// idle, and instances of drives that are not fixed are dropped.
fn usage(
    spaces: &[DriveSpace],
    reads: &HashMap<String, u64>,
    writes: &HashMap<String, u64>,
) -> Vec<DriveUsage> {
    spaces
        .iter()
        .map(|space| DriveUsage {
            name: space.name.clone(),
            total_mb: space.total_mb,
            used_mb: space.used_mb,
            read_bytes_per_sec: reads.get(&space.name).copied().unwrap_or_default(),
            write_bytes_per_sec: writes.get(&space.name).copied().unwrap_or_default(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, process, thread};

    use rh_core::sampler::Sampler;

    use super::*;
    use crate::SystemSampler;

    #[test]
    fn test_letters_maps_bits_to_drive_letters() {
        assert_eq!(letters(0b1101).collect::<String>(), "ACD");
        assert_eq!(letters(1 << 25).collect::<String>(), "Z");
        assert_eq!(letters(0).count(), 0);
        assert_eq!(letters(u32::MAX).count(), 26);
    }

    #[test]
    fn test_drive_name_accepts_only_drive_letters() {
        assert_eq!(drive_name("C:").as_deref(), Some("C:"));
        assert_eq!(drive_name("d:").as_deref(), Some("D:"));
        for instance in ["_Total", "HarddiskVolume3", r"C:\mnt\data", "C", "1:", ""] {
            assert_eq!(drive_name(instance), None, "{instance}");
        }
    }

    #[test]
    fn test_per_drive_keeps_drive_instances_and_clamps_negatives() {
        let rates = per_drive(vec![
            ("C:".into(), 1_500.7),
            ("_Total".into(), 9_999.0),
            ("HarddiskVolume1".into(), 10.0),
            ("E:".into(), -1.0),
        ]);
        assert_eq!(
            rates,
            HashMap::from([("C:".into(), 1_500), ("E:".into(), 0)])
        );
    }

    #[test]
    fn test_usage_joins_by_name_and_drops_drives_that_are_not_fixed() {
        let spaces = [
            DriveSpace {
                name: "C:".into(),
                total_mb: 1_000,
                used_mb: 600,
            },
            DriveSpace {
                name: "D:".into(),
                total_mb: 2_000,
                used_mb: 100,
            },
        ];
        let reads = HashMap::from([("C:".into(), 40), ("F:".into(), 7)]);
        let writes = HashMap::from([("C:".into(), 8)]);
        let drives = usage(&spaces, &reads, &writes);

        assert_eq!(drives.len(), 2);
        assert_eq!(
            (drives[0].read_bytes_per_sec, drives[0].write_bytes_per_sec),
            (40, 8)
        );
        assert_eq!(drives[1].name, "D:");
        assert_eq!(
            (drives[1].read_bytes_per_sec, drives[1].write_bytes_per_sec),
            (0, 0)
        );
    }

    #[test]
    fn test_cadence_is_due_first_then_once_per_period() {
        let start = Instant::now();
        let mut cadence = Cadence::new(Duration::from_secs(10));
        assert!(cadence.due(start));
        assert!(!cadence.due(start + Duration::from_secs(1)));
        assert!(!cadence.due(start + Duration::from_millis(9_999)));
        assert!(cadence.due(start + Duration::from_secs(10)));
        assert!(!cadence.due(start + Duration::from_secs(19)));
        assert!(cadence.due(start + Duration::from_secs(25)));
        assert!(!cadence.due(start + Duration::from_secs(34)));
    }

    #[test]
    fn test_system_sampler_lists_c_drive_with_plausible_usage() {
        let mut sampler = SystemSampler::new().unwrap();
        let raw = sampler.sample().unwrap();
        let c = raw.drives.iter().find(|drive| drive.name == "C:");
        let c = c.expect("C: is a fixed drive");
        assert!(c.total_mb > 1_024, "C: is {} MB", c.total_mb);
        assert!(c.used_mb > 0 && c.used_mb < c.total_mb, "{c:?}");
        assert!(
            raw.drives
                .iter()
                .all(|drive| drive_name(&drive.name).is_some())
        );
    }

    /// Bytes written during the activity test; enough to stand out from background writes.
    const WRITTEN_BYTES: usize = 64 * 1024 * 1024;

    #[test]
    fn test_system_sampler_reads_write_activity_during_a_file_write() {
        let dir = std::env::temp_dir();
        let drive = drive_name(&dir.display().to_string()[..2]).expect("temp dir on a drive");
        let path = dir.join(format!("rh-sampler-io-{}.bin", process::id()));

        let mut sampler = SystemSampler::new().unwrap();
        let mut file = fs::File::create(&path).unwrap();
        let chunk = vec![0x5a_u8; 1024 * 1024];
        for _ in 0..WRITTEN_BYTES / chunk.len() {
            file.write_all(&chunk).unwrap();
        }
        // Without the flush the data could still sit in the cache when the counters are read.
        file.sync_all().unwrap();
        drop(file);
        thread::sleep(Duration::from_millis(250));
        let raw = sampler.sample().unwrap();
        fs::remove_file(&path).ok();

        let usage = raw.drives.iter().find(|usage| usage.name == drive);
        let usage = usage.unwrap_or_else(|| panic!("{drive} is listed: {:?}", raw.drives));
        assert!(
            usage.write_bytes_per_sec > 0,
            "{drive} read no write activity: {usage:?}"
        );
    }
}
