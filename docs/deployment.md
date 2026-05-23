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

## PostgreSQL Lease Manager

For multi-worker deployments, configure a shared PostgreSQL instance for distributed lease coordination:

```
database_url = "postgres://user:pass@host/dbname"
```

When `database_url` is set, the worker uses PostgreSQL for repo-level mutual exclusion instead of the in-memory lease manager. The table is created automatically on startup.

Lease acquisition uses `SELECT ... FOR UPDATE` within a transaction to provide row-level locking. This ensures exactly one worker holds a given repo lease at any time, even under concurrent contention.

Each worker identifies itself by hostname. Leases have a short TTL (default 30 seconds) and are continuously renewed by a background heartbeat while the holder is alive.

### Termination Handling

**Graceful shutdown (SIGTERM / SIGINT):** The server intercepts termination signals, stops accepting new connections, and calls `release_all()` to delete all leases held by this worker from the database before exiting. Leases are released immediately.

**Non-graceful termination (crash / OOM / kill -9):** Since the heartbeat stops when the process dies, leases expire within ~30 seconds (the default TTL). A background reaper loop runs every 10 seconds to proactively delete expired leases, so other workers can reclaim them promptly without waiting for their next acquisition attempt.

### Build

Build with the `postgres` feature enabled:

```
cargo build --features postgres
```

## Multi-Worker Safety

- Local repo corruption is handled by deleting local state and hydrating from object storage.
- Force-pushed branches update ref manifests after upstream verification; older commit manifests remain available until retention cleanup.
- Push endpoints are rejected at the HTTP layer and `git-receive-pack` is never served.

