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

## Known Limitations

- **Large repos may timeout on first access**: The default `git_timeout_seconds = 120` might not be enough for the initial full fetch of very large repos (e.g., astral-sh/uv). The Python integration tests use `--depth 1` shallow clones to handle this. Use small repos like `octocat/Hello-World` for quick smoke tests.
- **`upstream_root` vs real GitHub**: With `upstream_root` set (as in `local.example.toml`), the server uses local filesystem paths as upstreams. Remove it to proxy to real GitHub.
- **Integration tests use `multi_thread` tokio runtime**: The Rust integration tests require `#[tokio::test(flavor = "multi_thread")]` because single-threaded runtime deadlocks when blocking git CLI calls starve the Axum server.

## What to Verify After Validation Changes

When modifying input validators (`reject_remote_url`, `reject_refspec`, `reject_nul`, `reject_ref_arg`, `reject_revision_arg`, `reject_config_key`) or `--` separator placement:

1. Run integration tests — they exercise the full clone/fetch/update-ref flow
2. Start live server with `RUST_LOG=debug` and verify git command args in logs show `--` separators
3. Verify `git clone` through the proxy still works (validators not too strict)
4. Verify disallowed hosts return 4xx (validators not too loose)

## Secrets Needed

None — all testing uses public GitHub repos and local infrastructure.
