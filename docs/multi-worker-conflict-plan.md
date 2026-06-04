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

## Object-Store CAS Requirements

The object-store trait should grow compare-and-swap support. S3 can implement it
with ETag/version conditions; the local adapter can implement it with lock files
or atomic compare-then-rename in a critical section.

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
only by a holder of the repo-write lease. The write helper should reject older
observations:

- if existing `verified_at` is newer, keep existing;
- if existing has the same `verified_at` and different commit, return conflict;
- otherwise write the new verified observation.

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
5. Record `request_started_at`, `lease_token`, and `observed_head`.
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
5. If Worker1's generation now has a manifest that covers `tip~1`, hydrate and
   serve without fetching upstream.
6. Otherwise hydrate current generation head before building anything.
7. Fetch upstream for the requested commit/ref.
8. If the hydrated generation already contains the requested commit after fetch
   or local ancestor checks, write the commit manifest pointing at the known
   generation.
9. If not, publish a new generation parented to the hydrated head, or a full
   root generation if the parent cannot be hydrated.

This preserves incremental behavior without assuming Worker2 has Worker1's EBS
state.

### Required local-cache behavior

- `ensure_repo_dir` may create an empty bare repo.
- `hydrate_generation` must hydrate the full parent chain, oldest to newest.
- Hydration must reserve disk for bundle downloads before writing.
- Failed hydration should invalidate only the local repo, not durable metadata.
- A worker may discard its local repo at any time and rebuild from object store.

## Advanced Case: Two Workers Building Dependent Generations

Consider:

- Worker1 starts building `G1` for `tip~3`.
- Worker2 starts building `G2` for `tip~1`.
- `G2` should ideally parent to `G1`, not to the older head or a sibling chain.

Correctness-first behavior:

1. Worker1 acquires `repo-write`.
2. Worker2 cannot acquire `repo-write`.
3. Worker2 checks whether the desired commit manifest appeared.
4. If not, Worker2 returns `503`/`Retry-After` or retries after a short delay.
5. Worker1 publishes, verifies, advances head, releases.
6. Worker2 retries, rereads head, hydrates `G1`, and either:
   - serves `tip~1` from `G1` if the generation contains it; or
   - builds `G2` parented to `G1`.

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
2. If the answer is now available from verified cache, serve it.
3. For strict branch/default requests, only use a ref observation whose
   `verified_at >= request_started_at` and whose selector matches the request.
4. Otherwise return `503` with `Retry-After`.

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
