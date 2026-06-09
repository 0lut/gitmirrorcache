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

## Cold-Cache Perf Testing

Local perf observations that apply regardless of where the server runs:

- **Opt out of proxy-on-miss or you measure the proxy, not the cache.** Since
  proxy-on-miss became the default, read-through benchmarks must send a falsey header:
  ```bash
  git clone -c http.extraHeader="git-cache-use-proxy-on-miss: 0" <base-url>/git/github.com/<owner>/<repo>.git
  ```
- **Assert on server logs, not just wall time.** A healthy cold clone shows:
  - one `direct git batched read-through fetch for wanted commits` line with
    `refspec_count` ≈ the repo's advertised ref count (700+ for astral repos);
  - upstream `git command finished args=["fetch", ...]` lines: ≤ 3 per cold
    clone, not one per want;
  - exactly one `queued direct git background fsck` per upload-pack request.
- A fresh `git clone` wants the tip of EVERY advertised ref, so many-ref repos
  (astral-sh/ruff, astral-sh/uv, llvm) are the right stress tests; few-ref repos
  (octocat/Hello-World) won't exercise batching.
- **Time the clone yourself** (`S=$(date +%s.%N); git clone ...; date +%s.%N`).
  `/usr/bin/time` may not be installed.

For perf testing against live AWS preview stacks (deploy, CloudWatch log
assertions, cross-build gotchas, reference numbers), see the privileged runbook:
[gitmirrorcache-deploy](../../../.devin/skills/gitmirrorcache-deploy/SKILL.md).

## Known Limitations

- **Large repos may timeout on first access**: The default `git_timeout_seconds = 120` might not be enough for the initial full fetch of very large repos (e.g., astral-sh/uv). The Python integration tests use `--depth 1` shallow clones to handle this. Use small repos like `octocat/Hello-World` for quick smoke tests.
- **`upstream_root` vs real GitHub**: With `upstream_root` set (as in `local.example.toml`), the server uses local filesystem paths as upstreams. Remove it to proxy to real GitHub.
- **Integration tests use `multi_thread` tokio runtime**: The Rust integration tests require `#[tokio::test(flavor = "multi_thread")]` because single-threaded runtime deadlocks when blocking git CLI calls starve the Axum server.
- **Very large repos (llvm-scale) may still hit the server 1h git timeout** during
  pack-objects on full-ref blobless clones; this is independent of fetch batching.

## What to Verify After Validation Changes

When modifying input validators (`reject_remote_url`, `reject_refspec`, `reject_nul`, `reject_ref_arg`, `reject_revision_arg`, `reject_config_key`) or `--` separator placement:

1. Run integration tests — they exercise the full clone/fetch/update-ref flow
2. Start live server with `RUST_LOG=debug` and verify git command args in logs show `--` separators
3. Verify `git clone` through the proxy still works (validators not too strict)
4. Verify disallowed hosts return 4xx (validators not too loose)
5. Upstream-derived strings (e.g. branch names from ls-remote) interpolated into
   refspecs must be validated per-component (e.g. `branch_cache_refspec`) — `reject_refspec`
   alone allows `:` because refspecs legitimately contain it

## Secrets Needed

None — all testing uses public GitHub repos and local infrastructure.
For AWS preview-stack perf testing, see
[gitmirrorcache-deploy](../../../.devin/skills/gitmirrorcache-deploy/SKILL.md).
