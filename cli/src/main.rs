use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};

mod classes;
mod daemon;
mod history;
mod hook;
mod install;
mod probe;
mod registration;
mod sample;
mod status;

/// Inspect machine usage and leases, control the daemon, and run the Claude Code hook.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show machine usage, sessions, and their leases. Starts rhd when it is not running.
    Status,
    /// List active and queued leases.
    Leases {
        /// Show the most recent leases in any state instead.
        #[arg(long)]
        all: bool,
    },
    /// Print the heavy-command rules and class profiles in effect, as Markdown.
    Classes,
    /// Show past jobs and what they needed, or the runs of one job.
    History {
        /// A fingerprint as `rh history` prints it (quote it: it contains `|`), to list that
        /// job's runs.
        fingerprint: Option<String>,
        /// Most runs to list (default 20).
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Start, stop, or check the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Record this process's ancestry, environment, and optionally stdin to a file, or print
    /// live machine samples with `--sample`.
    Probe(probe::ProbeArgs),
    /// Copy the built binaries into %USERPROFILE%\.resources-helper\bin, restart rhd on them
    /// when it runs, copy the resource-guard skill, and print the remaining setup steps.
    Install {
        /// Directory holding the built rh.exe, rhd.exe, and rh-mcp.exe (default: this
        /// executable's directory, such as target\release).
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
    },
    /// Stop rhd, remove the binaries and the skill copy, and print the steps that unregister
    /// the rest. Keeps the database, token, config, and logs.
    Uninstall {
        /// Also delete the database, token, config, and logs.
        #[arg(long)]
        purge: bool,
    },
    /// Claude Code hooks. They always exit 0 and report a deny in their JSON output.
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },
}

#[derive(Debug, Subcommand)]
enum HookEvent {
    /// PreToolUse for the Bash and PowerShell tools: reads the hook payload from stdin and
    /// denies heavy commands without a covering lease.
    PreTool,
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start rhd in the background unless it is already running.
    Start,
    /// Ask rhd to shut down and wait until it has.
    Stop,
    /// Report whether rhd is running, without starting it.
    Status,
}

fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        // `--help` and `--version` exit 0 and print as usual.
        Err(error) if hook::invoked() && error.exit_code() != 0 => {
            return hook::argument_error(&error);
        }
        Err(error) => error.exit(),
    };
    let result = match args.command {
        Command::Status => status::status(),
        Command::Leases { all } => status::leases(all),
        Command::Classes => classes::classes(),
        Command::History { fingerprint, limit } => history::history(fingerprint.as_deref(), limit),
        Command::Daemon { action } => match action {
            DaemonAction::Start => daemon::start(),
            DaemonAction::Stop => daemon::stop(),
            DaemonAction::Status => daemon::status(),
        },
        Command::Install { from } => install::install(from.as_deref()),
        Command::Uninstall { purge } => install::uninstall(purge),
        Command::Probe(args) => probe::run(&args),
        Command::Hook {
            event: HookEvent::PreTool,
        } => return hook::pre_tool(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            ExitCode::FAILURE
        }
    }
}
