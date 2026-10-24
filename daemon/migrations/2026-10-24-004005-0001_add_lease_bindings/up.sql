-- The command a lease covers, from the PreToolUse hook's report: its tool call and when it was
-- reported. Processes that started before the report are never part of the command.
ALTER TABLE leases ADD COLUMN bind_tool_use_id TEXT;
ALTER TABLE leases ADD COLUMN bound_at_ms BIGINT;
-- Root of the command's process tree, once it is found.
ALTER TABLE leases ADD COLUMN bound_pid BIGINT;
ALTER TABLE leases ADD COLUMN bound_pid_start_time BIGINT;
