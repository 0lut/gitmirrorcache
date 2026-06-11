---
name: testing-git-remote
description: Test the read-through Git remote feature end-to-end. Use when verifying changes to the /git/ endpoint, input validation, domain-layer materializer methods, or git wrapper functions.
---

# Testing the Read-Through Git Remote

Requirements: local Rust/Git toolchain; optional public GitHub network access
for live validation; no secrets; no live-infrastructure access.

This local-only runbook covers validation for the read-through Git remote.
Follow the shared repository rules in [AGENTS.md](../../../AGENTS.md);
privileged AWS deployment and recovery operations live in
[gitmirrorcache-deploy](../../../.devin/skills/gitmirrorcache-deploy/SKILL.md).

## Quick Checks

```bash
# Rust integration tests (6 tests, ~0.4s, uses local fixture)
cargo test --test git_remote_integration -- --nocapture

# Full workspace tests
cargo test --workspace

# Lint
cargo clippy --workspace
```

## Live End-to-End Testing Against Real GitHub

### 1. Create a config file WITHOUT `upstream_root`

When `upstream_root` is set, the server resolves repos from the local filesystem. To proxy to real GitHub, omit it:

```toml
# /tmp/test-config.toml
bind_addr = "127.0.0.1:8080"
public_base_url = "http://127.0.0.1:8080"
cache_root = "/tmp/gitcache-test/cache"
git_binary = "git"
git_timeout_seconds = 120
max_git_output_bytes = 16777216
session_ttl_seconds = 3600
rate_limit_per_minute = 120
allowed_upstream_hosts = ["github.com"]

[object_store]
kind = "local"
root = "/tmp/gitcache-test/object-store"

[disk]
quota_bytes = 10737418240
min_free_bytes = 1073741824

[git_remote]
enabled = true
branch_ref_check = "always"
commit_read_through = true
```

### 2. Start the server

The config env var is `GIT_CACHE_CONFIG` (NOT `CONFIG_PATH`):

```bash
mkdir -p /tmp/gitcache-test/cache /tmp/gitcache-test/object-store
cargo build --release --bin git-cache-api
RUST_LOG=debug GIT_CACHE_CONFIG=/tmp/test-config.toml ./target/release/git-cache-api
```

### 3. Test commands

```bash
# Clone a small public repo through the proxy
git clone http://127.0.0.1:8080/git/github.com/octocat/Hello-World.git /tmp/hello-world-test

# Verify host validation rejects disallowed hosts (expect HTTP 400)
curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:8080/git/evil.com/org/repo.git/info/refs?service=git-upload-pack"

# Verify push is rejected (expect HTTP 405)
curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:8080/git/github.com/octocat/Hello-World.git/info/refs?service=git-receive-pack"
```

## Verifying Clone Integrity (exit 0 is NOT enough)

A clone served from a partially-hydrated or shallow cache repo can exit 0 yet
be silently corrupt. Always assert all of:

```bash
ls | wc -l                       # worktree populated (>0)
git status --porcelain | wc -l   # clean (== 0)
git log --oneline | wc -l        # full history walks without errors
                                 # (linux ~1.45M commits, llvm ~580k)
```

If `git log` dies with "Failed to traverse parents of commit ...", the serving
cache repo was likely shallow (had a `shallow` file from a depth-limited
hydration) when it served a full-history want. The server is expected to force
an `--unshallow` fetch (read-through) or decline prepare and proxy in that
case — if it doesn't, that's a correctness bug, not a flake.

## Intent-Shape Test Matrix

Bugs in this area are usually shape-transition bugs, not single-shape bugs.
When touching hydration/serving, test the *sequence* of shapes against the same
repo, not just each shape cold:

1. blobless `--depth 1` (hydrates blobless + shallow)
2. full `--depth 1` (exercises partial-hydration refetch / prepare decline)
3. full-history (exercises unshallow handling)
4. blobless clone checkout (exercises the batched lazy blob fetch — the
   checkout sends ONE upload-pack POST with tens of thousands of blob wants;
   it should be served as one batched fetch, not per-want lookups)

## Git Wrapper Gotchas

- **Raw object OIDs are not revisions**: fetching exact blob/tree OIDs must
  mirror git's own promisor lazy-fetch argv — pass OIDs via `--stdin`, with
  `-c fetch.negotiationAlgorithm=noop` (the `-c` goes BEFORE the `fetch`
  subcommand), `--no-write-fetch-head`, and `--recurse-submodules=no`.
  Otherwise git tries to write FETCH_HEAD from blob OIDs and dies with
  "bad revision".
- **`GIT_NO_LAZY_FETCH=1` requires git >= 2.45**: older gits (e.g. Debian
  bookworm's 2.39) die with "could not fetch ... from promisor remote"
  instead of reporting objects as missing. If classification/serving behaves
  differently in a container than locally, check the container's git version.
- **`--unshallow` on a complete repo is a hard error** ("complete repository").
  Any unshallow flag must be dropped when no `shallow` file exists — and
  re-checked per fetch, since an earlier fetch in the same request may have
  already unshallowed the repo.

## Known Limitations

- **Large repos may timeout on first access**: The default `git_timeout_seconds = 120` might not be enough for the initial full fetch of very large repos (e.g., astral-sh/uv). The Python integration tests use `--depth 1` shallow clones to handle this. Use small repos like `octocat/Hello-World` for quick smoke tests.
- **`upstream_root` vs real GitHub**: With `upstream_root` set (as in `local.example.toml`), the server uses local filesystem paths as upstreams. Remove it to proxy to real GitHub.
- **Integration tests use `multi_thread` tokio runtime**: The Rust integration tests require `#[tokio::test(flavor = "multi_thread")]` because single-threaded runtime deadlocks when blocking git CLI calls starve the Axum server.
- **CI only runs on PRs targeting main**: a PR based on another PR's branch
  gets no CI until it retargets main — run `cargo fmt --check`, clippy, and
  the full workspace tests locally and say so explicitly in the PR.

## What to Verify After Validation Changes

When modifying input validators (`reject_remote_url`, `reject_refspec`, `reject_nul`, `reject_ref_arg`, `reject_revision_arg`, `reject_config_key`) or `--` separator placement:

1. Run integration tests — they exercise the full clone/fetch/update-ref flow
2. Start live server with `RUST_LOG=debug` and verify git command args in logs show `--` separators
3. Verify `git clone` through the proxy still works (validators not too strict)
4. Verify disallowed hosts return 4xx (validators not too loose)

## Secrets Needed

None — all testing uses public GitHub repos and local infrastructure.
