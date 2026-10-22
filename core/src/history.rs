//! History-based estimates: which past runs resemble a request, and what they used.
//!
//! A request's estimate is the declared profile raised to the p90 of its similar runs, never
//! lowered. Memory uses each run's peak because it must fit at its peak. CPU and GPU use each
//! run's average: a build briefly touches every core, and reserving its one-second peak would
//! make it wait for an idle machine. Raised values stop at what the machine can free, so a
//! learned profile never turns into a request that can never start.

use rh_shared::{
    api::JobSummary,
    types::{JobProfile, JobRun},
};

use crate::{
    admission::{self, Room},
    classifier::{Classification, Classifier, Shell},
};

/// Most runs an estimate looks at, newest first.
pub const ESTIMATE_RUNS: usize = 20;

/// What the similar runs of a request needed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Learned {
    /// p90 of peak RAM and VRAM, of average cores and GPU load, and of run time.
    pub profile: JobProfile,
    /// Runs the values come from.
    pub runs: usize,
}

/// A request's profile after history, with one reason per value history changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    /// The profile to reserve.
    pub profile: JobProfile,
    /// Where the values come from; empty without history.
    pub reasons: Vec<String>,
}

/// The key similar runs share: the normalized command, `|`, and the normalized working
/// directory.
///
/// The command is the first heavy part of `label` as the classifier sees it, so redirects,
/// pipes into `tail`, and a leading `cd` do not change it. A label with no heavy part is
/// used whole. Words are joined by single spaces and the program is lowercased; paths compare
/// case-insensitively, as Windows does.
pub fn fingerprint(classifier: &Classifier, label: &str, cwd: &str) -> String {
    format!(
        "{}|{}",
        normalize_command(classifier, label),
        normalize_cwd(cwd)
    )
}

fn normalize_command(classifier: &Classifier, label: &str) -> String {
    let heavy = [Shell::Bash, Shell::PowerShell]
        .into_iter()
        .find_map(|shell| match classifier.classify(label, shell) {
            Classification::Heavy { command, .. } => Some(command),
            _ => None,
        });
    let command = heavy.as_deref().unwrap_or(label);
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return String::new();
    };
    let mut normalized = program.to_lowercase();
    while let Some(word) = words.next() {
        match redirect(word) {
            Some(Redirect::Bare) => {
                words.next();
            }
            Some(Redirect::WithTarget) => {}
            None => {
                normalized.push(' ');
                normalized.push_str(word);
            }
        }
    }
    normalized
}

enum Redirect {
    /// `>`, `2>>`: the target is the next word.
    Bare,
    /// `2>&1`, `>log.txt`.
    WithTarget,
}

/// Whether `word` redirects output or input; where it goes does not change the job.
fn redirect(word: &str) -> Option<Redirect> {
    let operator = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '*' || c == '&');
    if !operator.starts_with(['>', '<']) {
        return None;
    }
    if operator.trim_start_matches(['>', '<']).is_empty() {
        Some(Redirect::Bare)
    } else {
        Some(Redirect::WithTarget)
    }
}

/// Lowercase with backslashes and no trailing separator; Git Bash paths such as `/e/work`
/// become `e:\work`.
fn normalize_cwd(cwd: &str) -> String {
    let mut path = cwd.trim().replace('/', "\\").to_lowercase();
    let bytes = path.as_bytes();
    let msys_drive = bytes.first() == Some(&b'\\')
        && bytes.get(1).is_some_and(u8::is_ascii_alphabetic)
        && matches!(bytes.get(2), None | Some(b'\\'));
    if msys_drive {
        let drive = char::from(bytes[1]);
        path = format!("{drive}:{}", &path[2..]);
    }
    while path.ends_with('\\') && !path.ends_with(":\\") && path.len() > 1 {
        path.pop();
    }
    if path.len() == 2 && path.ends_with(':') {
        path.push('\\');
    }
    path
}

/// What up to [`ESTIMATE_RUNS`] of `runs` (newest first) needed, skipping runs that
/// overlapped another lease of their session. `None` when no run is left.
pub fn learned(runs: &[JobRun]) -> Option<Learned> {
    let clean: Vec<&JobRun> = runs
        .iter()
        .filter(|run| !run.overlapped)
        .take(ESTIMATE_RUNS)
        .collect();
    if clean.is_empty() {
        return None;
    }
    let p90_of = |value: fn(&JobRun) -> f64| p90(clean.iter().map(|run| value(run)).collect());
    let profile = JobProfile {
        cores: p90_of(|run| f64::from(run.avg.cores)) as f32,
        ram_mb: p90_of(|run| run.peak.ram_mb as f64).round() as u64,
        gpu_pct: (p90_of(|run| f64::from(run.avg.gpu_pct)) as f32).min(100.0),
        vram_mb: p90_of(|run| run.peak.vram_mb as f64).round() as u64,
        est_secs: p90_of(|run| {
            admission::ms_to_secs_ceil(run.ended_at_ms - run.started_at_ms) as f64
        }) as u64,
    };
    Some(Learned {
        profile,
        runs: clean.len(),
    })
}

/// Nearest-rank 90th percentile of a non-empty list.
fn p90(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let rank = (values.len() * 9).div_ceil(10).max(1);
    values[rank - 1]
}

/// Raises `declared` to `learned` value by value, each capped at what `ceiling` (the room
/// with every active lease ended) can hold, and never below the declared value.
pub fn estimate(declared: &JobProfile, learned: Option<&Learned>, ceiling: &Room) -> Estimate {
    let Some(learned) = learned else {
        return Estimate {
            profile: *declared,
            reasons: Vec::new(),
        };
    };
    let runs = learned.runs;
    let mut profile = *declared;
    let mut reasons = Vec::new();
    let mut note = |name: &str, raised: Raised, show: &dyn Fn(f64) -> String| {
        let (value, declared, p90) = (show(raised.value), show(raised.declared), show(raised.p90));
        reasons.push(if raised.capped {
            format!(
                "{name} {value}, the most the machine can free \
                 (p90 of {runs} past run(s): {p90}, declared {declared})"
            )
        } else {
            format!("{name} {value} from p90 of {runs} past run(s) (declared {declared})")
        });
    };

    let wanted = learned.profile;
    if let Some(raised) = raise(
        f64::from(declared.cores),
        f64::from(wanted.cores),
        Some(ceiling.cores),
    ) {
        profile.cores = raised.value as f32;
        note("cores", raised, &|value| format!("{value:.1}"));
    }
    if let Some(raised) = raise(
        declared.ram_mb as f64,
        wanted.ram_mb as f64,
        Some(ceiling.ram_mb),
    ) {
        profile.ram_mb = raised.value.round() as u64;
        note("RAM", raised, &|value| format!("{value:.0} MB"));
    }
    if let Some(raised) = raise(
        f64::from(declared.gpu_pct),
        f64::from(wanted.gpu_pct),
        ceiling.gpu_pct,
    ) {
        profile.gpu_pct = raised.value as f32;
        note("GPU", raised, &|value| format!("{value:.0}%"));
    }
    if let Some(raised) = raise(
        declared.vram_mb as f64,
        wanted.vram_mb as f64,
        ceiling.vram_mb,
    ) {
        profile.vram_mb = raised.value.round() as u64;
        note("VRAM", raised, &|value| format!("{value:.0} MB"));
    }
    if let Some(raised) = raise(declared.est_secs as f64, wanted.est_secs as f64, None) {
        profile.est_secs = raised.value as u64;
        note("run time", raised, &|value| format!("{value:.0}s"));
    }

    if reasons.is_empty() {
        reasons.push(format!(
            "{runs} past run(s) stayed within the declared profile"
        ));
    }
    Estimate { profile, reasons }
}

/// One summary per fingerprint in `runs` (newest first), in the order each was last run.
pub fn summarize(runs: &[JobRun]) -> Vec<JobSummary> {
    let mut groups: Vec<(&str, Vec<JobRun>)> = Vec::new();
    for run in runs {
        match groups
            .iter_mut()
            .find(|(fingerprint, _)| *fingerprint == run.fingerprint)
        {
            Some((_, group)) => group.push(run.clone()),
            None => groups.push((run.fingerprint.as_str(), vec![run.clone()])),
        }
    }
    groups
        .into_iter()
        .map(|(fingerprint, group)| {
            let learned = learned(&group);
            let newest = &group[0];
            JobSummary {
                fingerprint: fingerprint.to_owned(),
                command: newest.command.clone(),
                cwd: newest.cwd.clone(),
                runs: u32::try_from(group.len()).unwrap_or(u32::MAX),
                estimate_runs: learned
                    .map_or(0, |learned| u32::try_from(learned.runs).unwrap_or(u32::MAX)),
                last_ended_at_ms: newest.ended_at_ms,
                learned: learned.map(|learned| learned.profile),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct Raised {
    value: f64,
    declared: f64,
    p90: f64,
    capped: bool,
}

/// The raised value when `p90` is above `declared`; it stops at `cap` but never drops below
/// `declared`.
fn raise(declared: f64, p90: f64, cap: Option<f64>) -> Option<Raised> {
    if p90 <= declared {
        return None;
    }
    let (value, capped) = match cap {
        Some(cap) if p90 > cap => (cap.max(declared), true),
        _ => (p90, false),
    };
    Some(Raised {
        value,
        declared,
        p90,
        capped,
    })
}

#[cfg(test)]
mod tests {
    use rh_shared::{
        config::ClassifierConfig,
        types::{JobProfile, ResourceUsage},
    };

    use super::*;

    fn classifier() -> Classifier {
        Classifier::new(&ClassifierConfig::default())
    }

    fn declared() -> JobProfile {
        JobProfile {
            cores: 8.0,
            ram_mb: 4096,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: 300,
        }
    }

    fn roomy() -> Room {
        Room {
            cores: 13.6,
            ram_mb: 27_000.0,
            gpu_pct: Some(85.0),
            vram_mb: Some(9_000.0),
        }
    }

    /// A run that averaged `avg_cores`, peaked at `peak_ram_mb`, and took `secs`.
    fn run(avg_cores: f32, peak_ram_mb: u64, secs: i64) -> JobRun {
        JobRun {
            lease_id: None,
            fingerprint: "cargo build|e:\\app".into(),
            command: "cargo build".into(),
            cwd: "E:\\app".into(),
            declared: declared(),
            peak: ResourceUsage {
                cores: 16.0,
                ram_mb: peak_ram_mb,
                gpu_pct: 0.0,
                vram_mb: 0,
            },
            avg: ResourceUsage {
                cores: avg_cores,
                ram_mb: peak_ram_mb / 2,
                gpu_pct: 0.0,
                vram_mb: 0,
            },
            started_at_ms: 1_000,
            ended_at_ms: 1_000 + secs * 1_000,
            noise: None,
            overlapped: false,
        }
    }

    #[test]
    fn test_fingerprint_ignores_wrappers_redirects_and_path_spelling() {
        let classifier = classifier();
        let plain = fingerprint(&classifier, "cargo build -p rhd", r"E:\Work\App");
        assert_eq!(plain, r"cargo build -p rhd|e:\work\app");
        for (label, cwd) in [
            (
                "cd sub && cargo build -p rhd 2>&1 | tail -5",
                "E:/Work/App/",
            ),
            ("Cargo  build   -p rhd", "/e/work/app"),
            ("cargo build -p rhd; echo done", r"e:\work\app\"),
            ("cargo build -p rhd > build.log 2> err.log", r"e:\work\app"),
            ("cargo build -p rhd *>&1", r"e:\work\app"),
        ] {
            assert_eq!(
                fingerprint(&classifier, label, cwd),
                plain,
                "{label} in {cwd}"
            );
        }
    }

    #[test]
    fn test_fingerprint_keeps_arguments_and_directories_apart() {
        let classifier = classifier();
        let base = fingerprint(&classifier, "cargo build", r"E:\app");
        assert_ne!(
            fingerprint(&classifier, "cargo build --release", r"E:\app"),
            base
        );
        assert_ne!(fingerprint(&classifier, "cargo build", r"E:\other"), base);
        assert_eq!(fingerprint(&classifier, "  ", ""), "|");
    }

    #[test]
    fn test_fingerprint_of_a_light_label_uses_the_whole_label() {
        let classifier = classifier();
        assert_eq!(
            fingerprint(&classifier, "Render   scene 4", r"C:\"),
            r"render scene 4|c:\"
        );
        assert_eq!(fingerprint(&classifier, "x", "/c"), r"x|c:\");
    }

    #[test]
    fn test_learned_takes_p90_of_average_cores_peak_ram_and_run_time() {
        // Ten runs: 1..=10 average cores, 1000..=10000 MB peak, 10..=100 s.
        let runs: Vec<JobRun> = (1..=10)
            .map(|step| run(step as f32, step * 1_000, step as i64 * 10))
            .collect();
        let learned = learned(&runs).unwrap();
        assert_eq!(learned.runs, 10);
        assert_eq!(learned.profile.cores, 9.0);
        assert_eq!(learned.profile.ram_mb, 9_000);
        assert_eq!(learned.profile.est_secs, 90);
        assert_eq!(learned.profile.gpu_pct, 0.0);
    }

    #[test]
    fn test_learned_from_one_run_is_that_run() {
        let learned = learned(&[run(5.5, 2_500, 42)]).unwrap();
        assert_eq!(learned.runs, 1);
        assert_eq!(learned.profile.cores, 5.5);
        assert_eq!(learned.profile.ram_mb, 2_500);
        assert_eq!(learned.profile.est_secs, 42);
    }

    #[test]
    fn test_learned_skips_overlapped_runs_and_keeps_the_newest_twenty() {
        let mut runs = vec![JobRun {
            overlapped: true,
            ..run(15.0, 30_000, 900)
        }];
        runs.extend((0..ESTIMATE_RUNS).map(|_| run(2.0, 1_000, 10)));
        runs.push(run(15.0, 30_000, 900));
        let learned = learned(&runs).unwrap();
        assert_eq!(learned.runs, ESTIMATE_RUNS);
        assert_eq!(learned.profile.cores, 2.0);

        let only_overlapped = [JobRun {
            overlapped: true,
            ..run(1.0, 1, 1)
        }];
        assert_eq!(super::learned(&only_overlapped), None);
        assert_eq!(super::learned(&[]), None);
    }

    #[test]
    fn test_summarize_groups_by_fingerprint_in_order_of_the_last_run() {
        let other = |overlapped| JobRun {
            fingerprint: r"pnpm test|e:\web".into(),
            command: "pnpm test".into(),
            overlapped,
            ..run(3.0, 900, 30)
        };
        let runs = [
            other(true),
            run(6.0, 2_000, 50),
            run(4.0, 1_000, 40),
            other(true),
        ];
        let summaries = summarize(&runs);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].command, "pnpm test");
        assert_eq!((summaries[0].runs, summaries[0].estimate_runs), (2, 0));
        assert_eq!(summaries[0].learned, None);
        assert_eq!((summaries[1].runs, summaries[1].estimate_runs), (2, 2));
        assert_eq!(summaries[1].learned.map(|profile| profile.cores), Some(6.0));
        assert!(summarize(&[]).is_empty());
    }

    #[test]
    fn test_estimate_without_history_is_the_declared_profile() {
        let estimate = estimate(&declared(), None, &roomy());
        assert_eq!(estimate.profile, declared());
        assert!(estimate.reasons.is_empty());
    }

    #[test]
    fn test_estimate_raises_values_history_exceeds_and_names_the_source() {
        let learned = learned(&[run(11.2, 6_000, 95), run(10.0, 5_000, 80)]).unwrap();
        let estimate = estimate(&declared(), Some(&learned), &roomy());
        assert_eq!(estimate.profile.cores, 11.2);
        assert_eq!(estimate.profile.ram_mb, 6_000);
        // Faster runs never lower the declared run time.
        assert_eq!(estimate.profile.est_secs, 300);
        assert_eq!(
            estimate.reasons,
            [
                "cores 11.2 from p90 of 2 past run(s) (declared 8.0)",
                "RAM 6000 MB from p90 of 2 past run(s) (declared 4096 MB)",
            ]
        );
    }

    #[test]
    fn test_estimate_never_lowers_the_declared_profile() {
        let learned = learned(&[run(1.0, 500, 5)]).unwrap();
        let estimate = estimate(&declared(), Some(&learned), &roomy());
        assert_eq!(estimate.profile, declared());
        assert_eq!(
            estimate.reasons,
            ["1 past run(s) stayed within the declared profile"]
        );
    }

    #[test]
    fn test_estimate_caps_at_what_the_machine_can_free() {
        let learned = learned(&[run(15.3, 2_000, 60)]).unwrap();
        let estimate = estimate(&declared(), Some(&learned), &roomy());
        assert!((estimate.profile.cores - 13.6).abs() < 1e-4);
        assert_eq!(
            estimate.reasons,
            [
                "cores 13.6, the most the machine can free (p90 of 1 past run(s): 15.3, declared 8.0)"
            ]
        );

        let cramped = Room {
            cores: 6.0,
            ..roomy()
        };
        let estimate = super::estimate(&declared(), Some(&learned), &cramped);
        assert_eq!(estimate.profile.cores, 8.0);
    }

    #[test]
    fn test_estimate_gpu_without_an_adapter_is_not_capped() {
        let mut gpu_run = run(1.0, 1_000, 10);
        gpu_run.avg.gpu_pct = 70.0;
        gpu_run.peak.vram_mb = 3_000;
        let learned = learned(&[gpu_run]).unwrap();
        let no_gpu = Room {
            gpu_pct: None,
            vram_mb: None,
            ..roomy()
        };
        let estimate = estimate(&declared(), Some(&learned), &no_gpu);
        assert_eq!(estimate.profile.gpu_pct, 70.0);
        assert_eq!(estimate.profile.vram_mb, 3_000);
    }
}
