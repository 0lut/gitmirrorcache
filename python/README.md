# python/

Python helpers invoked by the bash scripts under `scripts/`. The directory
layout mirrors `scripts/` (`python/aws/` ↔ `scripts/aws/`, `python/github/` ↔
`scripts/github/`).

- `aws/` — JSON payload builders (IAM policies, ECS task definitions,
  EventBridge targets, ALB listener rules), small AWS CLI output parsers, and
  `ssm_command.py`, which wraps a bash fragment from `scripts/aws/ssm/` plus
  shell-quoted variable assignments into an SSM RunShellScript parameters
  document.
- `github/` — date-cutoff checks used by the Actions cache/artifact cleanup
  scripts.

All helpers are stdlib-only and run with the system `python3`.
