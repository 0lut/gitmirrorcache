# Git Fetch Cache Implementation Plan

Source: [git_fetch_cache_handoff.md](git_fetch_cache_handoff.md)

## Direction

Build the control plane and safety boundaries in Rust while continuing to delegate Git object/protocol work to the `git` binary. Object storage is the durable source of truth; local block storage is a disposable hot cache.

## Initial Architecture

- `git-cache-core`: shared contracts, validation, config, manifests, request/response models, and error types.
- `git-cache-git`: hardened wrapper around the Git CLI with explicit argv, controlled env, and timeouts.
- `git-cache-objectstore`: portable object store trait plus local dev adapter first, S3-compatible adapter next.
- `git-cache-disk`: reservations, quota accounting, temp directories, eviction, and disk status.
- `git-cache-api`: Axum API and Git smart HTTP session entrypoints.
- `git-cache-worker`: cron/read-through/event update orchestration.
- `git-cache-cli`: admin and local development commands.

## Implementation Sequence

1. Finish M1 skeleton.
   Establish a buildable workspace, config loading, API health check, and basic logging.

2. Build reliable local development fixtures.
   Use local bare repositories as fake upstreams and the local object-store adapter. This keeps the consistency contract testable before adding S3 or GitHub credentials.

3. Implement Git wrapper operations.
   Add `init-bare`, `fetch`, `rev-parse`, `fsck`, bundle creation, bundle hydration, and upload-pack support with timeouts and stderr/stdout bounds.

4. Implement manifests and object-store publishing.
   Store generation, commit, ref, session, and lease manifests with conditional writes. Treat generation publish as durable only after Git verification and bundle upload complete.

5. Implement exact commit materialization.
   Prefer known-complete cached commit manifests. Contact upstream only when the commit is unknown or incomplete. Return `503` if upstream is required and unavailable.

6. Implement strict branch/default materialization.
   Always verify upstream before resolving mutable selectors in strict mode, then serve through a pinned synthetic session ref.

7. Add Git smart HTTP session serving.
   Route session URLs to `git-upload-pack`, advertise only the session synthetic ref, and reject all receive-pack attempts.

8. Add update flows and multi-worker coordination.
   Implement read-through updates behind per-repo leases, cron refreshes, event hints, and duplicate request coalescing.

9. Harden for production.
   Add metrics, rate limits, credential handling, bundle compaction, disk pressure tests, chaos tests, and deployment notes.

## Near-Term Acceptance Checks

- `cargo check --workspace` succeeds.
- `/healthz` returns an OK response from the local API server.
- Selector and repo validation reject traversal and malformed refs.
- Local object-store `put_if_absent` behaves as the basis for conditional leases.
- Disk reservations return `507`-style errors once quota cannot be honored.

## Completion Notes

- API materialization now covers exact commits, strict branches, strict default branch, cached branch/default, session manifests, and upload-pack session serving.
- Integration tests cover upstream-offline cached commits, strict upstream failures, default branch resolution, branch force-push behavior, receive-pack rejection, and session advertisement.
- Worker, disk, Git, and object-store crates each have focused tests for their handoff responsibilities.
- Operational follow-up details live in [README.md](README.md), [docs/local-dev.md](docs/local-dev.md), [docs/deployment.md](docs/deployment.md), and [docs/chaos-tests.md](docs/chaos-tests.md).
