//! The parts of the setup that live in Claude Code's and Windows' own configuration: the MCP
//! server, the PreToolUse hook, the CLAUDE.md rule, and the logon task. `rh install` and
//! `rh uninstall` print the commands for them, because Claude Code refuses edits to its own
//! settings from a session, and look up which ones are already in place.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use rh_shared::paths::Paths;
use serde_json::{Value, json};

/// The name the MCP server is registered under.
const MCP_NAME: &str = "resources-helper";
/// The name of the scheduled task that starts rhd at logon.
const TASK_NAME: &str = "resources-helper";
/// The heading of the section `skill/claude-md-rule.md` adds to the global CLAUDE.md.
const RULE_HEADING: &str = "## Shared Machine Resources";
const HOOK_ARGS: [&str; 2] = ["hook", "pre-tool"];

/// Whether a step is already done, as far as rh can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Done,
    Missing,
    /// The file or command that would tell could not be read.
    Unknown,
}

impl State {
    fn from_lookup(found: Option<bool>) -> Self {
        match found {
            Some(true) => Self::Done,
            Some(false) => Self::Missing,
            None => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Done => "[done]",
            Self::Missing => "[todo]",
            Self::Unknown => "[check]",
        }
    }
}

/// The per-user `resource-guard` skill directory Claude Code loads skills from.
pub fn skill_dir(paths: &Paths) -> PathBuf {
    claude_dir(paths).join("skills").join("resource-guard")
}

fn claude_dir(paths: &Paths) -> PathBuf {
    paths.home().join(".claude")
}

fn claude_json(paths: &Paths) -> PathBuf {
    paths.home().join(".claude.json")
}

fn settings_json(paths: &Paths) -> PathBuf {
    claude_dir(paths).join("settings.json")
}

fn claude_md(paths: &Paths) -> PathBuf {
    claude_dir(paths).join("CLAUDE.md")
}

fn rh_exe(paths: &Paths) -> PathBuf {
    paths.bin_dir().join("rh.exe")
}

fn rh_mcp_exe(paths: &Paths) -> PathBuf {
    paths.bin_dir().join("rh-mcp.exe")
}

fn rhd_exe(paths: &Paths) -> PathBuf {
    paths.bin_dir().join("rhd.exe")
}

/// Prints the setup steps rh does not do itself, with what is already done marked.
pub fn print_install_steps(paths: &Paths, rule_copy: &Path) {
    println!();
    println!("Remaining steps. Run them yourself; [done] ones are already set up.");

    if step(1, mcp_state(paths), "MCP server, user scope") {
        println!(
            "     claude mcp add --scope user {MCP_NAME} -- \"{}\"",
            rh_mcp_exe(paths).display()
        );
        println!("   Sessions load it when they start; restart open sessions to get it.");
    }

    let settings = format!("PreToolUse hook in {}", settings_json(paths).display());
    if step(2, hook_state(paths), &settings) {
        println!("   Add this entry to the \"hooks\".\"PreToolUse\" array (it loads live):");
        for line in hook_entry(paths).lines() {
            println!("     {line}");
        }
    }

    let rule = format!("CLAUDE.md rule in {}", claude_md(paths).display());
    if step(3, rule_state(paths), &rule) {
        println!(
            "   Paste the section from {} between sessions.",
            rule_copy.display()
        );
    }

    let task = format!("Autostart at logon, scheduled task \"{TASK_NAME}\"");
    if step(4, task_state(), &task) {
        println!("   In PowerShell (no admin needed):");
        for line in register_task_script(paths).lines() {
            println!("     {line}");
        }
    }
}

/// Prints a step's heading and returns whether its instructions follow.
fn step(number: u8, state: State, title: &str) -> bool {
    println!();
    if state == State::Done {
        println!("{number}. {} {title}", state.label());
        false
    } else {
        println!("{number}. {} {title}:", state.label());
        true
    }
}

/// Prints the steps that undo the registrations rh cannot remove itself, skipping those
/// that are already gone.
pub fn print_uninstall_steps(paths: &Paths) {
    let mut steps = Vec::new();
    if mcp_state(paths) != State::Missing {
        steps.push(format!("claude mcp remove --scope user {MCP_NAME}"));
    }
    if hook_state(paths) != State::Missing {
        steps.push(format!(
            "Remove the PreToolUse entry that runs {} from {}",
            hook_command(paths),
            settings_json(paths).display()
        ));
    }
    if rule_state(paths) != State::Missing {
        steps.push(format!(
            "Remove the \"{RULE_HEADING}\" section from {}",
            claude_md(paths).display()
        ));
    }
    if task_state() != State::Missing {
        steps.push(format!(
            "In PowerShell: Unregister-ScheduledTask -TaskName '{TASK_NAME}' -Confirm:$false"
        ));
    }
    if steps.is_empty() {
        return;
    }
    println!();
    println!("Remaining steps. Run them yourself:");
    for (number, step) in steps.iter().enumerate() {
        println!("{}. {step}", number + 1);
    }
}

fn mcp_state(paths: &Paths) -> State {
    let found = read_json(&claude_json(paths))
        .map(|config| config.is_some_and(|config| mcp_registered(&config, &rh_mcp_exe(paths))));
    State::from_lookup(found)
}

fn hook_state(paths: &Paths) -> State {
    let found = read_json(&settings_json(paths)).map(|settings| {
        settings.is_some_and(|settings| hook_registered(&settings, &rh_exe(paths)))
    });
    State::from_lookup(found)
}

fn rule_state(paths: &Paths) -> State {
    let found = match fs::read_to_string(claude_md(paths)) {
        Ok(text) => Some(text.contains(RULE_HEADING)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    State::from_lookup(found)
}

/// Asks Task Scheduler for the task; any failure to run `schtasks` is [`State::Unknown`].
fn task_state() -> State {
    let status = Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    State::from_lookup(status.ok().map(|status| status.success()))
}

/// The parsed file, `Ok(None)` when it does not exist, and `None` when it cannot be read or
/// parsed.
fn read_json(path: &Path) -> Option<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).ok().map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

/// Whether `~/.claude.json` registers the MCP server, at user scope, to run `exe`.
fn mcp_registered(config: &Value, exe: &Path) -> bool {
    config["mcpServers"][MCP_NAME]["command"]
        .as_str()
        .is_some_and(|command| same_path(command, exe))
}

/// Whether `settings.json` has a PreToolUse hook that runs `exe hook pre-tool`.
fn hook_registered(settings: &Value, exe: &Path) -> bool {
    let Some(entries) = settings["hooks"]["PreToolUse"].as_array() else {
        return false;
    };
    entries
        .iter()
        .filter_map(|entry| entry["hooks"].as_array())
        .flatten()
        .any(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| same_path(command, exe))
                && hook["args"]
                    .as_array()
                    .is_some_and(|args| args.iter().map(Value::as_str).eq(HOOK_ARGS.map(Some)))
        })
}

/// Windows paths compare without case and with either separator.
fn same_path(text: &str, path: &Path) -> bool {
    let normalize = |text: &str| text.trim_matches('"').replace('/', "\\").to_lowercase();
    normalize(text) == normalize(&path.to_string_lossy())
}

/// The hook command with forward slashes, the form Claude Code's settings examples use.
fn hook_command(paths: &Paths) -> String {
    rh_exe(paths).to_string_lossy().replace('\\', "/")
}

/// The PreToolUse entry for `settings.json`, as pretty JSON. Exec form (`args`, no shell),
/// so the hook's parent is the session's `claude.exe`.
fn hook_entry(paths: &Paths) -> String {
    let entry = json!({
        "matcher": "Bash|PowerShell",
        "hooks": [{
            "type": "command",
            "command": hook_command(paths),
            "args": HOOK_ARGS,
            "timeout": 10,
        }],
    });
    serde_json::to_string_pretty(&entry).unwrap_or_else(|_| entry.to_string())
}

/// PowerShell that registers the logon task for the current user. Task Scheduler's defaults
/// would stop rhd after 72 hours and refuse to start it on battery.
fn register_task_script(paths: &Paths) -> String {
    let exe = rhd_exe(paths).to_string_lossy().replace('\'', "''");
    format!(
        "$action = New-ScheduledTaskAction -Execute '{exe}'\n\
         $trigger = New-ScheduledTaskTrigger -AtLogOn -User \"$env:USERDOMAIN\\$env:USERNAME\"\n\
         $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) \
         -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew\n\
         Register-ScheduledTask -TaskName '{TASK_NAME}' -Action $action -Trigger $trigger \
         -Settings $settings -Description 'Starts the resources-helper daemon (rhd) at logon'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::under(r"C:\Users\someone")
    }

    #[test]
    fn test_mcp_registered_matches_path_without_case_or_separator() {
        let config = json!({
            "mcpServers": {
                "resources-helper": {
                    "type": "stdio",
                    "command": "c:/users/SOMEONE/.resources-helper/bin/rh-mcp.exe",
                    "args": [],
                },
            },
        });
        assert!(mcp_registered(&config, &rh_mcp_exe(&paths())));
    }

    #[test]
    fn test_mcp_registered_other_path_or_project_scope_is_missing() {
        let elsewhere = json!({
            "mcpServers": { "resources-helper": { "command": "E:/target/debug/rh-mcp.exe" } },
        });
        let project_only = json!({
            "projects": {
                "E:/repo": {
                    "mcpServers": {
                        "resources-helper": {
                            "command": r"C:\Users\someone\.resources-helper\bin\rh-mcp.exe",
                        },
                    },
                },
            },
        });
        for config in [elsewhere, project_only, json!({})] {
            assert!(!mcp_registered(&config, &rh_mcp_exe(&paths())), "{config}");
        }
    }

    #[test]
    fn test_hook_registered_finds_the_entry_beside_other_hooks() {
        let settings = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "rtk hook" }] },
                    serde_json::from_str::<Value>(&hook_entry(&paths())).unwrap(),
                ],
            },
        });
        assert!(hook_registered(&settings, &rh_exe(&paths())));
    }

    #[test]
    fn test_hook_registered_needs_the_pre_tool_args() {
        let settings = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "command": hook_command(&paths()), "args": ["probe"] }],
                }],
            },
        });
        assert!(!hook_registered(&settings, &rh_exe(&paths())));
        assert!(!hook_registered(&json!({ "hooks": {} }), &rh_exe(&paths())));
    }

    #[test]
    fn test_hook_entry_uses_forward_slashes_and_exec_form() {
        let entry: Value = serde_json::from_str(&hook_entry(&paths())).unwrap();
        let hook = &entry["hooks"][0];
        assert_eq!(
            hook["command"],
            "C:/Users/someone/.resources-helper/bin/rh.exe"
        );
        assert_eq!(hook["args"], json!(["hook", "pre-tool"]));
        assert_eq!(entry["matcher"], "Bash|PowerShell");
    }

    #[test]
    fn test_register_task_script_disables_the_time_limit_and_quotes_the_path() {
        let script = register_task_script(&Paths::under(r"C:\Users\o'brien"));
        assert!(script.contains(r"-Execute 'C:\Users\o''brien\.resources-helper\bin\rhd.exe'"));
        assert!(script.contains("-ExecutionTimeLimit ([TimeSpan]::Zero)"));
        assert!(script.contains("-AtLogOn -User \"$env:USERDOMAIN\\$env:USERNAME\""));
        assert_eq!(script.lines().count(), 4);
    }
}
