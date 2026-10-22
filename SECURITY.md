# Security Policy

## Supported versions

Only the latest release receives security fixes.

## Reporting a vulnerability

Please do not open a public issue. Report it privately through
[GitHub private vulnerability reporting](https://github.com/meh7an/resources-helper-mcp/security/advisories/new),
or by email to hi@mehran.tech. Include the version, what an attacker can do, and the steps to
reproduce it. You will get an answer within a week, and credit in the advisory if you want it.

## Threat model

resources-helper is a per-user tool for one machine. In scope:

- rhd listens only on `127.0.0.1` and requires a bearer token stored in
  `%USERPROFILE%\.resources-helper\token`. Another local user, or a process that cannot read
  that file, reaching the API is a vulnerability.
- The PreToolUse hook reads JSON from Claude Code on stdin. Input that makes it crash, hang,
  or run something is a vulnerability.
- `rh install` and `rh uninstall` write and delete files only under
  `%USERPROFILE%\.resources-helper\` and `~/.claude/skills/resource-guard/`. Anything else is
  a vulnerability.

Out of scope: processes running as the same user, which can already read the token, and a
session bypassing the hook on purpose. The hook coordinates cooperating sessions and is not a
security boundary.
