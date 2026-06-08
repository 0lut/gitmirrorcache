# Exact Commit Materialize Hydration Plan

## Problem

Exact commit `/v1/materialize` can be slow for large repositories such as
`github.com/llvm/llvm-project` even when the object store already has verified
generations for the repo.

The current main-branch path in
`crates/git-cache-domain/src/materializer/planning.rs` is:

1. Check for a complete commit manifest for the exact requested SHA.
2. If the local bare repo already has the commit, try to index it as an ancestor
   of a known generation tip.
3. Otherwise fetch all upstream heads.
4. If the commit is now present, try the same local ancestry indexing again.
5. If no known generation applies, publish a new generation.

That means existing S3 generations only help exact commit materialize when the
exact commit is already indexed or the local EBS repo already contains enough
objects to prove ancestry. A cold local repo with a valid verified generation in
S3 can still fall through to `fetch_all_refs`, which is extremely expensive for
LLVM.

## Goal

Before exact commit materialize performs a broad upstream fetch, try to use the
latest verified generation already stored in the object store.

For a request such as `tip~5` after `main` has recently been materialized:

```text
commit manifest missing
local repo cold or incomplete
repo generation head exists in S3
hydrate generation head
prove requested commit is ancestor of a known tip
write commit manifest for requested commit
return cache_verified
```

This should make `tip~5 -> tip~3` exact materialize behave like an incremental
cache lookup instead of a large upstream fetch.

## Proposed Shape

Add one helper near the existing exact-commit planning helpers:

```text
try_index_exact_commit_from_hydrated_generation(repo, repo_dir, commit)
    -> Option<GenerationId>
```

The helper should:

1. Load `repo_head` from manifests.
2. If no generation head exists, return `None`.
3. Hydrate `repo_head.generation` into the local repo using the existing
   `hydrate_generation` path.
4. Reuse `index_local_commit_from_known_generation` to prove the requested
   commit is an ancestor of a verified generation tip and write the commit
   manifest.
5. Return the generation id on success.

`ensure_exact_commit` should call this helper after the cheap local check and
before `fetch_all_refs`.

The intended order becomes:

```text
complete commit manifest
local repo already has commit + known-generation ancestry
hydrate latest verified generation + known-generation ancestry
fetch all upstream refs
post-fetch known-generation ancestry
publish new exact-commit generation
```

## Correctness Rules

- Do not trust generation metadata alone. The requested SHA is reusable only
  after Git proves `requested_commit` is an ancestor of a known verified
  generation tip in the hydrated local repo.
- Hydrate only verified generations. `hydrate_generation` already requires
  verified generation metadata, checks object-store bundle size, verifies
  bundle SHA-256, and fetches the bundle into the local bare repo.
- Do not create a commit manifest merely because a commit appears in a
  generation manifest's `commits` list. The local Git ancestry proof remains the
  authorization and integrity boundary.
- If hydration fails because the generation bundle or verification metadata is
  missing, fall back to the existing upstream path unless the error indicates
  local corruption that should surface.
- Preserve the current behavior for repos with no generation head.

## Performance Rules

- The new path should be bounded to the latest repo generation head first. Do
  not scan every generation or every commit manifest on the hot request path.
- Hydrating a generation is still potentially expensive, but it should be much
  cheaper and more predictable than `fetch_all_refs` for large repos whose
  generation bundles already exist in S3.
- If the local repo is already warm, the current cheap local ancestry path should
  remain first and should avoid any S3 downloads.
- Add timing logs around:
  - loading the repo generation head;
  - hydrating the generation head;
  - proving/indexing the requested commit after hydration;
  - falling through to upstream fetch.

## Tests

Add focused domain tests in
`crates/git-cache-domain/src/materializer/tests/generation_tests.rs`.

### 1. Cold Local Repo Reuses Hydrated Generation

Setup:

1. Create commits `A`, `B`, `C`.
2. Materialize branch `main` at `C`.
3. Wait for commit manifest and verified generation for `C`.
4. Delete only the local cached bare repo directory, leaving object-store
   manifests and bundles intact.
5. Materialize exact commit `A`.

Assertions:

- Response source is `cache_verified`.
- Commit is `A`.
- A commit manifest for `A` is written and points at `C`'s generation.
- No new generation bundle is written.
- No upstream fetch is required.

### 2. Missing Generation Head Falls Back

Setup:

1. Request an exact commit for a repo with no generation head.

Assertions:

- Behavior matches current main.
- The helper returns `None`.
- Existing upstream fetch/publish path remains active.

### 3. Hydrated Generation Without Ancestry Falls Back

Setup:

1. Create a verified generation for one line of history.
2. Request a commit that is not an ancestor of any known generation tip.

Assertions:

- No commit manifest is written for the unrelated commit.
- The path falls through to existing upstream validation.

### 4. Corrupt Verified Generation Surfaces Safely

Setup:

1. Create a verified generation, then corrupt/delete its bundle or verification
   metadata.
2. Request an ancestor commit that would require hydration.

Assertions:

- Bundle checksum or missing verified metadata errors do not panic.
- Decide during implementation whether to fall back to upstream or return the
  corruption error. Prefer returning corruption when verification metadata
  exists but the bundle is invalid, because silently fetching upstream would hide
  durable-cache corruption.

## Runtime Validation

After implementation and deploy:

1. Use the side-by-side materialize script:

   ```sh
   python ~/dev/compare_materialize_incremental.py --runs 1 --offsets 5 3
   ```

2. Expected result after warming `main` branch:

   ```text
   tip~5: cache_verified, materially faster than previous timeout
   tip~3: cache_verified, no new broad upstream fetch
   ```

3. Check API logs for the new timing events. The successful path should show:

   ```text
   exact commit generation-head hydrate started
   hydrate generation finished
   indexed exact commit from hydrated generation
   ```

   It should not show `fetch_all_refs` for the same request.

4. Compare main and preview against LLVM. Preview should avoid the multi-minute
   exact-commit timeout when a recent verified generation already exists.

## Non-Goals

- Do not change direct Git read-through behavior in this optimization.
- Do not introduce GitHub REST API visibility checks.
- Do not scan all historical generations on every exact commit request.
- Do not weaken commit readiness checks for sessions.
- Do not change auth policy. This is a cache/materialization optimization only.

## Open Questions

- Should a corrupt verified generation fall back to upstream or fail loudly?
  My preference is fail loudly for checksum/verification mismatches, but fall
  back for an absent generation head.
- Should the helper try only `repo_head.generation`, or also try a bounded number
  of generation-head tips if the head is stale? Start with only the current head;
  broaden only if tests or production data show a need.
- Should we add a metric counter for "exact commit indexed from hydrated
  generation"? Logs are enough for the first pass, but a counter would make this
  easier to verify in production.
