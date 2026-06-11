# Active TODOs: Verified S3 Generations and Non-Blocking Request Paths

## Goal

Ensure `git fsck --connectivity-only` never blocks clone/materialize request paths while keeping durable S3 cache data safe by only hydrating bundles that have explicit verified metadata.

## Completed

- [x] Add `VerifiedGenerationManifest` core type.
- [x] Add verified generation sidecar keys/read/write helpers.
- [x] Add checksum-aware S3/local bundle hydration.
- [x] Require `verified.json` before hydrating any durable generation bundle.
- [x] Verify bundle length and SHA-256 before `git fetch` from bundle.
- [x] Remove hydrate-time `git fsck`.
- [x] Remove redundant request-path fscks before publish/fetch flows.
- [x] Move the remaining `publish_generation` fsck into a background verifier task.
- [x] Add pending generation publish records as verifier work items.
- [x] Ensure canonical generation/commit/ref/head manifests are written only after background fsck succeeds.
- [x] Keep branch/default request responses fast by serving from locally fetched refs rather than waiting for durable manifests.
- [x] Enqueue background verification after branch/default materialize, exact commit materialize, direct Git read-through fetches, changed-ref fetches, and compaction publish.
- [x] Bound concurrent generation verification with `max_concurrent_generation_verifications`.
- [x] Recover pending generations request-driven: foreground materialize and direct Git want handling check pending metadata before fetching upstream (the earlier startup scanner was removed; see `implementation-notes.md` tradeoff T3).
- [x] Add automatic v2 object-store namespace suffixing for local and S3 backends.
- [x] Add/update tests for verified sidecars, background verification, checksum hydrate, cold hydrate, compaction, branch/default refs, and manifest publication ordering.
- [x] Update example configs with the verifier concurrency knob.

## Remaining follow-up hardening

- [ ] Add explicit metrics/timers for enqueue, background verify, fsck, bundle creation, object-store upload, hydrate checksum, and request-path cache-miss reason.
- [ ] Add admin cleanup instructions for old S3/local cache data after v2 rollout.
