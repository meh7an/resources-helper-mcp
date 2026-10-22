//! Runs the built `rh hook pre-tool` the way Claude Code does: payload on stdin, decision on
//! stdout, and always exit code 0.

use std::{
    env, fs,
    io::Write,
    net::{Ipv4Addr, TcpListener},
    path::PathBuf,
    process::{self, Command, Output, Stdio},
};

use serde_json::Value;

const BASH_CARGO_BUILD: &str = include_str!("../fixtures/hook/bash_cargo_build.json");
const BASH_LIGHT: &str = include_str!("../fixtures/hook/bash_light.json");

/// A home directory whose config points at a port nothing listens on and that has no token,
/// so the daemon is unreachable.
struct Home(PathBuf);

impl Home {
    fn new(name: &str, extra_config: &str) -> Self {
        let root = env::temp_dir().join(format!("rh-hook-{}-{name}", process::id()));
        let app = root.join(".resources-helper");
        fs::create_dir_all(&app).unwrap();
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = format!("[daemon]\nport = {port}\n{extra_config}");
        fs::write(app.join("config.toml"), config).unwrap();
        Self(root)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(home: &Home, args: &[&str], stdin: &str) -> Output {
    run_bytes(home, args, stdin.as_bytes())
}

fn run_bytes(home: &Home, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rh"))
        .args(args)
        .env("USERPROFILE", &home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    output
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn test_hook_light_command_prints_nothing() {
    let home = Home::new("light", "");
    let output = run(&home, &["hook", "pre-tool"], BASH_LIGHT);
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn test_hook_daemon_down_fails_open_with_system_message() {
    let home = Home::new("open", "");
    let json = stdout_json(&run(&home, &["hook", "pre-tool"], BASH_CARGO_BUILD));
    assert!(json.get("hookSpecificOutput").is_none(), "{json}");
    let message = json["systemMessage"].as_str().unwrap();
    assert!(message.contains("rhd is unavailable"), "{message}");
}

#[test]
fn test_hook_daemon_down_with_fail_open_off_denies() {
    let home = Home::new("closed", "[hook]\nfail_open = false\n");
    let json = stdout_json(&run(&home, &["hook", "pre-tool"], BASH_CARGO_BUILD));
    assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn test_hook_bad_arguments_warn_and_exit_zero() {
    let home = Home::new("args", "");
    let json = stdout_json(&run(
        &home,
        &["hook", "pre-tool", "--bogus"],
        BASH_CARGO_BUILD,
    ));
    let message = json["systemMessage"].as_str().unwrap();
    assert!(message.contains("hook command is wrong"), "{message}");
}

#[test]
fn test_hook_unknown_event_warns_and_exits_zero() {
    let home = Home::new("event", "");
    let json = stdout_json(&run(&home, &["hook", "post-tool"], BASH_CARGO_BUILD));
    assert!(json["systemMessage"].is_string(), "{json}");
}

#[test]
fn test_hook_garbage_input_warns_and_exits_zero() {
    let home = Home::new("garbage", "");
    let json = stdout_json(&run(&home, &["hook", "pre-tool"], "{\"tool_name\":"));
    assert!(json["systemMessage"].is_string(), "{json}");
}

#[test]
fn test_hook_unreadable_input_warns_and_exits_zero() {
    let home = Home::new("bytes", "");
    let mut huge = BASH_CARGO_BUILD.as_bytes().to_vec();
    huge.extend(std::iter::repeat_n(b' ', 4 << 20));
    for stdin in [
        &b"\xff\xfe{\"tool_name\":\"Bash\"}"[..],
        b"",
        b"\0\0\0",
        &huge,
    ] {
        let output = run_bytes(&home, &["hook", "pre-tool"], stdin);
        if !output.stdout.is_empty() {
            assert!(
                stdout_json(&output)["systemMessage"].is_string(),
                "{output:?}"
            );
        }
    }
}

#[test]
fn test_hook_invalid_config_warns_with_the_file_and_key() {
    let home = Home::new("config", "[leases]\nheartbeat_ttl_secs = 0\n");
    let json = stdout_json(&run(&home, &["hook", "pre-tool"], BASH_CARGO_BUILD));
    let message = json["systemMessage"].as_str().unwrap();
    assert!(message.contains("config.toml"), "{message}");
    assert!(message.contains("leases.heartbeat_ttl_secs"), "{message}");
}
