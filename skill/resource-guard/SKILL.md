---
name: resource-guard
description: >
  Coordinates heavy shell commands with the other Claude sessions on this machine through the
  resources-helper MCP tools (system_snapshot, can_fit, acquire_lease, release_lease,
  list_leases). Use before running any build, test suite, benchmark, training run, render, or
  other command that loads the CPU or GPU for more than a few seconds; whenever a tool call is
  "Blocked by resources-helper"; when an acquire_lease result says "queued"; and when the user
  asks for timing or performance numbers or about machine load.
---

# Resource Guard

Several Claude sessions share this PC. A per-user daemon (`rhd`) keeps a ledger of *leases*:
reservations of cores, RAM, GPU and VRAM for heavy jobs. A session runs a heavy command only
while it holds an active lease that covers it. A PreToolUse hook enforces this for the Bash
and PowerShell tools, but the hook only knows the rules below; the policy also covers heavy
work it cannot recognize.

## What counts as heavy

The hook classifies commands with these rules. They come from the default config; the user's
`%USERPROFILE%\.resources-helper\config.toml` can replace them, so run `rh classes` (light,
no lease needed) when the hook's verdict surprises you or before relying on this list.

<!-- rh classes: begin -->
A command line is heavy when any command in it (after `&&`, `;`, `|`, env prefixes, and wrappers such as `npx` or `bash -c`) matches a rule:

- `cargo build|test|run|bench -> cpu_heavy`
- `pnpm build|test -> cpu_heavy`
- `msbuild (any arguments) -> cpu_heavy`
- `cmake --build -> cpu_heavy`
- `python* train*.py -> gpu_heavy`
- `py train*.py -> gpu_heavy`

A command with any of `--help` `-h` `--version` `-V` `/?` is light whatever it runs.

| Class | Cores | RAM MB | GPU % | VRAM MB | Est. secs |
|-------|------:|-------:|------:|--------:|----------:|
| light | 1 | 512 | 0 | 0 | 30 |
| cpu_heavy | 8 | 4096 | 0 | 0 | 300 |
| gpu_heavy | 2 | 4096 | 80 | 4096 | 600 |
| mixed | 6 | 8192 | 50 | 2048 | 600 |
<!-- rh classes: end -->

Matching is case-insensitive and `*` is a wildcard. An argument pattern starting with `-`
matches that flag anywhere; any other pattern matches the first positional argument, and
`npm|pnpm|yarn|bun run X` reads as `X`. A line with both CPU and GPU parts is `mixed`.

Heavy work the rules miss still needs a lease. Use your judgment for anything that keeps
several cores or the GPU busy for more than about 30 seconds: `ninja`, `make -j`, `go build`
of a large module, `docker build`, `ffmpeg` encodes, Blender or other renders, ML inference
servers, game or app executables, large `pytest` runs. Pick the closest class, or give a
custom profile (`cores` and `ram_mb`, plus `gpu_pct`, `vram_mb`, `est_secs` when they apply).

Light, never leased: reading and searching files, `git`, `cargo check`, `cargo clippy`,
`cargo fmt`, single quick scripts, `rh` itself, and anything with a light flag above.

## The loop: check, lease, run, release

1. **Classify** the command (above). Light: just run it.
2. **Optionally check** with `can_fit` (same `class` or profile, `kind`, and the command as
   `label`) when the answer changes what you do, for example to choose between a full build
   now and a narrower one (`-p <crate>`), or to tell the user up front that the machine is
   busy. It changes nothing and holds no place in the queue.
3. **Acquire**: `acquire_lease` with `class` (or a custom profile) and `label` set to the
   exact command, for example `cargo test --workspace`. `wait_secs` defaults to 60, at most 120.
   When the command runs somewhere other than the session's directory, pass that directory
   as `cwd`.
4. **Run** the command only when the result's `state` is `active`. A `queued` result keeps
   its place: follow "Waiting on a queued lease" below. Never acquire a second lease for the
   same work while one is queued.
5. **Release** with `release_lease` as soon as the heavy command (or a tight batch of them,
   such as build then test) finishes, whether it passed, failed, or was cancelled. Do not hold
   a lease while you read output, think, or edit for more than a minute or two; acquire again
   for the next run.

One active lease covers several commands of its kind: a lease with `cores > 0` covers CPU
work, one with `gpu_pct > 0` or `vram_mb > 0` covers GPU work, `mixed` needs both, and a
`benchmark_exclusive` lease covers everything. Leases are kept alive by the MCP server and
released automatically when the session ends; you never send heartbeats.

### When the hook blocks a command

A deny starting "Blocked by resources-helper" names the class to acquire, or says that your
queued lease has not started yet. Follow it: acquire (or wait), then retry the same command.
Never work around the hook by renaming, wrapping, splitting, or moving the command into a
script; the policy applies to the work, not the wording.

### When the daemon is unavailable

If the tools return a connection error, the hook lets heavy commands through with a warning
("runs unchecked"). Tell the user once that coordination is off and suggest `rh daemon start`,
then continue. If the hook instead blocks because rhd is unavailable (fail-closed config), ask
the user to run `rh daemon start`. If this session has no `acquire_lease` tool at all, it was
started before the MCP server was registered: tell the user to restart the session.

## Waiting on a queued lease

1. Tell the user in one line what is ahead and for how long, from the result's `verdict`:
   its `reasons` and `wait_estimate_secs`. `list_leases` with `all_sessions` shows the leases
   ahead and their labels.
2. If `wait_estimate_secs` is over 600 or missing (unknown wait, or a request the machine can
   never fit), ask the user whether to keep waiting or give up. Waiting keeps the place;
   giving up is `release_lease`. A reason saying "more than the machine can free" will never
   fit: offer a smaller custom profile or a narrower command instead.
3. Keep waiting by calling `acquire_lease` again with only `lease_id` (and `wait_secs`, up to
   120). It holds the same place in the queue, returns as soon as the lease starts, and its
   `verdict` is fresh each time. Do not sleep in the shell between calls, and never request a
   new lease for the same work. Do light work that does not depend on the command's result
   between calls if there is any.
4. Once a call returns `active`, run the command, then release it as usual.

## Reading the machine

`system_snapshot` returns:

- `cpu_pct` (1, 10, 60 s averages) and `logical_cores`. Free cores are roughly
  `logical_cores * (1 - cpu_pct/100)`; prefer the 10 s value, the 1 s value is jumpy.
- `memory`: used and total RAM and commit charge.
- `gpus`: per adapter, `busiest_engine_pct` (1 s and 10 s) and VRAM used and total.
- `sessions`: each Claude session's summed `cores`, `ram_mb`, `gpu_pct`, `vram_mb`;
  `is_current` marks yours. `other` is load from everything outside the sessions.

Admission uses reservations, not just measured load: a granted lease keeps its full profile
reserved even before its processes ramp up, and a headroom (15% by default) of each resource
is never handed out.
So `can_fit` is the authority on whether a job starts; the snapshot explains why (who is
busy, whether the load is another session's lease or unrelated background like a game).
Mention it to the user when unrelated background load is what blocks you.

The daemon also learns from history. Every released lease records what its session used,
keyed by the command and its directory, so `label` must be the real command. When earlier
runs of the same command needed more than the declared class, the lease reserves the larger
values (p90 of past runs, capped at what the machine can free), and the first `reasons` say
so, for example "cores 11.2 from p90 of 4 past run(s) (declared 8.0)". That is expected, not
an error. `rh history` shows what each job needed.

## Benchmarks

Use `kind: benchmark_exclusive` when the output is a measurement someone will compare or
report: `cargo bench` or criterion runs for numbers, `hyperfine`, profiling, latency or
throughput tests, before/after performance comparisons.

Do not use it for correctness: running tests, checking that benches compile
(`cargo bench --no-run`), or a smoke run whose timing nobody reads. Those take a normal lease.

A benchmark lease waits until no other lease is active, then measures the machine for 10
seconds: it starts once total CPU and the busiest GPU engine average at or below 10% over that
window. A single busy second does not restart the wait. If the average stays above the limits,
the lease is granted anyway 60 seconds after the drain, marked as not quiet. (Defaults from
`[benchmark]` in the config: `quiet_cpu_pct`, `quiet_gpu_pct`, `quiet_secs`,
`max_quiet_wait_secs`.) While it waits it blocks every later request so the machine drains,
and while it is active nothing else is granted. That costs every other session, so:

- Tell the user before acquiring that other sessions will be held back.
- Build first under a normal lease (`cargo bench --no-run`), release it, then acquire the
  benchmark lease for the measuring run only.
- Release it the moment the measurement ends.

While it waits, `verdict.benchmark` says where it stands, by `phase`:

- `draining`: `active_leases` other leases still run; nothing to do but wait.
- `measuring`: `measured_secs` of `needed_secs` averaged so far, with the averages
  (`cpu_avg_pct`, `gpu_avg_pct`).
- `noisy`: the window is above the limits; it starts anyway in `grant_anyway_in_secs`.
  Unrelated background load (a game, a video call) is the usual cause: tell the user what
  the averages are, and that the benchmark will start without quiet unless they close it.

`release_lease` on a benchmark lease returns a `noise` report: average and peak CPU and GPU
load from everything outside your session while it ran, and `quiet_at_start`. Report the
numbers with the results, for example "noise: 1.2% CPU average, 4% peak". When
`quiet_at_start` is false or the averages are more than a few percent, say the numbers may be
disturbed.

## Reporting to the user

Keep it to one line per event, in plain words:

- Queued: "Waiting for a build slot: 1 lease ahead (`cargo build` in another session),
  about 3 min. I'll start when it's granted."
- Long or unknown wait: say why (from `reasons`) and ask whether to keep waiting.
- Benchmark: "Waiting for a quiet machine for the benchmark; other sessions' builds are
  held until it finishes." Once it ends, give the noise report with the results.
- Granted after a wait: "Lease granted after 2 min; running `cargo test`."

Do not report routine grants that came back `active` at once.
