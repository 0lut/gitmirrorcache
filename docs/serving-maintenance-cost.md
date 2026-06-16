# Serving Maintenance Cost & Lock Contention

**Status:** findings + proposed work (not implemented). Pick-up-ready for an
implementing agent.

Background serving maintenance (`repack` + `commit-graph`) rewrites the
**entire** cached repo every time it runs, and it holds the per-repo mutation
lock for that whole duration. On a large hot repo (e.g. `torvalds/linux`) that
is minutes of work, during which **cache-miss read-through fetches block**.
This is a scaling cliff, not a correctness bug. See the sibling doc
[`cold-read-through-performance.md`](./cold-read-through-performance.md) for the
related cold-serve analysis.

## Evidence (dev stack, image `0.0.9`)

Isolated dev measurements of proxy-off `git fetch --deepen=40` on
`github.com/torvalds/linux`, against the ALB origin (no Cloudflare), with the
cache reset to a clean slate first:

| scenario | client total | server-side want-prep | notes |
|----------|-------------:|----------------------:|-------|
| **MISS** (read-through extends boundary) | **10 s** | 5324 ms | fetch + `index-pack` + serve |
| **HIT** (already cached, served locally) | **4 s** | 693 ms | no fetch; `upload-pack` serve only |
| **MISS, contended** (prod, behind a repack) | **2138 s** | — | blocked on the mutation lock |

The same serving maintenance (`repack -a -d` + `commit-graph write --reachable`)
finished in **531 ms** on the small freshly-reset dev repo, but ran for
**minutes** on a prod `linux` cache that had been deepened to ~233k commits /
400+ MiB. The 2138 s "deepen" was almost entirely time spent **waiting for the
lock** behind that repack — not deepen work. Contention severity scales with
**repo size**, not with the request.

## Root cause

- `Git::repack_for_serving` runs **`git -c repack.writeBitmaps=false repack -a -d`**
  — a full rewrite of every object into a single pack. `O(repo size)`.
  (`crates/git-cache-git/src/lib.rs`, `repack_for_serving`.)
- `Git::commit_graph_write` runs **`git commit-graph write --reachable`** — a
  full graph rewrite over all reachable commits. `O(all commits)`.
  (`crates/git-cache-git/src/lib.rs`, `commit_graph_write`.)
- `Materializer::enqueue_serving_maintenance` runs both **while holding
  `lock_repo_mutation`** for the repo, debounced ~60 s after hydration.
  (`crates/git-cache-domain/src/materializer/direct_git.rs`.)
- `lock_repo_mutation` is the same lock a read-through fetch takes before
  mutating the boundary, so a MISS that needs the lock waits for the entire
  repack. (`crates/git-cache-domain/src/materializer/repo.rs`; the lock exists
  to keep concurrent fetch/import/repack from colliding on git lock files such
  as `shallow.lock` — see PR #126 / [`AGENTS.md`](../AGENTS.md).)

## What is and isn't blocked (important)

The cache is **not** fully unusable during maintenance:

- **HIT serves do not take `lock_repo_mutation`.** When the wanted history is
  already cached, `ensure_wants_read_through` finds no pending fetch and spawns
  `git upload-pack` directly. These keep serving during maintenance. So *all
  already-cached clones/deepens remain servable* — only the **read-through /
  cache-miss** path (cold clones, boundary-extending deepens, tip-shift
  refetches) blocks on the lock.
- The remaining drag on HIT serves during a heavy `repack -a -d` is **IO/CPU
  contention** plus a brief "pack vanished, retrying" window as redundant packs
  are deleted — they get *slower*, not blocked.

So the goal is: keep the lock-hold tiny, and stop blocking read-through fetches
behind cosmetic packing.

## Proposed work (ranked by leverage)

### 1. Make maintenance `O(new data)` instead of `O(repo)` — highest leverage

Swap the two full rewrites for incremental forms so the lock is held for
seconds regardless of repo size:

- **Geometric repack + multi-pack-index** instead of `-a -d`:
  ```
  git -c repack.writeBitmaps=false repack --geometric=2 -d --write-midx
  ```
  Geometric repacking only rolls up small/new packs to maintain a size
  progression and registers a multi-pack-index (MIDX); it does not rewrite the
  large base pack. The MIDX lets the repo serve efficiently from many packs, so
  collapsing to one pack is unnecessary. Cost ∝ newly-fetched packs.
- **Split commit-graph** instead of full `--reachable`:
  ```
  git commit-graph write --reachable --split
  ```
  Appends a layer for new commits; reads stay fast via the chain.

**Files:** `crates/git-cache-git/src/lib.rs` (`repack_for_serving`,
`commit_graph_write`). Keep `repack.writeBitmaps=false`.

**Expected effect:** prod `linux` maintenance from minutes → low seconds;
MISS-block window shrinks to near-zero **without changing the locking model**.

**Risks / gates:**
- Requires **git ≥ 2.32** for `--geometric`/`repack --write-midx` (MIDX bitmaps
  if ever added need ≥ 2.34). The deploy image installs git from the git-core
  PPA **without pinning** — assert/pin the version first, exactly as
  [`cold-read-through-performance.md`](./cold-read-through-performance.md)
  flags for `pack.allowPackReuse=multi`.
- Confirm `core.multiPackIndex=true` (modern default) and that `.rev`/MIDX
  reverse-index reads still satisfy the served upload-pack config.

### 2. Gate maintenance on actual need

`enqueue_serving_maintenance` currently arms after every hydration. Skip the
repack unless it will help — e.g. pack count above the geometric target or
loose-object count over a threshold (`git count-objects -v`). Avoids re-walking
an already-tidy repo. **File:** `enqueue_serving_maintenance` in
`crates/git-cache-domain/src/materializer/direct_git.rs`.

### 3. Deprioritize the maintenance subprocess

Even when it runs, keep it off the serving hot path: launch repack/commit-graph
with `nice`/`ionice` and bound `pack.threads`, so concurrent HIT serves keep
their IO/CPU. **File:** the spawn path in `crates/git-cache-git/src/lib.rs`.

### 4. Decouple read-through fetch from cosmetic maintenance (only if 1–3 fall short)

A read-through fetch needs new objects *present*, not optimally packed.
Blocking it behind a cosmetic repack is the actual waste. Options:
- Have maintenance **try-lock / yield**: if a read-through is waiting (or the
  repack has already run longer than a budget), abort/defer and let the fetch
  proceed; repack later.
- Or split the lock: serialize boundary-mutating ops (fetch/deepen ↔ each
  other — git already guards these via `shallow.lock`) separately from
  pack-optimizing maintenance, relying on git's own repack-vs-fetch safety.

**Risk:** `git repack -d` deleting a pack a concurrent fetch just wrote is the
exact race the blanket mutation lock prevents — this option needs careful
validation and is why it's last. Item 1 makes the lock window so small that
this may be unnecessary.

### 5. (Separate, serving-side) Revisit disabled bitmaps

The HIT serve (~4 s) is partly no-bitmap `pack-objects`. A **MIDX bitmap**
(`multi-pack-index write --bitmap`, or `repack --write-midx
--write-bitmap-index`) would accelerate enumeration — but direct upload-pack
**deliberately disables bitmap traversal for correctness** (hidden cache refs,
synthetic served refs, `allowAnySHA1InWant`, shallow/partial). Treat as an
*investigation* with a dedicated correctness pass, not a flag flip. Cross-refs
the same caveat in `cold-read-through-performance.md` option 2.

## How to validate (before/after)

Reproduce the isolated measurement on the **dev** stack (`gitmirrorcache-arm`),
which has no real traffic, so there's no external contention:

1. Reset the repo: `NAME_PREFIX=gitmirrorcache-arm scripts/aws/remove-cache-repo.sh github.com/torvalds/linux`.
2. Bloat it to a realistic size (so maintenance has real work): a proxy-off
   `git fetch --deepen=80` against the dev ALB origin (see
   `scripts/aws/test-prod-linux-cache-depths.sh` and PR #127 for the deepen
   matrix; `--deepen` is relative/accumulating and linux fans out hard past
   ~depth 50).
3. Read the maintenance duration from CloudWatch
   (`/ecs/gitmirrorcache-arm/ec2-api`): the
   `direct git serving maintenance finished ... elapsed_ms=<N>` line.
4. Measure a MISS deepen issued *while maintenance is running* (depth-1 clone +
   `--deepen` over an uncovered boundary) and confirm it no longer stalls.

**Acceptance criteria:**
- Serving maintenance `elapsed_ms` on a bloated `linux` cache drops from
  minutes to **single-digit seconds**.
- A boundary-extending proxy-off deepen issued during maintenance completes in
  its intrinsic time (≈ seconds), not blocked for the repack duration.
- Served clones/deepens (HIT and MISS) remain correct: blobless/shallow markers
  intact, `git fsck --connectivity-only` clean, no stale `shallow.lock`
  (PR #126 regression test still green:
  `cargo test -p git-cache-domain direct_git_deepen_recovers_from_stale_shallow_lock`).

## References

- `crates/git-cache-git/src/lib.rs` — `repack_for_serving`, `commit_graph_write`, `fsck`
- `crates/git-cache-domain/src/materializer/direct_git.rs` — `enqueue_serving_maintenance`, `enqueue_direct_fsck`
- `crates/git-cache-domain/src/materializer/repo.rs` — `lock_repo_mutation`, `clear_stale_repo_locks`
- `docs/cold-read-through-performance.md` — cold-serve analysis + bitmap/git-version caveats
- PR #126 — stale-`shallow.lock` recovery + fsck serialization (introduced the lock discipline this builds on)
- PR #127 — depth+deepen prod matrix used to surface this contention
