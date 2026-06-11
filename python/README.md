# python/

Python helpers invoked by the bash scripts under `scripts/`. The directory
layout mirrors `scripts/` (`python/aws/` ↔ `scripts/aws/`, `python/github/` ↔
`scripts/github/`).

- `aws/` — JSON payload builders (IAM policies, ECS task definitions,
  EventBridge targets, ALB listener rules), SSM RunShellScript command
  generators, and small AWS CLI output parsers used by the deploy and
  diagnostics scripts.
- `github/` — date-cutoff checks used by the Actions cache/artifact cleanup
  scripts.

All helpers are stdlib-only and run with the system `python3`.
