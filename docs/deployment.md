# Deployment Notes

## Worker Model

Run at least three API/worker instances behind a load balancer. Each worker owns local block storage mounted at `cache_root`; this storage is a hot cache only and can be deleted without losing durable state.

Object storage is the source of truth for:

- generation manifests
- bundles
- commit manifests
- ref manifests
- session manifests
- lease objects

## Coordination

The worker crate provides:

- per-repo lease traits
- in-memory lease manager for tests
- read-through update path
- cron update loop
- event hint intake
- same repo/ref in-flight dedupe

Production lease implementations should use object-store conditional writes, matching `put_if_absent` semantics.

## Credentials

Set `upstream_auth_token_env = "GITHUB_TOKEN"` in config and provide that environment variable to the process. The API injects the token through Git config environment variables:

- no token in argv
- no token in manifests
- no token in structured logs from command arguments

## Metrics And Limits

- `/metrics` exposes Prometheus-style counters.
- `rate_limit_per_minute` applies a simple global materialize limit.
- `max_git_output_bytes` bounds captured Git stdout/stderr.
- `git_timeout_seconds` bounds Git process lifetime.

## Bundle Strategy And Compaction

The current publish path writes a full bundle for each generation. That gives a simple v1 compaction posture: every generation is self-contained, hydration does not require a long incremental chain, and cache loss recovery is straightforward.

Future incremental bundles can reuse the existing `parent_generation` field. A compactor should publish a new full generation, verify it with `git fsck`, move commit/ref manifests to the compacted generation, then prune old generations after retention expires.

## Multi-Worker Safety

- Local repo corruption is handled by deleting local state and hydrating from object storage.
- Force-pushed branches update ref manifests after upstream verification; older commit manifests remain available until retention cleanup.
- Push endpoints are rejected at the HTTP layer and `git-receive-pack` is never served.

