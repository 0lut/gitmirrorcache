# python/

Python helpers invoked by the bash scripts under `scripts/`. Each file was
extracted verbatim from a former inline heredoc; the directory layout mirrors
`scripts/` (`python/aws/` ↔ `scripts/aws/`, `python/github/` ↔
`scripts/github/`).

All helpers are stdlib-only and run with the system `python3`, so no
virtualenv or uv project is needed. If a helper ever grows third-party
dependencies, turn this directory into a uv project (`uv init`) and switch the
calling script to `uv run`.
