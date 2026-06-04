# Multi-Worker Conflict Avoidance Plan

## Purpose

This plan describes how the cache should avoid conflicts when multiple API or
worker instances operate on the same repository with separate local `/cache`
volumes and one shared object store.

The central rule is:

> Object storage is the coordination and durability plane. A worker's local
> `/cache` is only a disposable acceleration layer.

We do not need distributed singleflight. We do need worker-local request
deduplication and object-store backed coordination for every operation that can
publish or mutate shared repository metadata.

## Research Context: Packhorse

This plan was informed by a comparison with GitLab's
[Packhorse](https://gitlab.com/gitlab-org/packhorse), inspected on `main` at
`d8cebce033d11789ac1da01c39ec46f01c7166ed` from 2026-06-03. Packhorse is the
closest public system found, but it solves a different primary problem.

Packhorse is a Git Smart HTTP v2 proxy for GitLab/Gitaly CI clone storms. It
protects Gitaly when many CI jobs fetch the same repository at the same time by
caching upload-pack responses, coalescing identical misses inside one process,
and optionally injecting Git `packfile-uris` so large base packs are downloaded
from object storage rather than streamed from Gitaly.

Important Packhorse findings:

- Its main cache key is derived from repository, sorted `want`s, optional
  `have`s, optional base-packfile hash, and shallow depth.
- Its cacheable surface is intentionally narrow: Git protocol v2 fetches with
  explicit `want`s, `deepen` between 1 and 10, no filters, and no `want-ref`.
  Other traffic falls through to upstream.
- Its request coalescing is in-process `singleflight`. Redis/distributed
  coalescing is discussed as future work, not an implemented requirement.
- Its packfile-URI path depends on externally generated base packfiles uploaded
  to object storage and registered with Packhorse. Packhorse extracts commit
  IDs, injects those commits as `have`s, and injects a fresh signed URI into the
  upload-pack response.
- Its local cache is response-file oriented and instance-local. Object storage
  is used for optional base packfiles, not as a durable source of truth for the
  repository's verified generation graph.
- Its Phase 1 base-packfile manager is in-memory, and distributed cache
  consistency/auth-aware caching are either planned or design-level in the
  inspected code.
- Its README describes LRU/size eviction and broad metrics, but the inspected
  implementation is narrower: cache eviction metadata does not currently remove
  disk files in `evictIfNeeded`, and app metrics are mostly hit/miss and
  non-cacheable request counters.

The lesson for this project is not "copy Packhorse." Packhorse is optimized as a
near-runner Git CDN/proxy. This project is a correctness-gated Git
materialization cache whose durable state is object-store manifests and bundles.
Therefore:

- keep worker-local singleflight, because it is useful and simple;
- do not add distributed singleflight unless a future performance profile proves
  it is needed;
- do add object-store backed leases, because cross-worker mutation conflicts
  cannot be solved with in-memory state;
- do not treat local response files or local bare repos as authoritative;
- consider packfile-URI offload later as a throughput optimization, separate
  from the correctness work in this plan.

## Local System Context

The current cache already differs from Packhorse in the ways this plan depends
on:

- `POST /v1/materialize` resolves selectors into short-lived session Git URLs.
- Exact complete commit manifests can be served from cache without contacting
  upstream.
- Strict branch/default requests must verify upstream before serving.
- Sessions pin one commit behind `refs/cache/sessions/<uuid>`.
- Generation bundles, generation manifests, commit manifests, ref manifests,
  sessions, and pending publishes are stored in the object store.
- Local `/cache` contains hydrated bare repos and temporary bundle work files,
  but losing it should only force rehydration or upstream verification.
- Push is not supported; `git-receive-pack` is rejected.

Relevant implementation areas:

- `crates/git-cache-api`: API wiring, materialize handler, direct Git remote,
  metrics, upload-pack streaming bounds.
- `crates/git-cache-domain`: materialization, generation publishing,
  verification, hydration, compaction, session repos.
- `crates/git-cache-worker`: worker-local in-flight dedupe and repo lease
  abstraction.
- `crates/git-cache-objectstore`: object-store trait, manifest helpers,
  create-once writes, lease helpers, local and S3 adapters.
- `crates/git-cache-disk`: local repo locks, disk reservations, LRU eviction,
  stale temp cleanup.
- `crates/git-cache-git`: hardened Git subprocess wrapper and argument
  validation.

## Goals

- Keep exact known-complete commit reads fast and independent of upstream.
- Allow multiple workers with partial or empty local caches.
- Prevent conflicting generation chains, ref observations, and generation-head
  updates.
- Avoid in-memory leases for cross-worker exclusion.
- Make write paths idempotent where possible and fenced where mutation is
  unavoidable.
- Let a cold worker incrementally hydrate from object storage before fetching or
  publishing new state.
- Make crash recovery deterministic for pending generations, orphan bundles,
  stale leases, and compaction.

## Non-Goals

- Distributed singleflight across workers.
- A distributed queue as a required dependency.
- Relying on replicated block storage as a correctness boundary.
- Making branch/latest requests serve stale data unless a caller explicitly asks
  for bounded-staleness behavior in a separate feature.

## Terms

| Term | Meaning |
| --- | --- |
| Worker | One API or background process with its own local `/cache` root. |
| Local singleflight | Per-worker in-memory dedupe for identical in-flight requests. This is allowed and useful, but never coordinates across workers. |
| Durable lease | A lease stored in object storage with TTL, holder identity, and fencing token. This is the only cross-worker exclusion primitive. |
| Generation | An immutable bundle plus manifest, optionally parented to an older generation. |
| Pending generation | Bundle and pending manifest uploaded before verified metadata and head/ref advancement. |
| Verified generation | A generation whose bundle chain was hydrated, hash/length checked, and accepted by `git fsck`. |
| Generation head | The current durable summary of the newest verified generation chain for a repo. |
| Ref manifest | Durable observation for a branch/default ref after upstream verification. |

## Current Gaps To Close

The code already has many of the right building blocks: object-store manifests,
pending generation publishes, generation verification, disk reservations,
per-worker in-flight request dedupe, and local repo locks. The gaps are mostly
coordination boundaries.

1. API wiring currently uses `InMemoryRepoLeaseManager`; this does not exclude
   other workers.
2. Object-store leases currently support create-once acquisition, but need
   expiry takeover, renewal, release, and fencing.
3. Current manifest writes are mixed: immutable generation data is mostly
   conditional, but current ref/head writes can be blind overwrites.
4. Asynchronous verification can allow another worker to build from an older
   generation head while a newer pending generation exists.
5. Compaction and generation publishing need a shared write discipline so
   compaction cannot delete parents needed by a worker that is building or
   verifying a child generation.

## Design Principles

### 1. Local singleflight stays local

Each worker should continue deduping identical in-flight updates by `repo +
target`. This prevents one worker from starting the same fetch repeatedly.

Across workers, do not attempt to wait on a distributed singleflight result.
Instead:

- acquire a durable lease for mutating work;
- if busy, reread durable manifests in case the other worker already published
  the answer;
- otherwise return `503`/`Retry-After` or let the caller retry.

This keeps cross-worker behavior simple and makes object-store state the only
shared truth.

### 2. Reads do not take repo-write leases

Known-complete exact commit reads should not take the repo-write lease.

Fast path:

1. Read commit manifest.
2. Require `complete = true`.
3. Hydrate the referenced generation chain into the local repo if necessary.
4. Create a session ref and serve from local `git-upload-pack`.

This path is safe because commit manifests point to immutable generation data.
It may use local repo locks and disk reservations, but it should not block
branch/default updates on other workers.

### 3. Mutations require durable leases

Any operation that can fetch from upstream or publish shared metadata needs a
durable lease:

- strict branch/default materialization;
- exact commit read-through when the commit is unknown or incomplete;
- direct remote upload-pack when it needs to fetch unknown wants;
- generation verification that can advance head/ref metadata;
- compaction and old-generation deletion;
- cleanup of pending generation objects and orphan bundles.

The first implementation can use one coarse lease:

```text
repos/{repo}/leases/repo-write.json
```

Later, we can split into `repo-update`, `repo-verify`, `repo-compact`, and
`repo-gc` if contention warrants it. Correctness should start coarse.

### 4. Leases need fencing, not just TTL

Lease manifests should include:

```json
{
  "schema_version": 1,
  "repo": "github.com/org/repo",
  "name": "repo-write",
  "holder": "worker-uuid/pid/boot-id",
  "token": "uuid-v7",
  "acquired_at": "...",
  "renewed_at": "...",
  "expires_at": "...",
  "operation": "strict-branch main",
  "expected_head": "optional generation id"
}
```

Required operations:

- `acquire_if_absent`
- `steal_if_expired_and_version_matches`
- `renew_if_token_matches`
- `release_if_token_matches`

TTL alone is not enough. If Worker A pauses, Worker B steals the expired lease,
and Worker A later resumes, Worker A must not be able to release B's lease or
advance shared metadata. Every mutable write should either happen while holding
the lease token or be conditional on an expected object-store version.

### Lease runtime defaults

Start with explicit, configurable defaults:

| Setting | Default | Meaning |
| --- | --- | --- |
| `GIT_CACHE_WORKER_ID` | generated boot UUID plus pid/hostname | Holder identity written into lease manifests and logs. |
| `GIT_CACHE_LEASE_TTL_SECONDS` | `300` | Lease duration long enough for temporary process stalls and object-store tail latency. |
| `GIT_CACHE_LEASE_RENEW_INTERVAL_SECONDS` | `60` | Renewal cadence while a worker is fetching, bundling, verifying, or mutating manifests. |
| `GIT_CACHE_LEASE_STEAL_SKEW_SECONDS` | `30` | Extra age required before treating a lease as expired, to absorb clock skew and object-store timestamp granularity. |
| `GIT_CACHE_LEASE_BUSY_RETRY_AFTER_SECONDS` | `5` | Default `Retry-After` for requests that cannot acquire `repo-write`. |

Lease expiry decisions should prefer object-store metadata time, such as S3
`LastModified`, over worker wall-clock fields in the JSON body. The manifest's
`acquired_at`, `renewed_at`, and `expires_at` fields are useful diagnostics, but
they are not the correctness boundary. If a backend cannot provide reliable
object metadata time, it must apply the configured skew margin before stealing.

A mutating operation should run a renewal task while it holds the lease. Before
publishing any mutable pointer, it must verify that the current lease object
still has its token. If renewal fails because the token changed, or if the
worker cannot prove it still owns the lease before a mutable write, the worker
must abort the operation. Uploaded immutable bundles or pending manifests can be
left for pending-generation recovery and GC.

## Object-Store CAS Requirements

The object-store trait should grow compare-and-swap support. S3 can implement it
with version IDs or ETag conditions; the local adapter can implement it with
lock files or atomic compare-then-rename in a critical section.

Minimum useful API:

```rust
struct ObjectVersion {
    etag_or_version: String,
}

async fn get_with_version(&self, key: &str) -> Result<Option<(Bytes, ObjectVersion)>>;
async fn put_if_version_matches(
    &self,
    key: &str,
    expected: &ObjectVersion,
    value: Bytes,
) -> Result<bool>;
async fn delete_if_version_matches(
    &self,
    key: &str,
    expected: &ObjectVersion,
) -> Result<bool>;
```

We can wrap those in JSON helpers:

- `compare_and_swap_json`
- `advance_generation_head`
- `write_ref_manifest_if_newer`
- `steal_expired_lease`
- `release_lease`

Until CAS exists, durable cross-worker leases are incomplete because expired
lease takeover and fenced release cannot be made safe.

### Backend CAS semantics

CAS is required only for small mutable JSON records: leases, generation heads,
current ref manifests, default-branch manifests, and compaction repoints.
Immutable bundles remain content-addressed or generation-ID-addressed and should
use create-once semantics.

For S3:

- Prefer bucket versioning and use `VersionId` as `ObjectVersion`.
- If versioning is unavailable, use the returned ETag as an opaque version token
  for small single-part JSON objects. Do not interpret ETags as MD5 hashes.
- Create-if-absent should use conditional create semantics, such as
  `If-None-Match: *`.
- Update-if-version-matches should use the provider's conditional write support
  for the current `VersionId` or ETag. If the provider cannot conditionally
  write, multi-worker durable leases must be disabled for that backend.
- Avoid correctness-critical conditional deletes. Release a lease by
  conditionally writing a `released` state with the current token/version, then
  let cleanup delete old released records later.
- Delete markers and multipart ETags must not appear on mutable JSON keys.
  Large bundles may be multipart, but they are immutable and are verified by
  length and SHA-256 in the generation metadata.

For the local object store:

- Use a per-key lock around read-version/write-version operations.
- Store a monotonically changing local version token next to mutable JSON files,
  or derive one from inode metadata only inside the per-key critical section.
- Exercise the same CAS failure paths in tests as S3/MinIO.

## Write Discipline

### Immutable objects

These should be `put_if_absent_or_matches`:

- bundle objects keyed by generation ID;
- generation manifests;
- verified generation manifests;
- pending generation manifests;
- append-only ref observation records;
- session manifests;
- commit manifests for ordinary publishing.

If another worker already wrote identical content, treat it as success. If it
wrote different content at the same key, return conflict.

### Mutable pointers

These need CAS or lease-fenced helpers:

- `generation-head.json`;
- current `refs/heads/<branch>.json`;
- default branch manifest;
- lease objects;
- compaction repoints of commit/ref manifests.

Blind overwrites should be removed from multi-worker write paths.

### Commit manifests

Commit manifests are mostly immutable because a complete commit can safely point
to any verified generation that contains its closure. The safest rule is:

- ordinary publish: write if absent or same;
- ancestor indexing: write if absent or same;
- compaction: repoint only while holding the compaction/repo-write lease, and
  only if the existing generation is in the compacted old-generation set.

That allows compaction to replace old generation pointers without racing a new
publish that discovered the same commit in another generation.

### Ref manifests

Current ref manifests should be changed only after upstream verification and
only by a holder of the repo-write lease. The write helper should CAS against
the ref object version read after lease acquisition:

- if the version still matches, write the new verified observation;
- if the current ref already equals the new observation, treat it as success;
- if the version changed unexpectedly, abort and re-enter materialization under
  a fresh durable-state read.

Do not use wall-clock `verified_at` values to choose the winner between workers.
They are audit metadata. The winner is the holder that still owns the lease token
and updates the expected object version.

The append-only ref observation path can remain immutable and useful for audit.

### Generation head

Generation head advancement should be explicit:

```text
advance_generation_head(repo, expected_current_head, new_head, lease_token)
```

Rules:

1. If no head exists and `expected_current_head = None`, create head.
2. If current head equals `expected_current_head`, replace with `new_head`.
3. If current head already equals `new_head`, treat as success.
4. If current head differs, abort and rebase/retry under a fresh lease.

Do not use wall-clock timestamps to decide which worker wins. Timestamps are
diagnostic; parent/expected-head checks are the consistency boundary.

## Repo-Write Flow

All mutating materialization paths should follow this structure:

1. Run worker-local singleflight for `repo + target`.
2. Re-read commit/ref manifests before taking the lease.
3. If the request can now be served from verified cache, serve it and stop.
4. Acquire durable `repo-write` lease.
5. Record `lease_token`, `observed_head`, relevant object versions, and
   diagnostic `operation_started_at`.
6. Re-read manifests after acquiring the lease.
7. If another worker already published the answer, serve it and release.
8. Hydrate `observed_head` into the local repo if an incremental publish may be
   built.
9. Fetch/verify upstream as required by selector semantics.
10. If target commit is already present in the hydrated head generation, index
    the commit manifest and serve without a new bundle.
11. Build a bundle:
    - incremental if parent generation is hydrated and usable;
    - full if the parent is missing locally, force-pushed away, or fails
      validation.
12. Publish pending bundle and pending manifest.
13. Verify the generation chain.
14. Publish verified metadata.
15. Advance ref/default/head with lease-fenced CAS.
16. Release the lease.

The first correctness-first implementation should keep the repo-write lease
through step 15. Holding the lease through verification is heavier, but it
prevents workers from building sibling or stale-parent generations while a new
pending generation is not yet visible as head.

If this becomes too slow, add a second-phase optimization: a durable
`pending-head-intent` object that other workers must honor before building a
child. Do not start with that complexity.

## Handling Cold Workers

### Scenario: Worker1 serves `tip~3`, Worker2 serves `tip~1`

Assume Worker1 had enough local data to publish a generation, while Worker2 has
an empty or partial `/cache`.

Worker2 should:

1. Check for a complete commit manifest for `tip~1`.
2. If present, hydrate the referenced generation chain from object storage into
   its local repo, verify the commit/tree exists, create a session, and serve.
3. If absent, acquire `repo-write`.
4. Re-read manifests after lease acquisition.
5. If another worker has meanwhile published a complete commit manifest for
   `tip~1`, hydrate and serve without fetching upstream.
6. Otherwise hydrate current generation head before building anything.
7. Fetch upstream for the requested commit/ref.
8. If the hydrated generation already contains the requested commit after fetch
   or local ancestor checks, write the commit manifest pointing at the known
   generation.
9. If not, publish a new generation parented to the hydrated head, or a full
   root generation if the parent cannot be hydrated.

This preserves incremental behavior without assuming Worker2 has Worker1's EBS
state.

In the usual Git notation, `tip~1` is newer than `tip~3`. The common path is
therefore not that Worker1's `tip~3` generation magically contains `tip~1`.
Instead, Worker2 should hydrate the durable `tip~3` generation as the parent,
fetch the newer objects needed for `tip~1`, and publish a child generation. If a
later worker already published `tip~1` while Worker2 was waiting, Worker2 should
reuse that manifest instead of building another generation.

### Required local-cache behavior

- `ensure_repo_dir` may create an empty bare repo.
- `hydrate_generation` must hydrate the full parent chain, oldest to newest.
- Hydration must reserve disk for bundle downloads before writing.
- Failed hydration should invalidate only the local repo, not durable metadata.
- A worker may discard its local repo at any time and rebuild from object store.

## Advanced Case: Two Workers Request Dependent Generations

Consider:

- Worker1 starts building `G1` for `tip~3`.
- Worker2 starts building `G2` for `tip~1`.
- `G2` should ideally parent to `G1`, not to the older head or a sibling chain.

The first implementation does not allow the two workers to truly build dependent
generations concurrently. It serializes writers with `repo-write` and lets the
second worker retry from the new durable head. That is simpler than a
distributed build graph and still produces a linear generation chain.

Correctness-first behavior:

1. Worker1 acquires `repo-write`.
2. Worker2 cannot acquire `repo-write`.
3. Worker2 checks whether the desired commit manifest appeared.
4. If not, Worker2 returns `503`/`Retry-After` or retries after a short delay.
5. Worker1 publishes, verifies, advances head, releases.
6. Worker2 retries, rereads head, hydrates `G1`, and builds `G2` parented to
   `G1`. If a complete commit manifest for `tip~1` appeared while Worker2 was
   waiting, Worker2 serves that durable result instead of building another
   generation.

Crash behavior:

- If Worker1 crashes before pending publish, the lease expires and Worker2
  builds from the last verified head.
- If Worker1 crashes after pending publish but before verification, Worker2
  steals the expired lease, scans pending generations, and either finishes
  verifying `G1` or ignores it until GC if invalid.
- If Worker1 resumes after its lease was stolen, fencing prevents it from
  releasing Worker2's lease or advancing head/ref metadata.

This serializes writers but keeps readers independent.

## Lease Busy API Policy

When a worker cannot acquire the durable repo-write lease:

1. Re-read relevant manifests.
2. If an exact commit answer is now available from a complete commit manifest,
   hydrate and serve it.
3. For strict branch/default requests, do not satisfy the request from another
   worker's timestamped ref observation while the lease is busy. Without
   distributed singleflight or synchronized clocks, that timestamp is not proof
   that upstream was verified after this request began.
4. Return `503` with `Retry-After`, unless the caller explicitly requested a
   future bounded-staleness mode.

This is not distributed singleflight because the waiting worker does not attach
to another worker's in-memory result. It only follows durable metadata if that
metadata appears.

## Compaction And GC

Compaction mutates commit/ref manifests and deletes generation objects, so it
must coordinate with publishers.

Rules:

1. Compaction must acquire `repo-write` or a stronger `repo-maintenance` lease
   that excludes repo writers.
2. Compaction must snapshot current head and pending generations after acquiring
   the lease.
3. It may compact only the verified chain reachable from the snapshotted head.
4. It must not delete any generation referenced by:
   - current head;
   - pending generation parent chains;
   - verified generation manifests not yet repointed;
   - sessions that have not expired.
5. Repoint commit/ref manifests with compare-and-swap against expected old
   generation IDs.
6. Delete old generation objects only after repoints and head advancement
   succeed.

Pending-generation GC should be TTL based and conservative:

- pending manifest without bundle: delete after TTL;
- bundle without pending manifest: delete after TTL;
- pending manifest with valid bundle and available parent chain: prefer
  verification over deletion;
- pending manifest whose parent chain was compacted: verify against compacted
  chain if possible, otherwise leave until explicit repair/GC.

## Pending Generation Recovery Protocol

Pending generations need enough metadata for a different worker to finish or
safely abandon work after a lease expires.

Use a lexically ordered key so bounded scans find older work first:

```text
repos/{repo}/pending-generations/{created_unix_ms}-{generation_id}.json
```

Pending manifest fields:

```json
{
  "schema_version": 1,
  "repo": "github.com/org/repo",
  "generation_id": "uuid-v7",
  "state": "pending",
  "target": "branch main",
  "holder": "worker-uuid/pid/boot-id",
  "lease_token": "uuid-v7",
  "created_at": "...",
  "observed_head": "optional parent generation id",
  "observed_head_version": "object-store version",
  "parent_generation": "optional parent generation id",
  "bundle_key": "repos/.../bundles/<generation>.bundle",
  "bundle_len": 123,
  "bundle_sha256": "...",
  "ref_updates": ["refs/heads/main"],
  "commit_ids": ["..."]
}
```

Recovery after acquiring or stealing `repo-write`:

1. List `repos/{repo}/pending-generations/` with `max_keys = 1000`. Do not use
   unbounded listing. If the result is truncated, process the returned page,
   emit `git_cache_pending_generation_scan_truncated_total`, and continue on a
   later cleanup pass.
2. Skip pending records whose holder still owns a non-expired lease.
3. Read the pending manifest with version metadata.
4. `head` the bundle object and require `bundle_len` to match before
   downloading.
5. Hydrate the parent generation chain, or the compacted replacement chain if a
   compaction manifest explicitly maps the old parent to a new root.
6. Reserve disk, download the bundle to a temp file, hash it, and require
   `bundle_sha256` to match.
7. Apply the bundle into the local repo and run the existing connectivity and
   `git fsck` verification.
8. If valid, publish verified generation metadata and advance head/ref manifests
   only with the current lease token and expected object versions.
9. If invalid, CAS the pending manifest to `state = "aborted"` with a reason and
   leave physical bundle deletion to conservative GC.

This lets Worker2 finish Worker1's work after a crash without guessing from
local disk state, and without loading an unbounded number of object-store keys.

## Execution Map

This section maps the plan to concrete work areas.

### Cross-Cutting Decisions

- New error categories should be explicit: `LeaseBusy`, `LeaseLost`,
  `LeaseStealConflict`, `CasConflict`, `PendingGenerationInvalid`, and
  `ColdHydrationFailed`.
- A `CasConflict` should not try to merge two mutable updates in place. Re-read
  durable state and re-enter the materialization flow.
- Config should use the lease names in this document so staging can tune them
  without code changes.
- Metrics labels should stay low-cardinality: use `result`, `operation`, and
  `backend`, not repo names, commit IDs, lease tokens, or generation IDs.
- Tests should expose deterministic pause points rather than relying on sleeps:
  before lease acquire, after bundle upload, after pending manifest write, before
  verification, and before head/ref CAS.

### Object Store

Files:

- `crates/git-cache-objectstore/src/lib.rs`
- `crates/git-cache-objectstore/src/local.rs`
- `crates/git-cache-objectstore/src/s3.rs`
- `crates/git-cache-objectstore/src/manifests.rs`
- `crates/git-cache-objectstore/src/tests.rs`

Tasks:

- Add version-aware object reads and conditional writes/deletes.
- Preserve the existing create-once helpers for immutable objects.
- Add JSON CAS helpers for mutable manifests and leases.
- Implement stale-version tests for local store and S3-compatible store.
- Ensure `list_prefix` callers that do not require a full listing keep passing
  a bounded `max_keys`.

### Worker Coordination

Files:

- `crates/git-cache-worker/src/lib.rs`
- `crates/git-cache-api/src/lib.rs`
- `crates/git-cache-domain/src/state.rs`

Tasks:

- Implement `ObjectStoreRepoLeaseManager` behind the existing
  `RepoLeaseManager` trait.
- Add holder identity from config or generated process identity at startup.
- Replace `InMemoryRepoLeaseManager` in production API wiring.
- Keep worker-local `inflight` dedupe exactly where it is: per worker,
  per `UpdateKey`, not distributed.
- Add lease-busy behavior that rereads durable manifests before returning
  `503`/`Retry-After`.

### Materialization And Publishing

Files:

- `crates/git-cache-domain/src/materializer.rs`
- `crates/git-cache-objectstore/src/manifests.rs`

Tasks:

- Re-read durable state before and after acquiring `repo-write`.
- Add `hydrate_current_head(repo)` and use it before incremental bundle
  creation.
- Thread lease token/expected head through publish, verification, ref writes,
  and generation-head advancement.
- Replace blind mutable writes with CAS helpers.
- Make conflicts explicit: if the expected head changed, abort and re-enter the
  materialization flow from the top rather than trying to merge state in place.

### Direct Git Remote

Files:

- `crates/git-cache-api/src/lib.rs`
- `crates/git-cache-domain/src/materializer.rs`
- `crates/git-cache-api/tests/git_remote_integration.rs`

Tasks:

- Ensure `/git/{repo}` read-through uses the same durable repo-write lease when
  it must fetch unknown `want`s.
- Keep ref advertisement upstream-verified.
- Keep exact known wants fast when their commit manifests are complete.
- Add cold-worker tests for direct remote clone/fetch, not only
  `/v1/materialize`.

### Compaction And Cleanup

Files:

- `crates/git-cache-domain/src/materializer.rs`
- `crates/git-cache-cli/src/main.rs`
- `docs/deployment.md`

Tasks:

- Require a writer-excluding durable lease before compaction mutates manifests
  or deletes generation objects.
- Snapshot pending generations under the lease.
- Repoint manifests with expected old generation IDs.
- Add a dry-run report for pending/orphan cleanup before enabling deletion in
  production.

### Test Harness

Files:

- `crates/git-cache-domain/src/materializer.rs` tests
- `crates/git-cache-api/tests/*`
- `crates/git-cache-worker/src/lib.rs` tests
- `integration_tests/*`

Tasks:

- Build a reusable two-worker fixture with one shared object store and two
  separate cache roots.
- Add deterministic pause points around lease acquisition, bundle upload,
  pending publish, verification, and head advancement.
- Test both local object store and S3-compatible/MinIO behavior for CAS and
  lease takeover.
- Add regression tests for stale holder fencing.

## Implementation Phases

### Phase 1: CAS-capable object store

- Add object version metadata to `ObjectMeta`.
- Add `get_with_version`, `put_if_version_matches`, and
  `delete_if_version_matches`.
- Implement for local store and S3.
- Add JSON compare-and-swap helpers.
- Test conflicting writers and stale expected versions.

### Phase 2: Durable lease manager

- Implement `ObjectStoreRepoLeaseManager`.
- Add lease token, holder ID, expiry, renewal, release, and expired takeover.
- Replace `InMemoryRepoLeaseManager` in API/worker production wiring.
- Keep `InMemoryRepoLeaseManager` only for focused unit tests.
- Add metrics for acquired, busy, renewed, expired, stolen, and release failure.

### Phase 3: Fenced publish protocol

- Hold repo-write lease through publish, verification, ref writes, and head
  advancement.
- Replace blind head/ref writes with CAS helpers.
- Make ordinary commit manifest writes `if_absent_or_matches`.
- Re-read durable state after lease acquisition and before bundle creation.
- Add explicit conflict/rebase handling when expected head changes.

### Phase 4: Cold-worker hydration and rebase

- Add helper: `hydrate_current_head(repo) -> observed_head`.
- Before incremental bundle creation, hydrate observed parent chain into the
  local repo.
- If parent hydration fails because the parent is unavailable or force-pushed
  away, fall back to a full root generation.
- If another worker already published the target commit, index/serve without a
  new bundle.

### Phase 5: Verification, compaction, and cleanup hardening

- Use per-generation verification leases if duplicate verification becomes too
  expensive; keep head/ref advancement under repo-write.
- Make compaction acquire a writer-excluding durable lease.
- Add conservative orphan/pending-generation GC.
- Ensure old generations are retained while sessions or pending generations may
  still need them.

### Phase 6: Multi-worker test suite

Add integration tests with one shared object store and two separate cache roots:

- same branch requested concurrently by two workers;
- Worker1 warms `tip~3`, Worker2 cold-serves `tip~1`;
- exact known commit is served from Worker2 while upstream is offline;
- two workers attempt dependent generation publishes;
- lease expiry and fencing prevents stale holder writes;
- crash after bundle upload before verification;
- compaction does not delete parents needed by a pending child;
- force-push causes a full root generation when the old parent is unavailable;
- direct `/git/...` read-through uses the same durable lease discipline.

## Observability

Add counters/gauges:

- `git_cache_lease_acquire_total{result}`
- `git_cache_lease_renew_total{result}`
- `git_cache_lease_steal_total`
- `git_cache_repo_write_busy_total`
- `git_cache_generation_publish_total{result}`
- `git_cache_generation_head_cas_conflict_total`
- `git_cache_ref_manifest_cas_conflict_total`
- `git_cache_cold_hydration_total{result}`
- `git_cache_cold_hydration_bytes_total`
- `git_cache_pending_generation_count`
- `git_cache_orphan_bundle_count`

Log fields for every mutating operation:

- repo;
- worker holder ID;
- lease token;
- selector/update target;
- observed head;
- new generation;
- parent generation;
- CAS expected/current result.

## Rollout Plan

1. Ship CAS and durable leases behind a config flag.
2. Enable durable leases in staging with two workers and separate cache roots.
3. Run the multi-worker test suite against local object store and MinIO/S3.
4. Enable durable leases in production while keeping local singleflight.
5. Switch publish/head/ref writes to require durable lease token.
6. Enable compaction only after fenced publish is stable.
7. Add GC last, with dry-run reporting before deletion.

## Acceptance Criteria

- No production path uses an in-memory lease for cross-worker exclusion.
- Exact known-complete commits can be served by a cold worker from object store.
- Two workers cannot advance conflicting generation heads for the same repo.
- A stale lease holder cannot release or mutate after another worker steals its
  lease.
- Concurrent dependent generation requests produce either a single reused
  generation or a linear parent chain, never an accidental sibling overwrite.
- Compaction cannot delete a generation required by an active session or pending
  child generation.
- Local `/cache` loss only causes rehydration or upstream verification, not data
  loss or stale serving.
