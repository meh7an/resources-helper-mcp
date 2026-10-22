# resources-helper runbook

Paths below are relative to `%USERPROFILE%\.resources-helper\` unless they start with `~` or a
drive. `rh` means `bin\rh.exe`.

## Is it working?

| Check | Healthy result |
|-------|----------------|
| `rh daemon status` | `rhd <version> running at http://127.0.0.1:47823 (pid ..., up ... s)` |
| `rh status` | Machine usage and one block per session. A `config REJECTED` line means rhd kept its previous config (see below). |
| `claude mcp get resources-helper` | `Connected`, command `...\.resources-helper\bin\rh-mcp.exe` |
| `Get-ScheduledTask resources-helper` | State `Ready` (rhd already started) or `Running` |
| `Get-ScheduledTaskInfo resources-helper` | `LastTaskResult` 267009 (still running). 1 means rhd exited at once: usually another rhd was already running, which is not logged (it fails before logging starts, so two daemons never share the log); other startup errors are in `logs\rhd.log`. |
| `logs\rhd.log` | A startup line, then lease grants and releases |

`rhd.exe` is a windowless program: run from a terminal it prints nothing, not even
`--help`. Use `rh daemon status` for its version and `logs\rhd.log` for its errors.

## Who starts rhd

- The logon task `resources-helper` starts it when you log on.
- `rh-mcp` (on a tool call or a heartbeat) and `rh status` start it when it is down, using
  the `rhd.exe` beside their own executable.
- The hook never starts it, so a heavy command never waits on a daemon start.
- `rh daemon start` starts it by hand; `rh daemon stop` stops it gracefully.

Only one rhd runs per user: a second one finds `rhd.lock` taken and exits. Stopping rhd while
sessions are open is temporary: a session holding a lease starts it again on its next
heartbeat (within about 20 s), and any session does on its next tool call.

## Fail-open behavior

The hook asks rhd for the session's leases with a 1 s budget. When that fails (rhd down,
starting, or answering with an error):

- `hook.fail_open = true` (default): the command runs, and the session shows the warning
  "rhd is unavailable, `<command>` runs unchecked: <reason>". Nothing is coordinated until
  rhd is back.
- `hook.fail_open = false`: the command is denied, and Claude is told to ask you to run
  `rh daemon start`.

The hook also never blocks on its own failures: unreadable input, an invalid config, or
wrong arguments produce a warning and let the command through. It always exits 0, so a
broken `rh.exe` cannot block every shell call.

## Troubleshooting

| Symptom | Cause and fix |
|---------|---------------|
| A session's heavy command is "Blocked by resources-helper", but the session has no `acquire_lease` tool | The session started before the MCP server was registered, or with `--strict-mcp-config`. Start a new session. |
| Every heavy command runs with the "runs unchecked" warning | rhd is down. `rh daemon status`, then read the end of `logs\rhd.log`. |
| rhd exits right after starting | `logs\rhd.log` says why. "another rhd is already running": one is up already. "binding 127.0.0.1:<port>": another program has the port, so set `[daemon] port` in `config.toml`. A config error names the file and key. |
| `config REJECTED` in `rh status` | `config.toml` has an invalid value; rhd keeps the previous config until the file is valid again (checked every 5 s). The message names the key. |
| Nothing can reach rhd after a port change | rhd keeps its old port until it restarts, but clients read the new one at once, `rh daemon stop` included. End `rhd.exe` in Task Manager, then run `rh daemon start`. |
| 401 errors | The token in `token` does not match rhd's. Clients re-read the file once. If it persists, restart rhd: it reads `token` at startup. |
| A lease stays queued | `rh leases` shows what is ahead. A `benchmark_exclusive` lease drains the machine and then waits up to `benchmark.max_quiet_wait_secs` for quiet. Leases of sessions that ended expire within a second (holder process gone) or after the heartbeat TTL (60 s). |
| `rh install` lists files "still in use" | Old binaries that running sessions or rhd still hold. They are harmless and are deleted by the next install once nothing uses them. |
| Copying a build over `bin\rh-mcp.exe` fails | Running sessions hold it open. Use `rh install`, which renames the old file aside first. |
| Registering the logon task fails with "Access is denied" | Fall back to the per-user Run key, which also needs no admin: `New-ItemProperty -Path HKCU:\Software\Microsoft\Windows\CurrentVersion\Run -Name resources-helper -Value '"C:\Users\<you>\.resources-helper\bin\rhd.exe"'`. |
| Terminal sessions see a different daemon, token, or database than desktop sessions | Something wrote under `%LOCALAPPDATA%` from inside the Claude desktop app, which MSIX redirects to `%LOCALAPPDATA%\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\`. Only `%USERPROFILE%\.resources-helper\` is shared. Stale copies in the package folder are safe to delete. |

## Shutdown and crashes

A stop (`rh daemon stop`, `rh install`, `rh uninstall`, or Ctrl+C) answers every open acquire
wait at once: the lease stays queued, and the verdict tells the session to resume it with
`lease_id` once rhd is back. The stop then takes about a second.

Windows ends rhd without a graceful shutdown at logoff and shutdown, and a crash does the
same. Nothing is lost: SQLite runs in WAL mode, and on the next start rhd restores the queued
and active leases it had, keeping their grant times. Leases whose holders are gone expire on
the first sample. A run that spans a restart records no job history.

## Configuration

`config.toml` is optional; each table overrides the defaults field by field, and unknown keys
are rejected. `rh classes` prints the classifier rules and class profiles in effect.

| Key | Default | Meaning |
|-----|---------|---------|
| `daemon.port` | 47823 | Port on 127.0.0.1. A change needs an rhd restart. |
| `admission.headroom_pct` | 15 | Capacity kept free of leases. |
| `benchmark.quiet_cpu_pct`, `quiet_gpu_pct` | 10, 10 | Load a benchmark waits under. |
| `benchmark.quiet_secs` | 10 | How long the machine must stay quiet. |
| `benchmark.max_quiet_wait_secs` | 60 | After this, a benchmark is granted without quiet. |
| `leases.heartbeat_ttl_secs` | 60 | A lease without a heartbeat for this long expires. |
| `retention.lease_days`, `history_days` | 30, 90 | Age at which ended leases and job runs are pruned (hourly). |
| `retention.keep_runs_per_job`, `max_job_runs` | 20, 5000 | Runs kept per job regardless of age, and in total. |
| `hook.fail_open` | true | See Fail-open behavior. |
| `[classes.<class>]` | built in | Profiles of `light`, `cpu_heavy`, `gpu_heavy`, `mixed`. |
| `[[classifier.rules]]` | built in | Heavy-command rules. A list here replaces the default list. |

## Resetting the database

This deletes all leases and job history; learned profiles start over.

1. `rh daemon stop` (and close the sessions that hold leases, or their `rh-mcp` starts rhd
   again).
2. Delete `rh.db`, `rh.db-wal`, and `rh.db-shm`.
3. `rh daemon start`. rhd creates the database and applies the migrations.

Sessions that still hold leases get "unknown lease" on their next heartbeat and drop them.

To reset the token, stop rhd, delete `token`, and start rhd; it writes a new one, and clients
read it on their next request.

## Logs

`logs\rhd.log` rotates to `rhd.1.log` ... `rhd.4.log` at 10 MB. `rh-mcp` writes its
diagnostics to stderr, which Claude Code captures with its other MCP server output.
