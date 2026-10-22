# Changelog

All notable changes to this project are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-23

First public release.

### Added

- `rhd`, a per-user daemon on `127.0.0.1` with bearer-token auth, a single-instance lock, and
  a SQLite ledger (Diesel, WAL, embedded migrations) of leases, their audit trail, and job
  history.
- A Windows sampler for CPU, RAM, GPU, and VRAM through PDH counters and DXGI, with per-process
  attribution, including short-lived build processes.
- Admission control with a configurable headroom, EASY backfill without starvation, heartbeat
  expiry, and reaping of leases whose session process has exited.
- `benchmark_exclusive` leases that drain the machine, wait for a quiet window (capped by
  `benchmark.max_quiet_wait_secs`), and record background noise on release.
- Learned job profiles: each released lease records what the job used, and later runs of the
  same command in the same folder are estimated from the p90 of that history.
- `rh-mcp`, an MCP server with `system_snapshot`, `can_fit`, `acquire_lease`, `release_lease`,
  and `list_leases`, which heartbeats its leases and releases them when the session ends.
- `rh hook pre-tool`, a PreToolUse hook for the Bash and PowerShell tools that denies heavy
  commands without a covering lease and fails open when rhd is down.
- A token-based heavy-command classifier with configurable rules, shown by `rh classes`.
- The `resource-guard` skill and a CLAUDE.md rule that tell sessions when and how to lease.
- The `rh` CLI: `status`, `leases`, `history`, `classes`, `daemon start|stop|status`,
  `install`, and `uninstall [--purge]`.
- Restart recovery of active and queued leases, log rotation, retention pruning, and config
  hot reload.
- A logon task for autostart, and a runbook for health checks, troubleshooting, and
  configuration.

[Unreleased]: https://github.com/meh7an/resources-helper-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/meh7an/resources-helper-mcp/releases/tag/v0.1.0
