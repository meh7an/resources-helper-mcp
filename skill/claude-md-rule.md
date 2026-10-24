# CLAUDE.md rule for resources-helper

Add the section below to the global `~/.claude/CLAUDE.md` between sessions (editing it inside
a running session breaks the prompt cache). It is global because the daemon coordinates
every session on the machine, whatever the project. It assumes the `resource-guard` skill is
installed in `~/.claude/skills/resource-guard/` and the `resources-helper` MCP server and the
`rh hook pre-tool` PreToolUse hook are registered.

---

```markdown
## Shared Machine Resources

This PC is shared by several Claude sessions, coordinated by the resources-helper daemon.

- Before any heavy command (builds, test suites, benchmarks, training, rendering, or anything
  that keeps several cores or the GPU busy for more than ~30 s), follow the `resource-guard`
  skill: `acquire_lease`, run only while the lease is active, then `release_lease` right away.
- A tool call "Blocked by resources-helper" means: do what the message says, then retry the
  same command. Never reword, wrap, or script a command to get past the hook.
- Timing or performance numbers need a `benchmark_exclusive` lease; tests and builds do not.
- When a lease is queued, tell the user in one line what is ahead and the estimated wait, then
  keep waiting; never ask whether to queue or wait. If it ends without starting (removed from
  the queue), don't run the command; say in one line that it was cancelled.
- Lease handling never needs the user's approval; it is not a state-changing command.
```
