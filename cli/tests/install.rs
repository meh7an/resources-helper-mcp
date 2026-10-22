//! Runs the built `rh install` and `rh uninstall` against a temporary `USERPROFILE`, including
//! over an `rh.exe` that is running.

use std::{
    env, fs,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
};

const SKILL: &str = include_str!("../../skill/resource-guard/SKILL.md");
const BINARIES: [&str; 3] = ["rh.exe", "rhd.exe", "rh-mcp.exe"];

/// A home directory with a build output to install from and a config that points at a port
/// nothing listens on, so no test reaches a real daemon.
struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Self {
        let root = env::temp_dir().join(format!("rh-install-{}-{name}", process::id()));
        let _ = fs::remove_dir_all(&root);
        let app = root.join(".resources-helper");
        fs::create_dir_all(&app).unwrap();
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        fs::write(
            app.join("config.toml"),
            format!("[daemon]\nport = {port}\n"),
        )
        .unwrap();

        // Only rh.exe has to run; the other two are never started by these tests.
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_rh"), build.join("rh.exe")).unwrap();
        fs::write(build.join("rhd.exe"), "rhd").unwrap();
        fs::write(build.join("rh-mcp.exe"), "rh-mcp").unwrap();
        Self(root)
    }

    fn app(&self) -> PathBuf {
        self.0.join(".resources-helper")
    }

    fn bin(&self) -> PathBuf {
        self.app().join("bin")
    }

    fn build(&self) -> PathBuf {
        self.0.join("build")
    }

    fn skill(&self) -> PathBuf {
        self.0.join(r".claude\skills\resource-guard\SKILL.md")
    }

    fn rh(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rh"))
            .args(args)
            .env("USERPROFILE", &self.0)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn install(&self) -> String {
        let build = self.build();
        let output = self.rh(&["install", "--from", build.to_str().unwrap()]);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// Starts the installed `rh.exe` as a hook that waits for its stdin, so the file stays in
    /// use until [`finish`] closes it.
    fn run_installed_rh(&self) -> Child {
        Command::new(self.bin().join("rh.exe"))
            .args(["hook", "pre-tool"])
            .env("USERPROFILE", &self.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn finish(mut child: Child) {
    drop(child.stdin.take());
    child.wait().unwrap();
}

fn old_copies(bin: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(bin) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".old-"))
        .collect()
}

#[test]
fn test_install_copies_binaries_skill_and_rule_and_prints_the_steps() {
    let home = Home::new("fresh");
    let stdout = home.install();

    for name in BINARIES {
        let installed = fs::read(home.bin().join(name)).unwrap();
        assert_eq!(
            installed,
            fs::read(home.build().join(name)).unwrap(),
            "{name}"
        );
    }
    assert_eq!(fs::read_to_string(home.skill()).unwrap(), SKILL);
    assert!(home.app().join("claude-md-rule.md").is_file());
    assert!(stdout.contains("rhd is not running"), "{stdout}");
    assert!(stdout.contains("1. [todo] MCP server"), "{stdout}");
    assert!(
        stdout.contains("claude mcp add --scope user resources-helper -- \""),
        "{stdout}"
    );
    assert!(stdout.contains("2. [todo] PreToolUse hook"), "{stdout}");
    assert!(stdout.contains("\"args\": ["), "{stdout}");
    assert!(stdout.contains("3. [todo] CLAUDE.md rule"), "{stdout}");
    // The machine running the tests may already have the real logon task.
    let task_step = stdout.contains("Register-ScheduledTask -TaskName 'resources-helper'")
        || stdout.contains("4. [done] Autostart");
    assert!(task_step, "{stdout}");
}

#[test]
fn test_install_marks_registered_steps_done() {
    let home = Home::new("registered");
    let bin = home.bin();
    let mcp = bin.join("rh-mcp.exe");
    let claude_json = serde_json::json!({
        "mcpServers": { "resources-helper": { "type": "stdio", "command": mcp, "args": [] } },
    });
    fs::write(home.0.join(".claude.json"), claude_json.to_string()).unwrap();
    let claude = home.0.join(".claude");
    fs::create_dir_all(&claude).unwrap();
    let hook = bin.join("rh.exe").to_string_lossy().replace('\\', "/");
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash|PowerShell",
                "hooks": [{ "type": "command", "command": hook, "args": ["hook", "pre-tool"] }],
            }],
        },
    });
    fs::write(claude.join("settings.json"), settings.to_string()).unwrap();
    // The rule's text under a different heading, as in a numbered contract.
    let rule = include_str!("../../skill/claude-md-rule.md")
        .split("```")
        .nth(1)
        .unwrap()
        .trim_start_matches("markdown")
        .replace(
            "## Shared Machine Resources",
            "## 11 — Shared Machine Resources",
        );
    fs::write(claude.join("CLAUDE.md"), format!("# Rules\n{rule}")).unwrap();

    let stdout = home.install();

    assert!(stdout.contains("1. [done] MCP server"), "{stdout}");
    assert!(stdout.contains("2. [done] PreToolUse hook"), "{stdout}");
    assert!(stdout.contains("3. [done] CLAUDE.md rule"), "{stdout}");
    assert!(!stdout.contains("claude mcp add"), "{stdout}");
}

#[test]
fn test_reinstall_over_a_running_rh_sets_the_old_copy_aside_until_it_exits() {
    let home = Home::new("running");
    home.install();
    let running = home.run_installed_rh();

    let stdout = home.install();

    assert!(stdout.contains("still in use"), "{stdout}");
    let old = old_copies(&home.bin());
    assert_eq!(old.len(), 1, "{old:?}");
    assert!(old[0].starts_with("rh.old-"), "{old:?}");
    assert!(home.bin().join("rh.exe").is_file());

    finish(running);
    let stdout = home.install();
    assert!(!stdout.contains("still in use"), "{stdout}");
    assert!(old_copies(&home.bin()).is_empty());
}

#[test]
fn test_install_from_the_bin_dir_or_without_a_binary_is_refused() {
    let home = Home::new("refused");
    home.install();

    let bin = home.bin();
    let output = home.rh(&["install", "--from", bin.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("is the install itself"));

    fs::remove_file(home.build().join("rh-mcp.exe")).unwrap();
    let build = home.build();
    let output = home.rh(&["install", "--from", build.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("has no rh-mcp.exe"));
}

#[test]
fn test_uninstall_keeps_the_data_and_purge_removes_it() {
    let home = Home::new("uninstall");
    home.install();
    fs::write(home.app().join("rh.db"), "db").unwrap();
    fs::write(home.app().join("token"), "token").unwrap();

    let output = home.rh(&["uninstall"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!home.bin().exists());
    assert!(!home.skill().exists());
    assert!(!home.skill().parent().unwrap().exists());
    assert!(!home.app().join("claude-md-rule.md").exists());
    for kept in ["rh.db", "token", "config.toml"] {
        assert!(home.app().join(kept).is_file(), "{kept}");
    }

    let output = home.rh(&["uninstall", "--purge"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!home.app().exists());
}

#[test]
fn test_uninstall_sets_a_running_rh_aside() {
    let home = Home::new("uninstall-running");
    home.install();
    let running = home.run_installed_rh();

    let output = home.rh(&["uninstall"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("still in use"), "{stdout}");
    assert!(!home.bin().join("rh.exe").exists());
    assert!(!home.bin().join("rhd.exe").exists());
    assert_eq!(old_copies(&home.bin()).len(), 1);
    finish(running);
}
