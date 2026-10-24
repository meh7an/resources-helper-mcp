//! How much of the machine is left for new leases, and why a profile does or does not fit.
//!
//! Room is reservation-based: an active lease counts with its declared profile from the
//! moment it is granted, whatever its session measures. Measured load only counts when it
//! comes from outside the sessions holding active leases, as background.

use std::collections::HashSet;

use rh_shared::types::{GpuAdapter, JobProfile, Lease, ResourceUsage, Snapshot};

// Tolerance for float sums of reservations, so 8.0 cores fit in 8.0 cores of room.
const EPSILON: f64 = 1e-6;

/// Resources still free for new leases. Values go negative when the machine is overcommitted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Room {
    /// Logical cores.
    pub cores: f64,
    /// Physical memory.
    pub ram_mb: f64,
    /// GPU engine load on the primary adapter, or `None` without a GPU.
    pub gpu_pct: Option<f64>,
    /// Dedicated video memory on the primary adapter, or `None` when there is no GPU or its
    /// VRAM size is unknown.
    pub vram_mb: Option<f64>,
}

impl Room {
    /// Usable capacity (after `headroom_pct`) minus background load minus the reservations of
    /// `active` leases.
    ///
    /// Background CPU and GPU use the 10 s averages so a single spike does not block a grant.
    pub fn measure(snapshot: &Snapshot, active: &[&Lease], headroom_pct: f32) -> Self {
        let keep = 1.0 - f64::from(headroom_pct) / 100.0;
        let leased_sessions: HashSet<_> =
            active.iter().map(|lease| &lease.holder.session).collect();
        let mut leased = ResourceUsage::default();
        for session in &snapshot.sessions {
            if leased_sessions.contains(&session.holder.session) {
                leased += session.usage;
            }
        }

        let logical_cores = f64::from(snapshot.cpu.logical_cores);
        let machine_cores = f64::from(snapshot.cpu.usage_pct.avg_10s) / 100.0 * logical_cores;
        let memory = &snapshot.memory;
        let mut room = Self {
            cores: logical_cores * keep - background(machine_cores, f64::from(leased.cores)),
            ram_mb: memory.total_mb as f64 * keep
                - background(memory.used_mb as f64, leased.ram_mb as f64),
            gpu_pct: None,
            vram_mb: None,
        };
        if let Some(gpu) = primary_adapter(&snapshot.gpus) {
            let busiest = f64::from(gpu.engines.busiest(|load| load.avg_10s));
            room.gpu_pct = Some(100.0 * keep - background(busiest, f64::from(leased.gpu_pct)));
            room.vram_mb = gpu.dedicated_total_mb.map(|total_mb| {
                total_mb as f64 * keep
                    - background(gpu.dedicated_used_mb as f64, leased.vram_mb as f64)
            });
        }
        for lease in active {
            room.reserve(&lease.profile);
        }
        room
    }

    /// Takes `profile` out of the room.
    pub fn reserve(&mut self, profile: &JobProfile) {
        self.shift(profile, -1.0);
    }

    /// Gives `profile` back to the room.
    pub fn release(&mut self, profile: &JobProfile) {
        self.shift(profile, 1.0);
    }

    fn shift(&mut self, profile: &JobProfile, sign: f64) {
        self.cores += sign * f64::from(profile.cores);
        self.ram_mb += sign * profile.ram_mb as f64;
        if let Some(gpu_pct) = &mut self.gpu_pct {
            *gpu_pct += sign * f64::from(profile.gpu_pct);
        }
        if let Some(vram_mb) = &mut self.vram_mb {
            *vram_mb += sign * profile.vram_mb as f64;
        }
    }

    /// One message per resource `profile` needs more of than the room has; empty means it
    /// fits. VRAM is not checked when the adapter does not report its size.
    pub fn shortfalls(&self, profile: &JobProfile) -> Vec<String> {
        let mut reasons = Vec::new();
        if exceeds(f64::from(profile.cores), self.cores) {
            reasons.push(format!(
                "cores: {:.1} requested, {:.1} free",
                profile.cores,
                self.cores.max(0.0)
            ));
        }
        if exceeds(profile.ram_mb as f64, self.ram_mb) {
            reasons.push(format!(
                "RAM: {} MB requested, {:.0} MB free",
                profile.ram_mb,
                self.ram_mb.max(0.0)
            ));
        }
        let needs_gpu = profile.gpu_pct > 0.0 || profile.vram_mb > 0;
        match self.gpu_pct {
            None if needs_gpu => {
                reasons.push("GPU: requested, but no GPU adapter was found".into())
            }
            Some(free) if exceeds(f64::from(profile.gpu_pct), free) => reasons.push(format!(
                "GPU: {:.0}% requested, {:.0}% free",
                profile.gpu_pct,
                free.max(0.0)
            )),
            _ => {}
        }
        if let Some(free) = self.vram_mb
            && exceeds(profile.vram_mb as f64, free)
        {
            reasons.push(format!(
                "VRAM: {} MB requested, {:.0} MB free",
                profile.vram_mb,
                free.max(0.0)
            ));
        }
        reasons
    }
}

/// The adapter leases reserve GPU and VRAM on: the one with the most dedicated memory, so a
/// discrete GPU wins over an integrated one. Ties keep the first adapter.
pub fn primary_adapter(gpus: &[GpuAdapter]) -> Option<&GpuAdapter> {
    gpus.iter().reduce(|best, gpu| {
        if gpu.dedicated_total_mb.unwrap_or(0) > best.dedicated_total_mb.unwrap_or(0) {
            gpu
        } else {
            best
        }
    })
}

/// A reservation and when it is expected to end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Running {
    /// Expected end, in milliseconds since the Unix epoch.
    pub end_ms: i64,
    /// The reservation.
    pub profile: JobProfile,
}

impl Running {
    /// An active lease, expected to end at its grant time plus its estimated run time.
    pub fn of(lease: &Lease) -> Self {
        let start = lease.granted_at_ms.unwrap_or(lease.requested_at_ms);
        Self::starting(start, &lease.profile)
    }

    /// A reservation of `profile` that starts at `start_ms`.
    pub fn starting(start_ms: i64, profile: &JobProfile) -> Self {
        Self {
            end_ms: start_ms.saturating_add(est_ms(profile)),
            profile: *profile,
        }
    }
}

/// The estimated run time of `profile` in milliseconds.
pub fn est_ms(profile: &JobProfile) -> i64 {
    i64::try_from(profile.est_secs.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

/// The earliest time `profile` fits as the `running` reservations end in order of their
/// expected end, with the background unchanged, and the room at that time before `profile`
/// is taken out. `None` when it does not fit even with every reservation ended.
pub fn first_fit(
    mut room: Room,
    running: &[Running],
    profile: &JobProfile,
    now_ms: i64,
) -> Option<(i64, Room)> {
    if room.shortfalls(profile).is_empty() {
        return Some((now_ms, room));
    }
    let mut by_end = running.to_vec();
    by_end.sort_by_key(|reservation| reservation.end_ms);
    for reservation in by_end {
        room.release(&reservation.profile);
        if room.shortfalls(profile).is_empty() {
            return Some((reservation.end_ms.max(now_ms), room));
        }
    }
    None
}

/// Seconds until `profile` fits, per [`first_fit`].
pub fn wait_estimate_secs(
    room: Room,
    running: &[Running],
    profile: &JobProfile,
    now_ms: i64,
) -> Option<u64> {
    first_fit(room, running, profile, now_ms).map(|(at_ms, _)| ms_to_secs_ceil(at_ms - now_ms))
}

/// Rounds a duration in milliseconds up to whole seconds; negative durations are 0.
pub fn ms_to_secs_ceil(ms: i64) -> u64 {
    u64::try_from(ms.max(0)).unwrap_or(0).div_ceil(1_000)
}

fn background(measured: f64, leased: f64) -> f64 {
    (measured - leased).max(0.0)
}

fn exceeds(requested: f64, free: f64) -> bool {
    requested > free + EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{active_lease, gpu, profile, session_usage, snapshot};

    struct Case {
        name: &'static str,
        snapshot: Snapshot,
        active: Vec<Lease>,
        profile: JobProfile,
        shortfall: Option<&'static str>,
    }

    #[test]
    fn test_room_shortfalls_table() {
        let cases = [
            Case {
                name: "idle machine fits a big build",
                snapshot: snapshot(0.0, 0, None),
                active: vec![],
                profile: profile(12.0, 16_000),
                shortfall: None,
            },
            Case {
                name: "background CPU leaves too few cores",
                snapshot: snapshot(50.0, 0, None),
                active: vec![],
                profile: profile(8.0, 1_000),
                shortfall: Some("cores: 8.0 requested, 6.4 free"),
            },
            Case {
                name: "background RAM leaves too little memory",
                snapshot: snapshot(0.0, 20_000, None),
                active: vec![],
                profile: profile(1.0, 12_000),
                shortfall: Some("RAM: 12000 MB requested"),
            },
            Case {
                name: "reservation of an active lease counts before its load shows",
                snapshot: snapshot(0.0, 0, None),
                active: vec![active_lease(1, "a", profile(8.0, 1_000))],
                profile: profile(8.0, 1_000),
                shortfall: Some("cores: 8.0 requested, 6.4 free"),
            },
            Case {
                name: "load of a leased session is not counted twice",
                snapshot: snapshot_with_session(50.0, "a", 8.0),
                active: vec![active_lease(1, "a", profile(8.0, 1_000))],
                profile: profile(6.0, 1_000),
                shortfall: None,
            },
            Case {
                name: "load of an unleased session is background",
                snapshot: snapshot_with_session(50.0, "b", 8.0),
                active: vec![active_lease(1, "a", profile(1.0, 1_000))],
                profile: profile(6.0, 1_000),
                shortfall: Some("cores: 6.0 requested, 5.4 free"),
            },
            Case {
                name: "GPU request without an adapter",
                snapshot: snapshot(0.0, 0, None),
                active: vec![],
                profile: gpu_profile(50.0, 0),
                shortfall: Some("no GPU adapter"),
            },
            Case {
                name: "busy GPU leaves too little engine room",
                snapshot: snapshot(0.0, 0, Some(gpu(60.0, 1_000, Some(12_288)))),
                active: vec![],
                profile: gpu_profile(50.0, 0),
                shortfall: Some("GPU: 50% requested, 30% free"),
            },
            Case {
                name: "VRAM above the adapter's free memory",
                snapshot: snapshot(0.0, 0, Some(gpu(0.0, 8_000, Some(12_288)))),
                active: vec![],
                profile: gpu_profile(10.0, 4_000),
                shortfall: Some("VRAM: 4000 MB requested"),
            },
            Case {
                name: "unknown VRAM size is not checked",
                snapshot: snapshot(0.0, 0, Some(gpu(0.0, 8_000, None))),
                active: vec![],
                profile: gpu_profile(10.0, 400_000),
                shortfall: None,
            },
        ];
        for case in cases {
            let active: Vec<&Lease> = case.active.iter().collect();
            let room = Room::measure(&case.snapshot, &active, 10.0);
            let reasons = room.shortfalls(&case.profile);
            match case.shortfall {
                None => assert!(reasons.is_empty(), "{}: {reasons:?}", case.name),
                Some(expected) => assert!(
                    reasons.iter().any(|reason| reason.contains(expected)),
                    "{}: expected {expected:?} in {reasons:?}",
                    case.name
                ),
            }
        }
    }

    fn gpu_profile(gpu_pct: f32, vram_mb: u64) -> JobProfile {
        JobProfile {
            gpu_pct,
            vram_mb,
            ..profile(1.0, 1_000)
        }
    }

    fn snapshot_with_session(cpu_pct: f32, session: &str, cores: f32) -> Snapshot {
        let mut snapshot = snapshot(cpu_pct, 0, None);
        snapshot.sessions.push(session_usage(session, cores));
        snapshot
    }

    #[test]
    fn test_primary_adapter_prefers_most_vram_then_first() {
        let integrated = gpu(0.0, 0, Some(512));
        let discrete = gpu(0.0, 0, Some(12_288));
        let gpus = [integrated.clone(), discrete.clone(), discrete.clone()];
        assert_eq!(
            primary_adapter(&gpus).map(|g| g.dedicated_total_mb),
            Some(Some(12_288))
        );
        assert!(std::ptr::eq(primary_adapter(&gpus).unwrap(), &gpus[1]));
        assert!(primary_adapter(&[]).is_none());
    }

    #[test]
    fn test_wait_estimate_counts_leases_ending_soonest_first() {
        let snapshot = snapshot(0.0, 0, None);
        let short = active_lease(1, "a", profile(6.0, 1_000));
        let long = JobProfile {
            est_secs: 900,
            ..profile(6.0, 1_000)
        };
        let long = active_lease(2, "b", long);
        let active = [&long, &short];
        let running: Vec<Running> = active.iter().map(|lease| Running::of(lease)).collect();
        let room = Room::measure(&snapshot, &active, 10.0);
        // `active_lease` grants at 0 ms; the short lease is expected to end at 300 s.
        let wait = wait_estimate_secs(room, &running, &profile(8.0, 1_000), 100_500);
        assert_eq!(wait, Some(200));
        let (at_ms, room_then) = first_fit(room, &running, &profile(8.0, 1_000), 100_500).unwrap();
        assert_eq!(at_ms, 300_000);
        assert!((room_then.cores - 8.4).abs() < 1e-3);
    }

    #[test]
    fn test_wait_estimate_impossible_profile_is_none() {
        let snapshot = snapshot(0.0, 0, None);
        let room = Room::measure(&snapshot, &[], 10.0);
        assert_eq!(
            wait_estimate_secs(room, &[], &profile(64.0, 1_000), 0),
            None
        );
        assert_eq!(
            wait_estimate_secs(room, &[], &profile(1.0, 1_000), 0),
            Some(0)
        );
    }

    #[test]
    fn test_ms_to_secs_ceil_rounds_up_and_clamps() {
        assert_eq!(ms_to_secs_ceil(-5), 0);
        assert_eq!(ms_to_secs_ceil(0), 0);
        assert_eq!(ms_to_secs_ceil(1), 1);
        assert_eq!(ms_to_secs_ceil(2_000), 2);
    }
}
