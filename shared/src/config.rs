use std::{fs, io, path::Path};

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    Error, Result,
    types::{JobClass, JobProfile},
};

/// Daemon configuration, read from `config.toml`.
///
/// Every value has a default, so a user file only lists what it changes. Tables merge field
/// by field over the defaults; arrays such as `classifier.rules` replace the default list as
/// a whole. Unknown keys are rejected so that typos surface instead of being ignored.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// HTTP listener settings.
    pub daemon: DaemonConfig,
    /// How much of the machine the admission engine keeps free.
    pub admission: AdmissionConfig,
    /// When the machine counts as quiet enough for a benchmark.
    pub benchmark: BenchmarkConfig,
    /// Lease lifetime settings.
    pub leases: LeaseConfig,
    /// How long ended leases and job history are kept.
    pub retention: RetentionConfig,
    /// PreToolUse hook behavior.
    pub hook: HookConfig,
    /// The profile behind each [`JobClass`].
    pub classes: ClassProfiles,
    /// Which shell commands count as heavy.
    pub classifier: ClassifierConfig,
}

impl Config {
    /// Parses and validates a TOML document. Missing values take their defaults.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigParse`] for malformed TOML, unknown keys, or wrongly typed
    /// values, and [`Error::Config`] for values out of range.
    pub fn from_toml(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Loads the file at `path`, or the defaults when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigFile`] naming `path` when the file exists but cannot be read,
    /// or holds what [`Config::from_toml`] rejects.
    pub fn load(path: &Path) -> Result<Self> {
        let loaded = match fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => Err(error.into()),
        };
        loaded.map_err(|reason| Error::ConfigFile {
            path: path.to_owned(),
            reason: Box::new(reason),
        })
    }

    /// Checks every value against its allowed range.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] naming the first invalid value.
    pub fn validate(&self) -> Result<()> {
        ensure(self.daemon.port != 0, "daemon.port must not be 0")?;
        ensure(
            (0.0..100.0).contains(&self.admission.headroom_pct),
            "admission.headroom_pct must be at least 0 and below 100",
        )?;
        ensure(
            is_pct(self.benchmark.quiet_cpu_pct),
            "benchmark.quiet_cpu_pct must be 0 to 100",
        )?;
        ensure(
            is_pct(self.benchmark.quiet_gpu_pct),
            "benchmark.quiet_gpu_pct must be 0 to 100",
        )?;
        ensure(
            self.benchmark.quiet_secs > 0,
            "benchmark.quiet_secs must be positive",
        )?;
        ensure(
            self.benchmark.max_quiet_wait_secs >= self.benchmark.quiet_secs,
            "benchmark.max_quiet_wait_secs must be at least quiet_secs",
        )?;
        ensure(
            self.leases.heartbeat_ttl_secs > 0,
            "leases.heartbeat_ttl_secs must be positive",
        )?;
        let retention = &self.retention;
        ensure(
            retention.lease_days > 0,
            "retention.lease_days must be positive",
        )?;
        ensure(
            retention.history_days > 0,
            "retention.history_days must be positive",
        )?;
        ensure(
            retention.keep_runs_per_job > 0,
            "retention.keep_runs_per_job must be positive",
        )?;
        ensure(
            retention.max_job_runs >= retention.keep_runs_per_job,
            "retention.max_job_runs must be at least keep_runs_per_job",
        )?;
        for class in JobClass::ALL {
            if let Some(reason) = self.classes.get(class).invalid_reason() {
                return Err(Error::Config(format!(
                    "classes.{}: {reason}",
                    class.as_str()
                )));
            }
        }
        for (index, rule) in self.classifier.rules.iter().enumerate() {
            let has_blank =
                rule.program.trim().is_empty() || rule.args.iter().any(|arg| arg.trim().is_empty());
            ensure(
                !has_blank,
                format!("classifier.rules[{index}] has a blank program or arg"),
            )?;
        }
        let flags = &self.classifier.light_flags;
        ensure(
            flags.iter().all(|flag| !flag.trim().is_empty()),
            "classifier.light_flags must not contain blank entries",
        )?;
        Ok(())
    }
}

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Config(message.into()))
    }
}

fn is_pct(value: f32) -> bool {
    (0.0..=100.0).contains(&value)
}

/// HTTP listener settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Port on `127.0.0.1`.
    pub port: u16,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { port: 47_823 }
    }
}

/// How much of the machine the admission engine keeps free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdmissionConfig {
    /// Share of each resource (cores, RAM, GPU, VRAM) never handed out to leases, so the
    /// desktop and unleased work stay responsive.
    pub headroom_pct: f32,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self { headroom_pct: 15.0 }
    }
}

/// When the machine counts as quiet enough for a benchmark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BenchmarkConfig {
    /// Total CPU load at or below which the machine is quiet.
    pub quiet_cpu_pct: f32,
    /// GPU engine load at or below which the machine is quiet.
    pub quiet_gpu_pct: f32,
    /// The window the loads are averaged over once no other lease is active. The machine is
    /// quiet when both averages are at or below their limits.
    pub quiet_secs: u64,
    /// How long a drained machine may stay above the limits before a waiting benchmark is
    /// granted anyway, marked as not quiet. At least `quiet_secs`.
    pub max_quiet_wait_secs: u64,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            quiet_cpu_pct: 10.0,
            quiet_gpu_pct: 10.0,
            quiet_secs: 10,
            max_quiet_wait_secs: 60,
        }
    }
}

/// Lease lifetime settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LeaseConfig {
    /// A lease without a heartbeat for this long expires.
    pub heartbeat_ttl_secs: u64,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            heartbeat_ttl_secs: 60,
        }
    }
}

/// How long ended leases and job history are kept. The daemon prunes once an hour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Released and expired leases, with their audit trail, are deleted this many days after
    /// they ended.
    pub lease_days: u64,
    /// Job runs are deleted this many days after they ended, except the newest
    /// `keep_runs_per_job` of each job.
    pub history_days: u64,
    /// Runs of each job kept regardless of age, so estimates survive long breaks. The
    /// estimator reads the newest 20.
    pub keep_runs_per_job: u64,
    /// Hard cap on stored job runs; the oldest beyond it are deleted whatever their job.
    pub max_job_runs: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            lease_days: 30,
            history_days: 90,
            keep_runs_per_job: 20,
            max_job_runs: 5_000,
        }
    }
}

/// PreToolUse hook behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookConfig {
    /// Allow heavy commands when the daemon is unreachable. Off means every heavy command is
    /// denied until the daemon is back.
    pub fail_open: bool,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self { fail_open: true }
    }
}

/// The profile behind each [`JobClass`].
///
/// A partial table such as `[classes.gpu_heavy]` with only `vram_mb` keeps that class's own
/// defaults for the other fields, not zeros.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClassProfiles {
    /// Profile of [`JobClass::Light`].
    pub light: JobProfile,
    /// Profile of [`JobClass::CpuHeavy`].
    pub cpu_heavy: JobProfile,
    /// Profile of [`JobClass::GpuHeavy`].
    pub gpu_heavy: JobProfile,
    /// Profile of [`JobClass::Mixed`].
    pub mixed: JobProfile,
}

impl ClassProfiles {
    /// Returns the profile configured for `class`.
    pub fn get(&self, class: JobClass) -> &JobProfile {
        match class {
            JobClass::Light => &self.light,
            JobClass::CpuHeavy => &self.cpu_heavy,
            JobClass::GpuHeavy => &self.gpu_heavy,
            JobClass::Mixed => &self.mixed,
        }
    }
}

impl Default for ClassProfiles {
    fn default() -> Self {
        Self {
            light: JobProfile {
                cores: 1.0,
                ram_mb: 512,
                gpu_pct: 0.0,
                vram_mb: 0,
                est_secs: 30,
            },
            cpu_heavy: JobProfile {
                cores: 8.0,
                ram_mb: 4096,
                gpu_pct: 0.0,
                vram_mb: 0,
                est_secs: 300,
            },
            gpu_heavy: JobProfile {
                cores: 2.0,
                ram_mb: 4096,
                gpu_pct: 80.0,
                vram_mb: 4096,
                est_secs: 600,
            },
            mixed: JobProfile {
                cores: 6.0,
                ram_mb: 8192,
                gpu_pct: 50.0,
                vram_mb: 2048,
                est_secs: 600,
            },
        }
    }
}

impl<'de> Deserialize<'de> for ClassProfiles {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let overrides = ClassOverrides::deserialize(deserializer)?;
        let defaults = Self::default();
        Ok(Self {
            light: overrides.light.apply(defaults.light),
            cpu_heavy: overrides.cpu_heavy.apply(defaults.cpu_heavy),
            gpu_heavy: overrides.gpu_heavy.apply(defaults.gpu_heavy),
            mixed: overrides.mixed.apply(defaults.mixed),
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ClassOverrides {
    light: ProfileOverride,
    cpu_heavy: ProfileOverride,
    gpu_heavy: ProfileOverride,
    mixed: ProfileOverride,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileOverride {
    cores: Option<f32>,
    ram_mb: Option<u64>,
    gpu_pct: Option<f32>,
    vram_mb: Option<u64>,
    est_secs: Option<u64>,
}

impl ProfileOverride {
    fn apply(self, base: JobProfile) -> JobProfile {
        JobProfile {
            cores: self.cores.unwrap_or(base.cores),
            ram_mb: self.ram_mb.unwrap_or(base.ram_mb),
            gpu_pct: self.gpu_pct.unwrap_or(base.gpu_pct),
            vram_mb: self.vram_mb.unwrap_or(base.vram_mb),
            est_secs: self.est_secs.unwrap_or(base.est_secs),
        }
    }
}

/// Which shell commands count as heavy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClassifierConfig {
    /// Rules checked against every simple command in a command line, in order.
    pub rules: Vec<HeavyRule>,
    /// A simple command containing any of these arguments only prints and exits, so it is
    /// light even when a rule matches it, for example `cargo build --help`.
    pub light_flags: Vec<String>,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            rules: vec![
                HeavyRule::new(
                    "cargo",
                    &["build", "test", "run", "bench"],
                    JobClass::CpuHeavy,
                ),
                HeavyRule::new("pnpm", &["build", "test"], JobClass::CpuHeavy),
                HeavyRule::new("msbuild", &[], JobClass::CpuHeavy),
                HeavyRule::new("cmake", &["--build"], JobClass::CpuHeavy),
                HeavyRule::new("python*", &["train*.py"], JobClass::GpuHeavy),
                HeavyRule::new("py", &["train*.py"], JobClass::GpuHeavy),
            ],
            light_flags: ["--help", "-h", "--version", "-V", "/?"]
                .map(String::from)
                .to_vec(),
        }
    }
}

/// Marks a simple command as heavy when its program and arguments match.
///
/// Matching ignores case, and `*` matches any run of characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeavyRule {
    /// Program name without directory or extension, for example `cargo` or `python*`.
    pub program: String,
    /// Any one of these makes the command heavy; empty matches any arguments. A pattern that
    /// starts with `-` matches that flag anywhere in the command; any other pattern matches
    /// the first positional argument (the first one not starting with `-` or `+`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Class the matched command needs a lease for.
    pub class: JobClass,
}

impl HeavyRule {
    fn new(program: &str, args: &[&str], class: JobClass) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|&arg| arg.into()).collect(),
            class,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use crate::mutate::mutations;

    fn config_error(text: &str) -> String {
        match Config::from_toml(text) {
            Err(Error::Config(message)) => message,
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[test]
    fn test_config_empty_document_yields_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn test_config_default_passes_validation() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn test_config_default_toml_round_trip_preserves_every_value() {
        let text = toml::to_string(&Config::default()).unwrap();
        assert_eq!(Config::from_toml(&text).unwrap(), Config::default());
    }

    #[test]
    fn test_config_partial_table_merges_over_defaults() {
        let config = Config::from_toml(
            "[leases]\nheartbeat_ttl_secs = 90\n\n[classes.gpu_heavy]\nvram_mb = 8192\n",
        )
        .unwrap();
        let defaults = Config::default();
        assert_eq!(config.leases.heartbeat_ttl_secs, 90);
        assert_eq!(config.classes.gpu_heavy.vram_mb, 8192);
        assert_eq!(
            config.classes.gpu_heavy.cores,
            defaults.classes.gpu_heavy.cores
        );
        assert_eq!(config.classes.cpu_heavy, defaults.classes.cpu_heavy);
        assert_eq!(config.daemon, defaults.daemon);
        assert_eq!(config.classifier, defaults.classifier);
    }

    #[test]
    fn test_config_integer_percentage_parses_as_float() {
        let config = Config::from_toml("[admission]\nheadroom_pct = 20\n").unwrap();
        assert_eq!(config.admission.headroom_pct, 20.0);
    }

    #[test]
    fn test_config_user_rules_replace_default_rules() {
        let config = Config::from_toml(
            "[[classifier.rules]]\nprogram = \"python\"\nargs = [\"train.py\"]\nclass = \"gpu_heavy\"\n",
        )
        .unwrap();
        assert_eq!(
            config.classifier.rules,
            vec![HeavyRule::new("python", &["train.py"], JobClass::GpuHeavy)]
        );
    }

    #[test]
    fn test_config_rule_without_args_matches_any_arguments() {
        let config =
            Config::from_toml("[[classifier.rules]]\nprogram = \"ninja\"\nclass = \"cpu_heavy\"\n")
                .unwrap();
        assert!(config.classifier.rules[0].args.is_empty());
    }

    #[test]
    fn test_config_user_rules_keep_default_light_flags() {
        let config =
            Config::from_toml("[[classifier.rules]]\nprogram = \"ninja\"\nclass = \"cpu_heavy\"\n")
                .unwrap();
        let defaults = ClassifierConfig::default();
        assert_eq!(config.classifier.light_flags, defaults.light_flags);
        assert!(defaults.light_flags.contains(&"--help".to_owned()));
    }

    #[test]
    fn test_config_blank_light_flag_is_rejected() {
        let message = config_error("[classifier]\nlight_flags = [\"--help\", \"\"]\n");
        assert!(message.contains("classifier.light_flags"));
    }

    #[test]
    fn test_config_unknown_key_is_rejected() {
        let result = Config::from_toml("[leases]\nheartbeat_tll_secs = 90\n");
        assert!(matches!(result, Err(Error::ConfigParse(_))));
        let result = Config::from_toml("[classes.mixed]\nvram = 1024\n");
        assert!(matches!(result, Err(Error::ConfigParse(_))));
        let result = Config::from_toml("[classes.huge]\ncores = 32\n");
        assert!(matches!(result, Err(Error::ConfigParse(_))));
    }

    #[test]
    fn test_config_wrong_type_is_rejected() {
        let result = Config::from_toml("[daemon]\nport = \"eighty\"\n");
        assert!(matches!(result, Err(Error::ConfigParse(_))));
    }

    #[test]
    fn test_config_out_of_range_values_name_the_key() {
        assert!(config_error("[daemon]\nport = 0\n").contains("daemon.port"));
        assert!(config_error("[admission]\nheadroom_pct = 100\n").contains("headroom_pct"));
        assert!(config_error("[benchmark]\nquiet_cpu_pct = 120\n").contains("quiet_cpu_pct"));
        assert!(config_error("[benchmark]\nquiet_secs = 0\n").contains("quiet_secs"));
        let short_wait = "[benchmark]\nquiet_secs = 30\nmax_quiet_wait_secs = 20\n";
        assert!(config_error(short_wait).contains("max_quiet_wait_secs"));
        assert!(config_error("[leases]\nheartbeat_ttl_secs = 0\n").contains("heartbeat_ttl"));
        assert!(config_error("[classes.mixed]\ngpu_pct = 150\n").contains("classes.mixed"));
        assert!(config_error("[retention]\nlease_days = 0\n").contains("retention.lease_days"));
        assert!(config_error("[retention]\nhistory_days = 0\n").contains("history_days"));
        assert!(config_error("[retention]\nkeep_runs_per_job = 0\n").contains("keep_runs"));
        let small_cap = "[retention]\nkeep_runs_per_job = 50\nmax_job_runs = 10\n";
        assert!(config_error(small_cap).contains("retention.max_job_runs"));
    }

    #[test]
    fn test_config_load_invalid_file_names_the_file_and_the_key() {
        let path = env::temp_dir().join(format!("rh-shared-test-bad-{}.toml", std::process::id()));
        fs::write(&path, "[leases]\nheartbeat_ttl_secs = 0\n").unwrap();
        let loaded = Config::load(&path);
        fs::remove_file(&path).unwrap();
        let message = loaded.unwrap_err().to_string();
        assert!(
            message.starts_with(&path.display().to_string()),
            "{message}"
        );
        assert!(message.contains("leases.heartbeat_ttl_secs"), "{message}");
    }

    #[test]
    fn test_config_malformed_documents_fail_without_panicking() {
        let text = toml::to_string(&Config::default()).unwrap();
        for variant in mutations(&text, 0x5eed, 2_000) {
            match Config::from_toml(&variant) {
                Ok(config) => config.validate().unwrap(),
                Err(error) => assert!(!error.to_string().is_empty()),
            }
        }
    }

    #[test]
    fn test_config_wrongly_typed_value_names_its_key() {
        let text = toml::to_string(&Config::default()).unwrap();
        let mut checked = 0;
        for (index, line) in text.lines().enumerate() {
            let Some((key, _)) = line.split_once(" = ") else {
                continue;
            };
            for wrong in ["\"text\"", "[1, 2]", "{ x = 1 }", "-1", "true", "1.5", "0"] {
                let mut lines: Vec<&str> = text.lines().collect();
                let replaced = format!("{key} = {wrong}");
                lines[index] = &replaced;
                // A replacement of the right type and range is simply accepted.
                if let Err(error) = Config::from_toml(&lines.join("\n")) {
                    let message = error.to_string();
                    assert!(message.contains(key.trim()), "{key} = {wrong}: {message}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 50, "only {checked} rejections were checked");
    }

    #[test]
    fn test_config_blank_rule_program_is_rejected() {
        let message =
            config_error("[[classifier.rules]]\nprogram = \" \"\nclass = \"cpu_heavy\"\n");
        assert!(message.contains("classifier.rules[0]"));
    }

    #[test]
    fn test_config_load_missing_file_yields_defaults() {
        let path = env::temp_dir()
            .join("rh-shared-test-missing")
            .join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }

    #[test]
    fn test_config_load_existing_file_parses_it() {
        let path = env::temp_dir().join(format!("rh-shared-test-{}.toml", std::process::id()));
        fs::write(&path, "[hook]\nfail_open = false\n").unwrap();
        let loaded = Config::load(&path);
        fs::remove_file(&path).unwrap();
        assert!(!loaded.unwrap().hook.fail_open);
    }
}
