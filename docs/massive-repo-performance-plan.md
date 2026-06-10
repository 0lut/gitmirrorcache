# Plan: serving massive repos (linux/llvm-class) fast from the cache

Status: planning document only — no implementation in this PR.

## Problem statement (measured on preview, m8g.2xlarge, gp3 8000 IOPS)

| shape | GitHub direct | cache warm | cache cold read-through |
|---|---|---|---|
| astral-sh/ruff full clone | ~9s | ~9s | ~49s (one batched upstream fetch) |
| linux `--filter=blob:none` | ~132s | hours | hours |
| llvm `--filter=blob:none` | ~196s | hours | hours |
| linux `--depth 1` | seconds | seconds–minutes | cheap (intent-preserving fetch) |

For small/medium repos the cache is already at parity with GitHub. For
multi-million-object repos, *serving* the clone from our cache is the
bottleneck — not hydrating it (the batched read-through fetch of linux
completed in ~80s). Two distinct costs dominate:

1. **`pack-objects` CPU**: GitHub answers a linux blobless clone in ~2 min
   because its repos carry reachability bitmaps and are kept aggressively
   repacked, so `pack-objects` mostly streams existing pack bytes. Our
   freshly-hydrated repo is a pile of unordered packs with no bitmaps, so the
   same request forces a full object walk + delta compression of ~5M objects
   on 8 vCPUs.
2. **Lazy blob fetch storm**: a `--filter=blob:none` clone checkout issues
   thousands of follow-up `git-upload-pack` POSTs (promisor batch blob
   fetches). Each is cheap, but the aggregate round-trip and per-request
   overhead is hours of tail latency.

`proxy-on-miss` (default since #66) already hides the cold case — clients
stream from GitHub at GitHub speed while the warm hydrates in the background.
This plan is about making **warm serving** GitHub-fast too.

## Proposed work, ordered by leverage

### 1. Post-hydration repack with bitmaps (highest leverage)

Run `git repack -adb --write-bitmap-index` (plus `git commit-graph write
--reachable`) after hydration settles, in the same maintenance slot as the
existing post-hydration `fsck`.

- Reachability bitmaps let `pack-objects` answer "objects reachable from
  these wants" via bitmap AND/OR instead of a graph walk, and verbatim-reuse
  pack bytes. This is the single change that closes most of the gap to GitHub.
- Single well-ordered pack also fixes the many-small-packs layout produced by
  incremental read-through fetches.
- Cost: one expensive repack per repo per hydration burst (linux: tens of
  minutes, CPU/IO heavy). Needs: debounce (don't repack on every fetch),
  disk headroom reservation via the existing quota system, and the Git
  semaphore so it can't starve serving.
- Sizing note: bitmaps only help the *serving* repo; they're orthogonal to
  generation bundles/S3.

### 2. `pack-objects` / `upload-pack` server config tuning (cheap, do first)

Set on cached repos (or via `-c` on the serving command):

- `pack.threads=0` (auto → all cores; verify it isn't capped today),
- `pack.useBitmaps=true` (default, but only useful after item 1),
- `uploadpack.allowFilter=true` (already set), `uploadpack.packObjectsHook`
  left unset (see item 4),
- `core.deltaBaseCacheLimit` / `pack.deltaCacheSize` bumps for large repos,
- `pack.writeReverseIndex=true` (needed for verbatim pack reuse),
- `fetch.unpackLimit=1` on hydration fetches so incoming objects stay packed
  (keeps repack input sane).

Low risk; measurable with the same preview matrix.

### 3. Tee-the-proxy: persist proxied pack bytes instead of re-fetching

Today a proxied cold miss costs GitHub bandwidth twice (client stream + warm
re-fetch). The stream could be teed through `git index-pack --fix-thin` into
the cache repo's objects, then refs fixed up from the negotiated wants.
Halves upstream bandwidth and makes the warm nearly free.

Caveats to design through: aborted/partial streams, shallow boundaries,
filtered (promisor) packs needing `.promisor` marking, and ref fixup must
respect the existing generation/manifest contract (warm path must keep
`without_manifest_hydration`). Medium complexity; independent of items 1–2.

### 4. Pack caching for common clone shapes (CDN-style, later)

For idempotent shapes (full clone, blobless clone, depth-1 of a tip), the
exact pack bytes can be cached keyed by `(repo, generation/tip, shape)` and
replayed via `uploadpack.packObjectsHook`. This is what makes repeated
CI-fleet clones of the same monorepo O(disk-stream). Only worth it after
items 1–2; invalidation keys off the generation tip so it composes with the
existing manifest model.

### 5. Blobless checkout storm mitigation

The thousands of lazy blob POSTs per blobless clone are inherent to the
client, but the server can: keep bitmaps (item 1) so each batch is cheap;
ensure keep-alive/h2 connection reuse through the ALB; and consider
`uploadpack.allowAnySHA1InWant` audit (already required for promisor
fetches). Mostly mitigated by 1 + 2.

## Sequencing / measurement

1. Item 2 (config) → re-run linux/llvm warm matrix on a preview.
2. Item 1 (repack+bitmaps in maintenance) → target: linux blobless warm clone
   in single-digit minutes on m8g.2xlarge.
3. Items 3–4 as follow-ups once warm serving is fast; item 3 also fixes
   double upstream bandwidth.

Each step is independently verifiable with the existing preview perf matrix
(`scripts/aws/deploy-preview.sh` + timed clones with
`git-cache-use-proxy-on-miss: 0`).
