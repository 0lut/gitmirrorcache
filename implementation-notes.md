# Implementation Notes: Multi-Worker Conflict Avoidance

## Running notes for PR #47 implementation

### Decisions not explicit in the spec

1. Implemented local object-store CAS with sidecar version files plus per-key
   lock files under the local store root. This keeps the same object-store API
   shape as S3 and exercises stale-version paths locally without adding a new
   filesystem-lock dependency.
2. S3 CAS uses ETag/version metadata as an opaque `ObjectVersion` token and
   conditional `If-Match` writes. Large bundle objects still use immutable
   create-once semantics; CAS is intended for small mutable JSON records.
3. Production API wiring now uses an object-store-backed `repo-write` lease via
   the existing worker `RepoLeaseManager` trait. The in-memory lease manager is
   retained for focused unit tests.
4. `repo-write` lease release is fenced by token and implemented as a conditional
   transition to `released_at` instead of an unconditional delete. Cleanup can
   safely delete released records later.
5. Generation-head advancement during pending-generation verification re-reads
   the actual current head, skips stale/out-of-order verification completions,
   and CASes forward only when the observed head is older than the verified
   generation.

### Tradeoffs / follow-ups

1. The implementation keeps asynchronous background verification for already
   published pending generations. Verification no longer uses wall-clock
   timestamps as conflict winners, and stale head advances lose CAS rather than
   replacing newer durable state.
2. Direct `/git/...` unknown-want read-through now holds the durable
   `repo-write` lease while `ensure_wants_available()` runs and passes the lease
   token into the materializer. This keeps fetch/publish under the same fencing
   path as `/v1/materialize`.
3. Compaction now checks the generation-head CAS result before cleanup and aborts
   deletion/repointing when another worker wins the head race.

### Hardening pass (review feedback)

1. **S3 CAS token always uses ETag.** `object_version()` previously preferred
   `VersionId` over `ETag`, but `put_if_version_matches`/`delete_if_version_matches`
   use S3 `If-Match` which compares ETags. On versioned buckets VersionId ≠ ETag,
   so every CAS operation would silently fail. Fixed to always use ETag.

2. **Local CAS crash consistency.** `put_inner` previously wrote the data file
   (rename) *before* the version sidecar token. A crash between rename and
   `write_version` left new bytes guarded by the old token — a stale CAS holder
   could overwrite data it never read. Fixed: version token is now written
   *before* data rename. A crash between version write and data rename leaves a
   new token guarding old bytes; any CAS holder with the previous token fails.

3. **Ref manifest conflict resolution uses generation ordering.** Previously
   `write_ref_manifest` and `write_default_ref_manifest` compared `verified_at`
   wall-clock timestamps to resolve concurrent writes. A skewed/stale verifier
   could write an older commit with a later timestamp. Replaced with monotonic
   `GenerationId` ordering (UUID v7, time-sortable). No wall-clock dependency.

4. **Compaction aborts on head CAS loss.** `compact_generation_chain_inner`
   previously discarded the return value of `advance_generation_head` (`let _ =
   ...`). If another worker advanced the head, compaction still repointed
   manifests and deleted old generations. Now aborts cleanup and returns `None`
   when the CAS fails.

5. **Lease guard Drop aborts renewal task.** `ObjectStoreRepoLease` now
   implements `Drop` to abort the renewal `JoinHandle`. Previously a panic or
   early return could leak the renewal task indefinitely.

6. **Clock-skew-resistant lease steal.** Lease expiry check now uses the
   object-store `updated_at` metadata (server-side timestamp) when available
   instead of comparing holder-authored `expires_at` with the local clock.
   This avoids inter-worker clock skew causing premature lease theft.

7. **Retry-After header on lease busy.** API 503 responses for `LeaseBusy` now
   include a `Retry-After` header populated from `config.leases.busy_retry_after_seconds`.
   API handlers also wait/retry for up to this configured window before returning
   503 so transient multi-worker contention does not unnecessarily fail requests.

8. **All materialize selectors routed through coordinator.** Previously only
   `Branch`/`DefaultBranch` selectors went through `UpdateCoordinator`;
   `Commit`/`ShortCommit` bypassed coordination entirely. Now all selectors
   acquire the repo-write lease before materializing.

9. **Direct `/git/` upload-pack holds repo-write lease.** The
   `handle_upload_pack` → `ensure_wants_available` path can fetch and publish
   generations. The API now acquires and holds the durable repo-write lease
   while processing upload-pack wants and constructs the materializer with that
   lease token, so any `publish_generation` calls verify ownership before
   mutating shared state.

10. **Lease token threaded through materializer.** `UpdateRequest` now carries
    an optional `lease_token` set by `execute_with_lease`. `Materializer` stores
    the token and calls `verify_lease_held()` before publishing shared state.
    `RepoLease` trait exposes `token()` to surface the fencing token.

---

# Implementation Notes: Read-Through Git Remote

## Overview
Implementing the spec from `git-api-plan.md` (PR #13): a read-through Smart HTTP
remote that allows `git clone`/`git fetch` directly against the cache server
without needing a prior `/v1/materialize` call.

---

## Decisions not in the spec

### 1. Reusing the existing bare cache repo as the served repo
The spec says "Start with the existing bare cache repo as the served repo" and
only introduce a separate served bare repo if ref isolation becomes necessary.
We follow this guidance: the direct Git remote serves from the same
`cache_root/repos/{host}/{owner}/{repo}.git` bare repo that the materializer
already manages. Public `refs/heads/*` and `HEAD` are written into the same
repo. Internal `refs/cache/*` are hidden via `git config` on the repo.

### 2. Streaming upload-pack via Axum's `StreamBody`
The spec requires streaming pack output. The existing `upload_pack_stateless_rpc`
buffers stdout into a `Vec<u8>`. For the direct remote, we add a new
`upload_pack_stream` method on the `Git` wrapper that returns an async reader
(the child process stdout) instead of buffering. The API layer wraps this in
an Axum `Body::from_stream()`.  We keep the old buffered method for the session
endpoint for backward compatibility.

### 3. Buffered ref advertisement
`git upload-pack --advertise-refs` output is typically small (a few KB). We
reuse the existing buffered `advertise_refs()` helper and prepend the
`# service=git-upload-pack` header in-memory (a fixed 34-byte prefix).
We initially tried streaming but the child process lifetime issue (see T2)
made buffering the simpler and more correct approach for this small payload.

### 4. Config structure for `git_remote`
The spec suggests a `[git_remote]` TOML section. We add a `GitRemoteConfig`
struct with `enabled: bool` (default `false`). Branch ref check policy and
commit read-through behavior follow the spec defaults. The config is optional;
when absent, the direct remote is disabled and the router doesn't register
the `/git/{*repo_path}` route.

### 5. Upstream ref comparison uses `git ls-remote --heads --symref`
For comparing upstream refs before advertising, we use a single
`git ls-remote --heads --symref <upstream>` call. This gets all branch heads
and the symbolic HEAD in one round-trip. We parse both symrefs and branch SHAs
from the output.

### 6. Public ref sync strategy
After verifying upstream refs, we update the bare cache repo's public refs:
- `refs/heads/<branch>` → verified commit SHA
- `HEAD` → symref to default branch
We use `git update-ref` for branch heads and `git symbolic-ref HEAD` for the
default branch. This is done inside the lease to prevent races.

### 7. Concurrency: direct domain-layer synchronization
The spec says concurrent requests should wait for the same upstream work. For the
direct Git remote, we bypass the `UpdateCoordinator` and call the domain's
`ensure_repo_advertisable()` directly. This method does upstream ref comparison
and targeted fetch internally, which is simpler than routing through the
coordinator for the branch-comparison-then-fetch flow. The coordinator remains
in use for the existing `/v1/materialize` endpoint.

### 8. Want-line parsing
For `POST /git-upload-pack`, we parse `want <oid>` lines from the pkt-line
formatted request body. We check each wanted OID against local objects and
object-store manifests, hydrating as needed before invoking upload-pack.

### 9. `uploadpack.allowAnySHA1InWant=true`
We configure this on all served repos so that `git fetch origin <sha>` works.
The spec notes this should only be on "validated, allowlisted repos" — our
repos are already validated via `allowed_upstream_hosts`, so this is safe.

### 10. Integration tests against real public repos
The spec mentions tests with `git clone --branch main`, `git fetch <sha>`, etc.
We add integration tests that spin up the Axum server and run real git commands.
For high-commit repos, we looked for public repos with 200k+ commits.

Candidates:
- `astral-sh/uv` — already in the test suite
- `torvalds/linux` — ~1.2M+ commits, the canonical huge repo
- `chromium/chromium` — extremely large, but impractical for CI (very slow)
- `gcc-mirror/gcc` — ~300k+ commits
- `llvm/llvm-project` — ~500k+ commits

We use `torvalds/linux` and `llvm/llvm-project` as the high-commit test targets.
For CI practicality, we do shallow operations (ls-remote, single-branch fetch)
rather than full clones.

---

## Tradeoffs

### T1. No separate served repo
Using the same bare repo for internal cache refs and public refs is simpler but
means we need `hideRefs` config to prevent leaking `refs/cache/*`. If future
features need stronger isolation, we'll need to refactor to use alternates.

### T2. Buffered advertise-refs, streaming upload-pack
Initially we tried streaming both `advertise-refs` and `upload-pack` output.
However, child process lifetime management caused issues: the `UploadPackProcess`
struct holds the child with `kill_on_drop(true)`, and when it drops at the end of
the handler scope, the child is killed before the response stream is consumed by
the client. For `advertise-refs` (output is small — just ref lines) we switched
to the existing buffered `advertise_refs()` helper. For `upload-pack` (output can
be arbitrarily large pack data), we use a `ChildGuardStream` wrapper that holds
the child process handle alongside the `ReaderStream`, keeping the process alive
for the duration of the HTTP response body.

### T2b. Multi-threaded tokio runtime needed for git integration tests
`#[tokio::test]` uses a single-threaded runtime by default. Because the tests
run blocking `git` CLI commands via `std::process::Command::output()`, they block
the only tokio thread and starve the Axum server spawned on the same runtime.
We use `#[tokio::test(flavor = "multi_thread")]` and `spawn_blocking` for all
git CLI calls in integration tests.

### T3. Always-compare branch policy
The spec explicitly chose "branches always latest" over TTL-based staleness.
This means every branch clone/fetch does at least one `ls-remote` round-trip to
GitHub. For high-traffic scenarios, the single-flight dedup amortizes this.

### T4. Full `ls-remote --heads` rather than per-branch
We compare all branch heads in one `ls-remote` call rather than checking
individual branches. This is slightly more data transferred but avoids multiple
round-trips and lets us update all stale branches in one fetch pass.

---

## Things I changed from existing code

### C1. New `Git::upload_pack_stream` method
Added alongside the existing buffered `upload_pack_stateless_rpc`. Returns a
streaming reader + child process handle for the API to manage.

### C2. New `Git::ls_remote_heads` method
Returns parsed branch→SHA map from `git ls-remote --heads --symref`.

### C3. `GitRemoteConfig` added to `AppConfig`
Optional field with `#[serde(default)]`. Zero impact on existing configs.

### C4. Direct route added conditionally
The `/git/{*repo_path}` route is only registered when `git_remote.enabled = true`.
Existing deployments are unaffected.

---

# Implementation Notes: Incremental Generations & Compaction

## Overview
Implementing the plan from PR #23: generation publishing now creates delta
bundles when a repository has a previous generation head, and generation chains
can be compacted into a new full-bundle root generation.

## Decisions not in the spec

### 1. Keep old notes and append this section
PR #23's diff deleted the prior `implementation-notes.md`, but the request for
this implementation explicitly asked for a running notes file. I preserved the
previous notes and appended a new section instead of replacing the file.

### 2. Compaction API returns `CompactionReport`
The plan's contract summary listed `compact_generation_chain` as returning
`Option<GenerationId>`, while the detailed section introduced `CompactionReport`.
I used `Option<CompactionReport>` for the domain API so callers get the new
generation plus old depth, old generations, and approximate bytes reclaimed.

### 3. Dry-run compaction reserves a synthetic generation ID
`git-cache compact --dry-run` reports the generation ID that would be used if it
ran now. It does not write bundles or manifests. Running the real compaction
later will allocate a different UUIDv7.

### 4. Repointing current manifests only
Compaction rewrites canonical commit manifests, canonical ref manifests, and the
default-branch manifest when they point at a compacted generation. Historical
`ref-updates` observation manifests are left intact because they are append-only
audit/history records keyed by the old generation.

### 5. Commit list ordering during compaction
Hydration walks chains from head to root, but compacted manifests store commits
in root-to-head order for readability and deterministic tests.

## Tradeoffs

### T1. Approximate `bytes_reclaimed`
The object-store trait does not expose metadata, so compaction computes
`bytes_reclaimed` by reading old bundles and summing their byte lengths. This is
accurate for local/S3 object bytes but costs extra reads during compaction.

### T2. Head update after generation publish
The generation bundle and per-commit/ref manifests are published through the
existing atomic-ish `GenerationPublish` helper, then `RepoGenerationHead` is
written afterward. If the final head write fails, the generation still exists and
can be found by commit/ref manifests; the next publish treats the old head as the
delta base and may duplicate some objects rather than corrupting state.

### T3. Fallback resets the chain
When incremental bundle creation fails, the code deletes any partial bundle file,
creates a full bundle, sets `parent_generation: None`, and resets
`tip_commits` to the current commit. This favors safe recovery over preserving
the old chain when the local hot cache cannot prove it has the old tips.

## Things I changed from the plan

### C1. CLI `--all` enumerates generation-head manifests
There is no repository registry, so `git-cache compact --all` lists
`repos/*/manifests/generation-head.json` in the object store and compacts those
repos.

### C2. Inline compaction ignores "nothing to compact"
When `[compaction].inline = true`, publish calls compaction after writing the new
head and treats `Ok(None)` as a no-op. Errors still propagate because inline
compaction is part of the publish path when enabled.

### C3. `Git::run` already supports dynamic args
The git wrapper's `run` accepts any `IntoIterator<Item = impl AsRef<OsStr>>`, so
no separate `run_vec` helper was needed for `bundle_create_incremental`.

---

# Implementation Notes: Verified S3 Generations and Deferred Fsck

## Overview

We are changing the cache contract so clone/materialize requests do not block on
full-repo `git fsck --connectivity-only`. Durable S3 cache data is trusted only
when there is a verified sidecar manifest for the exact bundle bytes. Request
paths should serve from local/upstream-fetched commits immediately, then enqueue
background verification/publication for future cold workers.

## Decisions not in the spec

### 1. Use a verified sidecar instead of rewriting generation manifests

I added `VerifiedGenerationManifest` as a sidecar at
`repos/{repo}/generations/{generation}/verified.json`. This lets durable commit
and ref manifests mean "verified" while keeping existing generation manifest
shape mostly stable during the refactor.

### 2. Check SHA-256 from disk after object-store download

Hydration now downloads the bundle to disk, computes SHA-256 with a bounded
1 MiB buffer, checks byte length and digest, then runs `git fetch` from the
bundle. This avoids loading large bundles into memory and follows the repo's
bounded-allocation rules.

### 3. Keep one publish-time fsck temporarily

The first implementation slice still keeps `publish_generation`'s fsck because
that is currently what makes the new verified sidecar truthful. The remaining
work is to move that verification into an async background publisher so the
request path no longer pays for it.

### 4. Create `TODOs.md` as active tracker

The repo already had `TODO.md`, but the request explicitly asked to refresh
`TODOs.md`. I created `TODOs.md` as the active tracker for this local-agent
work rather than rewriting the historical milestone file.

## Tradeoffs

### T1. Runtime applies a physical v2 object-store namespace

The code still uses existing logical object keys (`repos/...`,
`pending-generations/...`) inside the object-store adapter. Runtime store wiring
now adds a physical v2 suffix to the configured namespace so a deploy
automatically writes to a clean keyspace:

- local root `./tmp/object-store` resolves to `./tmp/object-store-v2`;
- S3 prefix `repos` resolves to `repos-v2`;
- already-suffixed values such as `repos-v2` are left unchanged.

### T2. Verified hydrate rejects missing sidecars

Cold hydration now requires `verified.json`. That is intentional for the clean
v2 model: unverified S3 data should not be rescued by request-path fsck. In a
fresh v2 deployment, missing verified manifests will cause fallback paths to use
upstream/local fetch and enqueue publication.

## Final deferred-fsck implementation notes

### 5. Pending generation publishes are verifier work items

The request path now uploads the generated bundle and writes a pending work item
under `pending-generations/{repo}/{generation}.json`. It does **not** write
canonical generation, commit, ref, default, or generation-head manifests. This
keeps the v2 invariant clear: canonical manifests are durable verified state;
pending publishes are internal work queue state.

### 6. Canonical manifests are verifier-gated

The background verifier downloads the pending bundle chain into a scratch bare
repo, fetches each bundle, runs `git fsck --connectivity-only`, computes bundle
length/SHA-256, writes `verified.json`, then publishes canonical generation,
commit/ref/default, and generation-head manifests. Hydration reads only canonical
generation manifests and still requires the verified sidecar.

### 7. Request paths serve local refs instead of waiting for manifests

Branch/default materialization after upstream validation now creates sessions
from the locally fetched `refs/cache/upstream/heads/*` ref. This preserves fast
request responses while durable manifests appear asynchronously after verifier
success.

### 8. Bounded background verification

A new `max_concurrent_generation_verifications` config field bounds concurrent
background verifier tasks with a semaphore. The default is `1` to avoid multiple
large-repo fscks competing for CPU/disk IO. Example configs were updated.

### 9. Verification publication ordering

For a pending generation, the verifier publishes the verified sidecar before the
canonical generation manifest, then commit/ref manifests, then generation-head.
Tests that assert generation-head state now wait for the head because commit
manifests can become visible slightly before the head write.

### 10. Compaction follows the same pending path

Compaction no longer writes an unverified canonical generation manifest. It
publishes its compacted bundle as a pending generation, runs verification, then
repoints canonical manifests and updates the head.

## Tradeoffs added during final implementation

### T3. Startup pending scan resumes interrupted verification

The verifier is enqueue-on-publish, and API startup also scans the global
`pending-generations/` prefix and re-enqueues any leftover pending generations.
This covers process exits after the request path writes pending metadata but
before background verification finishes. The scan is bounded to avoid unbounded
object-store listings.

### T4. Configured object-store namespace remains the base name

Operators can keep existing config values such as `prefix = "repos"` or
`root = "./tmp/object-store"`. The domain state builder appends the v2 suffix at
runtime and avoids double-suffixing if a config already contains `-v2` or `v2`.

### T5. Verification head advancement is monotonic but tolerant of races

Background verification no longer treats every generation-head CAS miss as a
hard error. Verifier tasks can complete out of order, so the verifier re-reads
the current head and only attempts to advance when the current head is older
than the pending generation. A strict `>` timestamp check is used instead of
`>=` because tests can publish multiple generations within the same timestamp
tick; equal timestamps must still allow CAS advancement.

### T6. Mutable ref/default manifests use versioned writes

Ref manifests and the default manifest now use object-store versions for writes
instead of blind overwrites. If a manifest with a strictly newer verification
timestamp already exists, the older write is skipped. This is a pragmatic
fencing guard for mutable branch/default state while preserving idempotent
rewrites of identical content.

### T7. Exact-commit reads can join pending verification

The request path can return from branch/default materialization before the
background verifier has published canonical commit manifests. Exact-commit
materialization now scans the repo's bounded pending-generation work queue for a
matching commit and waits for that generation verification before falling back to
publishing a new generation. This avoids duplicate generation publication during
cold-cache rehydrate races.

### T8. Synthetic git performance repos disable auto maintenance

The `git-cache-git` performance tests create hundreds of commits in temporary
repositories while other tests run concurrently. CI exposed intermittent
repository corruption from Git auto maintenance/gc during those synthetic setup
loops, so the test fixture disables `gc.auto` and `maintenance.auto` for those
temporary repos.
