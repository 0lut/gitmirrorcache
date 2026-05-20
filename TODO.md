# Git Fetch Cache TODO

Source: [git_fetch_cache_handoff.md](git_fetch_cache_handoff.md)

All handoff TODOs have an implemented first version, test coverage, or an operational document where the item is deployment/process-oriented.

## Current Focus

- [x] Create Rust workspace and crate boundaries.
- [x] Add first-pass shared core types for repos, selectors, manifests, config, and errors.
- [x] Add API/CLI/worker scaffolding that compiles.
- [x] Implement real materialization flow for cached exact commits.
- [x] Add local fake upstream and object-store integration tests.

## M1: Skeleton

- [x] Root Cargo workspace.
- [x] Crates from handoff:
  - [x] `git-cache-core`
  - [x] `git-cache-git`
  - [x] `git-cache-objectstore`
  - [x] `git-cache-disk`
  - [x] `git-cache-api`
  - [x] `git-cache-worker`
  - [x] `git-cache-cli`
- [x] Config model with local and production example files.
- [x] Axum server shell with `/healthz`.
- [x] Structured logging initialization.
- [x] Service README and local dev runbook.

## M2: Git Wrapper

- [x] Hardened `git` process runner scaffold.
- [x] Bound stdout/stderr capture.
- [x] `init-bare`.
- [x] `fetch-branch`.
- [x] `rev-parse`.
- [x] `fsck`.
- [x] `bundle create`.
- [x] `fetch from bundle`.
- [x] `upload-pack` session wrapper.
- [x] Integration tests against local bare repos.

## M3: Object Store

- [x] Portable `ObjectStore` trait.
- [x] Local filesystem adapter for tests/dev.
- [x] S3-compatible adapter.
- [x] Manifest read/write helpers.
- [x] Generation publish protocol.
- [x] Conditional manifest updates and leases.

## M4: Disk Manager

- [x] Reservation model scaffold.
- [x] Quota accounting placeholder.
- [x] LRU repo index.
- [x] Eviction of unlocked repos.
- [x] Temp directory cleanup.
- [x] `disk-status` admin command backed by real accounting.

## M5: Exact Commit Materialization

- [x] Commit manifest lookup.
- [x] Hydrate local bare repo from object storage.
- [x] Create pinned session manifest.
- [x] Serve known-complete cached commit while upstream is offline.

## M6: Strict Branch and Default Branch

- [x] Verify branch head upstream before serving strict branch.
- [x] Resolve upstream default branch before serving latest/default.
- [x] Return `503` when strict verification requires unavailable upstream.
- [x] Return `404` only after upstream confirms absence.

## M7: Git Smart HTTP

- [x] Reject `git-receive-pack` route attempts in API shell.
- [x] Session-aware `info/refs`.
- [x] Session-aware `git-upload-pack`.
- [x] Never advertise receive-pack.

## M8: Updaters and Leases

- [x] Cron update loop.
- [x] Read-through update path.
- [x] Event hint intake.
- [x] Per-repo lease acquisition with conditional object writes.
- [x] Concurrency dedupe for same repo/ref.

## M9: Production Hardening

- [x] Metrics.
- [x] Rate limits.
- [x] Credential handling.
- [x] Bundle compaction strategy.
- [x] Chaos tests.
- [x] Multi-worker deployment docs.

