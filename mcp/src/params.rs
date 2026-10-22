//! Tool parameters as Claude sends them, and their conversion to API types.

use rh_shared::{
    api::ProfileSpec,
    types::{JobClass, JobProfile, LeaseKind},
};
use schemars::JsonSchema;
use serde::Deserialize;

const DEFAULT_EST_SECS: u64 = 300;
/// How long `acquire_lease` waits for a grant unless told otherwise.
pub const DEFAULT_WAIT_SECS: u64 = 60;

/// A named resource class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClassParam {
    /// Short jobs that barely load the machine.
    Light,
    /// Compiles, test suites, and other CPU-bound work.
    CpuHeavy,
    /// Training, rendering, games, and other GPU-bound work.
    GpuHeavy,
    /// Jobs that load both CPU and GPU.
    Mixed,
}

impl From<ClassParam> for JobClass {
    fn from(class: ClassParam) -> Self {
        match class {
            ClassParam::Light => Self::Light,
            ClassParam::CpuHeavy => Self::CpuHeavy,
            ClassParam::GpuHeavy => Self::GpuHeavy,
            ClassParam::Mixed => Self::Mixed,
        }
    }
}

/// How the job shares the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KindParam {
    /// Runs alongside other jobs while their reservations fit.
    #[default]
    Normal,
    /// Waits for a quiet machine and runs alone, for trustworthy measurements.
    BenchmarkExclusive,
}

impl From<KindParam> for LeaseKind {
    fn from(kind: KindParam) -> Self {
        match kind {
            KindParam::Normal => Self::Normal,
            KindParam::BenchmarkExclusive => Self::BenchmarkExclusive,
        }
    }
}

/// The resources a job needs: either `class`, or a custom profile with at least `cores` and
/// `ram_mb`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct ProfileParams {
    /// Named class; omit when giving a custom profile.
    #[serde(default)]
    pub class: Option<ClassParam>,
    /// Custom profile: logical cores the job keeps busy.
    #[serde(default)]
    pub cores: Option<f32>,
    /// Custom profile: peak memory in MB.
    #[serde(default)]
    pub ram_mb: Option<u64>,
    /// Custom profile: GPU engine load in percent of one adapter (default 0).
    #[serde(default)]
    pub gpu_pct: Option<f32>,
    /// Custom profile: peak video memory in MB (default 0).
    #[serde(default)]
    pub vram_mb: Option<u64>,
    /// Custom profile: expected run time in seconds (default 300).
    #[serde(default)]
    pub est_secs: Option<u64>,
}

impl ProfileParams {
    /// The profile as the daemon expects it.
    ///
    /// # Errors
    ///
    /// A message for Claude when both or neither form is given, or a custom profile lacks
    /// `cores` or `ram_mb`.
    pub fn to_spec(&self) -> Result<ProfileSpec, String> {
        let custom = self.cores.is_some()
            || self.ram_mb.is_some()
            || self.gpu_pct.is_some()
            || self.vram_mb.is_some()
            || self.est_secs.is_some();
        match (self.class, custom) {
            (Some(_), true) => Err("give either `class` or a custom profile, not both".to_owned()),
            (Some(class), false) => Ok(ProfileSpec::Class(class.into())),
            (None, true) => {
                let (Some(cores), Some(ram_mb)) = (self.cores, self.ram_mb) else {
                    return Err("a custom profile needs both `cores` and `ram_mb`".to_owned());
                };
                Ok(ProfileSpec::Custom(JobProfile {
                    cores,
                    ram_mb,
                    gpu_pct: self.gpu_pct.unwrap_or(0.0),
                    vram_mb: self.vram_mb.unwrap_or(0),
                    est_secs: self.est_secs.unwrap_or(DEFAULT_EST_SECS),
                }))
            }
            (None, false) => {
                Err("give `class` (for example cpu_heavy) or a custom profile".to_owned())
            }
        }
    }
}

/// Parameters of `can_fit`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct CanFitParams {
    /// The resources the job needs.
    #[serde(flatten)]
    pub profile: ProfileParams,
    /// How the job shares the machine (default normal).
    #[serde(default)]
    pub kind: KindParam,
    /// The command, for example `cargo build --release`. With it, the estimate also uses
    /// what earlier runs of the same command in the same directory needed.
    #[serde(default)]
    pub label: Option<String>,
    /// Directory the command runs in (default: the session's working directory).
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Parameters of `acquire_lease`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct AcquireParams {
    /// Keep waiting for this queued lease instead of requesting a new one; it keeps its
    /// place. The profile, kind, and label are then ignored.
    #[serde(default)]
    pub lease_id: Option<i64>,
    /// The resources to reserve.
    #[serde(flatten)]
    pub profile: ProfileParams,
    /// How the job shares the machine (default normal).
    #[serde(default)]
    pub kind: KindParam,
    /// The command that will run, shown to other sessions, for example `cargo build --release`.
    /// Required for a new lease.
    #[serde(default)]
    pub label: String,
    /// Directory the command runs in (default: the session's working directory). Earlier
    /// runs of the same command there can raise the reserved profile.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Seconds to wait for the lease to start before returning it still queued (default 60,
    /// at most 120).
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

/// Parameters of `release_lease`.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ReleaseParams {
    /// The lease to end, as returned by `acquire_lease`.
    pub lease_id: i64,
}

/// Parameters of `list_leases`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct ListLeasesParams {
    /// Include other sessions' leases (default: only this session's).
    #[serde(default)]
    pub all_sessions: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(cores: Option<f32>, ram_mb: Option<u64>) -> ProfileParams {
        ProfileParams {
            cores,
            ram_mb,
            ..ProfileParams::default()
        }
    }

    #[test]
    fn test_to_spec_class_only() {
        let params = ProfileParams {
            class: Some(ClassParam::GpuHeavy),
            ..ProfileParams::default()
        };
        assert_eq!(params.to_spec(), Ok(ProfileSpec::Class(JobClass::GpuHeavy)));
    }

    #[test]
    fn test_to_spec_custom_fills_defaults() {
        let spec = custom(Some(4.0), Some(2_048)).to_spec().unwrap();
        let expected = JobProfile {
            cores: 4.0,
            ram_mb: 2_048,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: DEFAULT_EST_SECS,
        };
        assert_eq!(spec, ProfileSpec::Custom(expected));
    }

    #[test]
    fn test_to_spec_rejects_ambiguous_or_incomplete_input() {
        assert!(
            ProfileParams::default()
                .to_spec()
                .unwrap_err()
                .contains("give `class`")
        );
        assert!(
            custom(Some(4.0), None)
                .to_spec()
                .unwrap_err()
                .contains("ram_mb")
        );
        let both = ProfileParams {
            class: Some(ClassParam::CpuHeavy),
            ..custom(Some(4.0), Some(1))
        };
        assert!(both.to_spec().unwrap_err().contains("not both"));
    }

    #[test]
    fn test_acquire_params_parse_flattened_profile() {
        let params: AcquireParams = serde_json::from_str(
            r#"{"class":"cpu_heavy","kind":"benchmark_exclusive","label":"cargo bench"}"#,
        )
        .unwrap();
        assert_eq!(params.profile.class, Some(ClassParam::CpuHeavy));
        assert_eq!(params.kind, KindParam::BenchmarkExclusive);
        assert_eq!(params.wait_secs, None);
    }

    #[test]
    fn test_acquire_params_schema_lists_flattened_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(AcquireParams)).unwrap();
        let properties = schema["properties"].as_object().unwrap();
        for field in [
            "lease_id",
            "class",
            "cores",
            "ram_mb",
            "kind",
            "label",
            "cwd",
            "wait_secs",
        ] {
            assert!(properties.contains_key(field), "{field} missing: {schema}");
        }
    }
}
