//! `rh hook pre-tool`: the Claude Code PreToolUse hook for the Bash and PowerShell tools.
//! Denies heavy commands the session holds no covering active lease for.
//!
//! Exit code 2 from a hook blocks the tool call, and clap uses 2 for argument errors, so the
//! hook always exits 0 and denies only through its JSON output. Light commands never touch
//! the network, which keeps them under the 50 ms budget.

use std::{
    ffi::OsStr,
    fmt::Write as _,
    io::{self, Read},
    process::ExitCode,
    time::Duration,
};

use rh_client::{BlockingClient, ClientOptions};
use rh_core::classifier::{Classification, Classifier, Shell, describe_rule};
use rh_shared::{
    api::LeaseQuery,
    config::Config,
    paths::Paths,
    types::{JobClass, Lease, LeaseKind, LeaseState, SessionKey},
};
use serde::{Deserialize, Serialize};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const MCP_SERVER: &str = "resources-helper";

/// The fields of the PreToolUse payload the hook reads; the rest are ignored.
#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    tool_name: String,
    #[serde(default)]
    tool_input: ToolInput,
}

#[derive(Debug, Default, Deserialize)]
struct ToolInput {
    command: Option<String>,
}

/// What the hook tells Claude Code.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// No output: the normal permission flow decides.
    Pass,
    /// Like [`Outcome::Pass`], and the user is shown why the check was skipped.
    Warn(String),
    /// The call is blocked; the reason goes to Claude.
    Deny(String),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    hook_specific_output: Option<PermissionOutput<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_message: Option<&'a str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOutput<'a> {
    hook_event_name: &'static str,
    permission_decision: &'static str,
    permission_decision_reason: &'a str,
}

/// Whether the command line is `rh hook ...`, so argument errors must not exit 2.
pub fn invoked() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(OsStr::new("hook"))
}

/// Reports an argument error of `rh hook` as a warning and lets the tool call through.
pub fn argument_error(error: &clap::Error) -> ExitCode {
    let first_line = error.to_string();
    let first_line = first_line.lines().next().unwrap_or_default();
    emit(&Outcome::Warn(format!(
        "{MCP_SERVER}: lease check skipped, the hook command is wrong ({first_line})"
    )))
}

/// Runs the PreToolUse check on the payload from stdin.
pub fn pre_tool() -> ExitCode {
    let mut stdin = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut stdin) {
        return emit(&skipped(format_args!(
            "cannot read the hook input: {error}"
        )));
    }
    let setup = Paths::from_env().and_then(|paths| {
        let config = Config::load(&paths.config_file())?;
        Ok((paths, config))
    });
    let outcome = match setup {
        Ok((paths, config)) => evaluate(&stdin, &config, |session| {
            let options = ClientOptions {
                connect_timeout: CONNECT_TIMEOUT,
                request_timeout: REQUEST_TIMEOUT,
                ..ClientOptions::local(&paths, &config, false)
            };
            BlockingClient::new(options)?.leases(&LeaseQuery {
                session: Some(session.clone()),
                state: None,
            })
        }),
        Err(error) => skipped(format_args!("cannot load the configuration: {error}")),
    };
    emit(&outcome)
}

/// Decides on one payload. `lookup` returns the session's leases and is only called for heavy
/// commands.
fn evaluate(
    stdin: &str,
    config: &Config,
    lookup: impl FnOnce(&SessionKey) -> rh_client::Result<Vec<Lease>>,
) -> Outcome {
    let input: HookInput = match serde_json::from_str(stdin) {
        Ok(input) => input,
        Err(error) => return skipped(format_args!("unexpected hook input: {error}")),
    };
    let shell = match input.tool_name.as_str() {
        "Bash" => Shell::Bash,
        "PowerShell" => Shell::PowerShell,
        _ => return Outcome::Pass,
    };
    let Some(line) = input.tool_input.command else {
        return Outcome::Pass;
    };
    let (class, rule, command) = match Classifier::new(&config.classifier).classify(&line, shell) {
        Classification::Heavy {
            class,
            matched_rule,
            command,
        } => (class, describe_rule(&matched_rule), command),
        _ => return Outcome::Pass,
    };
    let leases = match lookup(&SessionKey(input.session_id)) {
        Ok(leases) => leases,
        Err(error) if config.hook.fail_open => {
            return skipped(format_args!(
                "rhd is unavailable, `{command}` runs unchecked: {error}"
            ));
        }
        Err(error) => {
            return Outcome::Deny(format!(
                "Blocked by {MCP_SERVER}: `{command}` is {} work, rhd is unavailable ({error}), \
                 and hook.fail_open is off. Ask the user to start it with `rh daemon start`, \
                 then retry.",
                class.as_str()
            ));
        }
    };
    judge(class, &rule, &command, &leases)
}

/// The decision for a heavy `command` of `class`, given every lease of its session.
fn judge(class: JobClass, rule: &str, command: &str, leases: &[Lease]) -> Outcome {
    let in_state = |state| leases.iter().filter(move |lease| lease.state == state);
    if in_state(LeaseState::Active).any(|lease| covers(lease, class)) {
        return Outcome::Pass;
    }
    let name = class.as_str();
    if let Some(queued) = in_state(LeaseState::Queued).find(|lease| covers(lease, class)) {
        return Outcome::Deny(format!(
            "Blocked by {MCP_SERVER}: `{command}` is {name} work and lease {} (\"{}\") that \
             covers it is still queued. Keep waiting on it (acquire_lease with only lease_id \
             {}) until it is active, then retry. Do not acquire another lease for it.",
            queued.id.0, queued.label, queued.id.0
        ));
    }
    let mut reason = format!(
        "Blocked by {MCP_SERVER}: `{command}` is {name} work (rule `{rule}`) and this session \
         holds no active lease that covers it. Call acquire_lease from the {MCP_SERVER} MCP \
         server with class \"{name}\" and the command as label, run the command once the lease \
         is active, then call release_lease."
    );
    for lease in in_state(LeaseState::Active) {
        // Writing to a String cannot fail.
        let _ = write!(
            reason,
            " Active lease {} (\"{}\") does not cover {name} work.",
            lease.id.0, lease.label
        );
    }
    Outcome::Deny(reason)
}

/// Whether `lease` reserves what a command of `class` uses: cores for CPU work, GPU time or
/// VRAM for GPU work. A benchmark lease has the machine to itself and covers everything.
fn covers(lease: &Lease, class: JobClass) -> bool {
    if lease.kind == LeaseKind::BenchmarkExclusive {
        return true;
    }
    let (cpu, gpu) = match class {
        JobClass::Light => (false, false),
        JobClass::CpuHeavy => (true, false),
        JobClass::GpuHeavy => (false, true),
        _ => (true, true),
    };
    let profile = &lease.profile;
    (!cpu || profile.cores > 0.0) && (!gpu || profile.gpu_pct > 0.0 || profile.vram_mb > 0)
}

/// A warning that the check was skipped; the hook fails open on its own errors.
fn skipped(why: std::fmt::Arguments<'_>) -> Outcome {
    Outcome::Warn(format!("{MCP_SERVER}: lease check skipped, {why}"))
}

/// The hook's stdout for `outcome`; `None` for [`Outcome::Pass`].
fn render(outcome: &Outcome) -> Option<String> {
    let output = match outcome {
        Outcome::Pass => return None,
        Outcome::Warn(message) => HookOutput {
            hook_specific_output: None,
            system_message: Some(message),
        },
        Outcome::Deny(reason) => HookOutput {
            hook_specific_output: Some(PermissionOutput {
                hook_event_name: "PreToolUse",
                permission_decision: "deny",
                permission_decision_reason: reason,
            }),
            system_message: None,
        },
    };
    // Plain structs of strings always serialize.
    serde_json::to_string(&output).ok()
}

fn emit(outcome: &Outcome) -> ExitCode {
    if let Some(json) = render(outcome) {
        println!("{json}");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use rh_client::ClientError;
    use rh_shared::types::{Holder, JobProfile, LeaseId};
    use serde_json::Value;

    use super::*;

    const BASH_CARGO_BUILD: &str = include_str!("../fixtures/hook/bash_cargo_build.json");
    const BASH_LIGHT: &str = include_str!("../fixtures/hook/bash_light.json");
    const BASH_HELP: &str = include_str!("../fixtures/hook/bash_cargo_build_help.json");
    const BASH_TRAIN: &str = include_str!("../fixtures/hook/bash_python_train.json");
    const POWERSHELL_CARGO_TEST: &str = include_str!("../fixtures/hook/powershell_cargo_test.json");
    const READ_TOOL: &str = include_str!("../fixtures/hook/read_tool.json");
    const FIXTURE_SESSION: &str = "00000000-0000-4000-8000-000000000001";

    fn lease(id: i64, state: LeaseState, profile: JobProfile) -> Lease {
        Lease {
            id: LeaseId(id),
            holder: Holder {
                session: SessionKey(FIXTURE_SESSION.into()),
                pid: 4_242,
                pid_start_time: 1,
            },
            kind: LeaseKind::Normal,
            state,
            class: None,
            profile,
            label: format!("job {id}"),
            cwd: String::new(),
            requested_at_ms: 0,
            granted_at_ms: None,
            ended_at_ms: None,
            expires_at_ms: 60_000,
        }
    }

    fn cpu() -> JobProfile {
        JobProfile {
            cores: 8.0,
            ram_mb: 4_096,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: 300,
        }
    }

    fn gpu() -> JobProfile {
        JobProfile {
            cores: 0.0,
            ram_mb: 2_048,
            gpu_pct: 0.0,
            vram_mb: 6_144,
            est_secs: 600,
        }
    }

    fn with(leases: Vec<Lease>) -> impl FnOnce(&SessionKey) -> rh_client::Result<Vec<Lease>> {
        move |_| Ok(leases)
    }

    fn unreachable(_: &SessionKey) -> rh_client::Result<Vec<Lease>> {
        Err(ClientError::Unreachable {
            url: "http://127.0.0.1:47823".into(),
            reason: "connection refused".into(),
        })
    }

    fn never(_: &SessionKey) -> rh_client::Result<Vec<Lease>> {
        panic!("light commands must not reach the daemon");
    }

    fn denial(outcome: Outcome) -> String {
        match outcome {
            Outcome::Deny(reason) => reason,
            other => panic!("expected a deny, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_light_command_passes_without_daemon() {
        assert_eq!(
            evaluate(BASH_LIGHT, &Config::default(), never),
            Outcome::Pass
        );
    }

    #[test]
    fn test_evaluate_light_flag_passes_without_daemon() {
        assert_eq!(
            evaluate(BASH_HELP, &Config::default(), never),
            Outcome::Pass
        );
    }

    #[test]
    fn test_evaluate_other_tool_passes_without_daemon() {
        assert_eq!(
            evaluate(READ_TOOL, &Config::default(), never),
            Outcome::Pass
        );
    }

    #[test]
    fn test_evaluate_heavy_asks_daemon_for_the_payload_session() {
        let asked = Cell::new(None);
        evaluate(BASH_CARGO_BUILD, &Config::default(), |session| {
            asked.set(Some(session.0.clone()));
            Ok(vec![])
        });
        assert_eq!(asked.take().as_deref(), Some(FIXTURE_SESSION));
    }

    #[test]
    fn test_evaluate_heavy_without_lease_denies_with_acquire_guidance() {
        let reason = denial(evaluate(BASH_CARGO_BUILD, &Config::default(), with(vec![])));
        assert!(reason.contains("`cargo build --release`"), "{reason}");
        assert!(reason.contains("class \"cpu_heavy\""), "{reason}");
        assert!(
            reason.contains("cargo build|test|run|bench -> cpu_heavy"),
            "{reason}"
        );
        assert!(reason.contains("acquire_lease") && reason.contains("release_lease"));
    }

    #[test]
    fn test_evaluate_heavy_with_covering_active_lease_passes() {
        let leases = vec![lease(1, LeaseState::Active, cpu())];
        let outcome = evaluate(BASH_CARGO_BUILD, &Config::default(), with(leases));
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn test_evaluate_powershell_heavy_with_covering_lease_passes() {
        let leases = vec![lease(1, LeaseState::Active, cpu())];
        let outcome = evaluate(POWERSHELL_CARGO_TEST, &Config::default(), with(leases));
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn test_evaluate_powershell_heavy_without_lease_denies() {
        let outcome = evaluate(POWERSHELL_CARGO_TEST, &Config::default(), with(vec![]));
        assert!(denial(outcome).contains("`cargo test --workspace`"));
    }

    #[test]
    fn test_evaluate_ended_leases_do_not_cover() {
        let leases = vec![
            lease(1, LeaseState::Released, cpu()),
            lease(2, LeaseState::Expired, cpu()),
        ];
        denial(evaluate(BASH_CARGO_BUILD, &Config::default(), with(leases)));
    }

    #[test]
    fn test_evaluate_gpu_command_with_cpu_lease_denies_and_names_it() {
        let leases = vec![lease(3, LeaseState::Active, cpu())];
        let reason = denial(evaluate(BASH_TRAIN, &Config::default(), with(leases)));
        assert!(reason.contains("class \"gpu_heavy\""), "{reason}");
        assert!(reason.contains("Active lease 3 (\"job 3\") does not cover gpu_heavy"));
    }

    #[test]
    fn test_evaluate_custom_profile_with_vram_covers_gpu_command() {
        let leases = vec![lease(4, LeaseState::Active, gpu())];
        let outcome = evaluate(BASH_TRAIN, &Config::default(), with(leases));
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn test_evaluate_gpu_only_lease_does_not_cover_cpu_command() {
        let leases = vec![lease(4, LeaseState::Active, gpu())];
        denial(evaluate(BASH_CARGO_BUILD, &Config::default(), with(leases)));
    }

    #[test]
    fn test_evaluate_benchmark_lease_covers_everything() {
        let mut benchmark = lease(5, LeaseState::Active, gpu());
        benchmark.kind = LeaseKind::BenchmarkExclusive;
        benchmark.profile.vram_mb = 0;
        let outcome = evaluate(BASH_CARGO_BUILD, &Config::default(), with(vec![benchmark]));
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn test_evaluate_queued_covering_lease_says_wait() {
        let leases = vec![lease(6, LeaseState::Queued, cpu())];
        let reason = denial(evaluate(BASH_CARGO_BUILD, &Config::default(), with(leases)));
        assert!(reason.contains("lease 6 (\"job 6\")") && reason.contains("still queued"));
        assert!(
            reason.contains("acquire_lease with only lease_id 6"),
            "{reason}"
        );
        assert!(!reason.contains("Call acquire_lease"), "{reason}");
    }

    #[test]
    fn test_evaluate_daemon_down_fails_open_with_warning() {
        let outcome = evaluate(BASH_CARGO_BUILD, &Config::default(), unreachable);
        let Outcome::Warn(message) = outcome else {
            panic!("expected a warning, got {outcome:?}");
        };
        assert!(message.contains("rhd is unavailable") && message.contains("cargo build"));
    }

    #[test]
    fn test_evaluate_daemon_down_with_fail_open_off_denies() {
        let mut config = Config::default();
        config.hook.fail_open = false;
        let reason = denial(evaluate(BASH_CARGO_BUILD, &config, unreachable));
        assert!(reason.contains("rh daemon start"), "{reason}");
    }

    #[test]
    fn test_evaluate_malformed_input_warns() {
        let outcome = evaluate("not json", &Config::default(), never);
        assert!(matches!(outcome, Outcome::Warn(message) if message.contains("hook input")));
    }

    #[test]
    fn test_render_deny_uses_permission_decision() {
        let json: Value =
            serde_json::from_str(&render(&Outcome::Deny("why".into())).unwrap()).unwrap();
        let decision = &json["hookSpecificOutput"];
        assert_eq!(decision["hookEventName"], "PreToolUse");
        assert_eq!(decision["permissionDecision"], "deny");
        assert_eq!(decision["permissionDecisionReason"], "why");
        assert!(json.get("systemMessage").is_none());
    }

    #[test]
    fn test_render_warn_is_system_message_only() {
        let json: Value =
            serde_json::from_str(&render(&Outcome::Warn("hm".into())).unwrap()).unwrap();
        assert_eq!(json, serde_json::json!({ "systemMessage": "hm" }));
    }

    #[test]
    fn test_render_pass_prints_nothing() {
        assert_eq!(render(&Outcome::Pass), None);
    }
}
