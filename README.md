# resources-helper

Coordinates CPU and GPU heavy work between the Claude Code sessions on one Windows PC. Sessions
ask a per-user daemon for a lease before a build, test suite, benchmark, or training run, and
wait their turn when the machine is busy, instead of all compiling at once.

| Part | What it does |
|------|--------------|
| `rhd` | The daemon: samples the machine every second, keeps the lease ledger in SQLite, and serves an HTTP API on `127.0.0.1` with a bearer token. No window; it logs to a file. |
| `rh-mcp` | The MCP server each session starts (`resources-helper`): `system_snapshot`, `can_fit`, `acquire_lease`, `release_lease`, `list_leases`. It heartbeats its leases and releases them when the session ends. |
| `rh hook pre-tool` | A PreToolUse hook for the Bash and PowerShell tools. It denies a heavy command when the session holds no covering lease, and lets it through with a warning when rhd is down. |
| `resource-guard` skill | Tells Claude when and how to lease. A CLAUDE.md rule points sessions to it. |
| `rh` | The CLI: status, leases, history, daemon control, install. |

Windows only. Everything lives in `%USERPROFILE%\.resources-helper\`, not under `AppData`,
because the Claude desktop app is MSIX-packaged and Windows redirects its writes to `AppData`
into a folder that terminal sessions cannot see.

## Install

Needs Rust (stable, MSVC toolchain) and Claude Code.

1. Build the release binaries from the workspace root. Inside a Claude session this is a
   `cpu_heavy` lease like any other build.

   ```powershell
   cargo build --release
   ```

2. Install them:

   ```powershell
   target\release\rh.exe install
   ```

   This copies `rh.exe`, `rhd.exe`, and `rh-mcp.exe` to `%USERPROFILE%\.resources-helper\bin`,
   restarts rhd on the new build when it is running, copies the `resource-guard` skill to
   `~/.claude/skills/resource-guard/`, and writes the CLAUDE.md rule to
   `%USERPROFILE%\.resources-helper\claude-md-rule.md`.

3. Do the steps it prints. Each is marked `[done]` once rh finds it in place:
   1. Register the MCP server: `claude mcp add --scope user resources-helper -- "<bin>\rh-mcp.exe"`.
   2. Add the printed PreToolUse entry to `~/.claude/settings.json`. Claude Code refuses this
      edit from inside a session, so make it yourself.
   3. Paste the section from `claude-md-rule.md` into `~/.claude/CLAUDE.md` between sessions.
   4. Run the printed PowerShell lines to register the `resources-helper` logon task. It
      starts rhd when you log on, needs no admin, and has no time limit.

4. Start a new Claude session (sessions load MCP servers only when they start), then check:

   ```powershell
   ~\.resources-helper\bin\rh.exe status
   claude mcp get resources-helper
   ```

Running `rh install` again is safe; it only overwrites its own files. To type `rh` anywhere,
add `%USERPROFILE%\.resources-helper\bin` to your user `PATH` (optional; the examples below
assume it).

## Upgrade

Pull, then repeat steps 1 and 2. Binaries that running sessions hold open cannot be
overwritten, so `rh install` renames each old file aside (`rh-mcp.old-<millis>.exe`) and copies
the new one in. Old copies that are still in use are listed and deleted by the next install.
rhd restarts on the new build and restores its active and queued leases. Open sessions keep
their old `rh-mcp` until they restart. The hook picks up the new `rh.exe` at once.

## Uninstall

1. Remove the PreToolUse entry from `~/.claude/settings.json` first, so sessions stop calling
   an `rh.exe` that is about to go away.
2. Run the uninstall from the build output, not from the bin directory (a running exe cannot
   delete itself):

   ```powershell
   target\release\rh.exe uninstall
   ```

   It stops rhd, removes the binaries, the skill copy, and the rule copy, and prints what is
   still registered: the MCP server, the hook, the CLAUDE.md section, and the logon task. The
   database, token, config, and logs stay. `rh uninstall --purge` deletes them too.

## Everyday commands

| Command | Shows or does |
|---------|---------------|
| `rh status` | Machine usage, sessions, and their leases. Starts rhd if it is down. |
| `rh leases [--all]` | Active and queued leases, or the most recent ones in any state. |
| `rh history [FINGERPRINT]` | What past jobs needed, or the runs of one job. |
| `rh classes` | The heavy-command rules and class profiles in effect. |
| `rh daemon start\|stop\|status` | Control rhd. `status` never starts it. |

## Files

All under `%USERPROFILE%\.resources-helper\`:

| Path | Contents |
|------|----------|
| `bin\` | `rh.exe`, `rhd.exe`, `rh-mcp.exe` |
| `config.toml` | Optional overrides of the defaults. rhd reloads it within 5 s. |
| `rh.db` | Leases, their audit trail, and job history (SQLite, WAL). |
| `token` | The bearer token clients send to rhd. |
| `logs\rhd.log` | The daemon log, rotated at 10 MB, five files kept. |
| `claude-md-rule.md` | The CLAUDE.md section, written by `rh install`. |

Troubleshooting, fail-open behavior, configuration, and resetting the database are in
[docs/runbook.md](docs/runbook.md).
