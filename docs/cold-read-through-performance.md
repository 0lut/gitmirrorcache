# Cold Read-Through Performance

Benchmarks against the AWS dev stack (see PR #82, `git_matrix` findings)
showed that cold full-history clones through the cache's read-through path
are much slower than cloning GitHub directly:

| repo  | direct GitHub | AWS cold read-through (proxy off) |
|-------|---------------|-----------------------------------|
| uv    | 6.6s          | 2.6s                              |
| ruff  | 4.4s          | 4.6s                              |
| linux | 157.7s        | 233.9s                            |
| llvm  | 74.5s         | 241.3s                            |

## Why cold read-through is slow

The proxy-off read-through path is strictly **fetch-then-serve**:

1. The cache fetches the wants from upstream into the shared bare repo
   (≈ the direct-GitHub cost).
2. It then spawns local `git upload-pack`, which regenerates the entire
   client pack from scratch.

Step 2 is expensive on the very first serve because the freshly fetched repo
has **no bitmaps and no commit-graph** — serving maintenance (repack with
bitmaps + commit-graph) only runs ~60s later in the background, so it never
helps the cold request itself. For llvm-sized repos, `pack-objects` walks
millions of objects with no reuse machinery, roughly doubling the request.

The default proxy-on-miss lane hid the client-visible latency (the client
streams upstream's bytes directly) but the background warm then performed a
**second** full upstream download to populate the cache.

## Option 1 (implemented): tee-import the proxied response

For a cold full-closure request (wants only — no `have` lines, no
shallow/deepen lines), the upstream upload-pack response pack is exactly the
pack the cache itself would fetch. So while proxying the response to the
client, the cache now:

1. Plans eligibility from the request body (`plan_upload_pack_tee`).
2. Demuxes the proxied v0 response (`PackDemux`): with side-band negotiated,
   band 1 carries the pack; without side-band, everything after the `NAK`
   pkt is raw pack bytes. Band 3 or `ERR` pkt fails the demux.
3. Spools the pack bytes to disk through a bounded channel and chunked disk
   reservations, hashing incrementally.
4. On clean stream completion, imports the spooled pack with
   `git index-pack` into the shared bare repo
   (`Materializer::import_proxied_upload_pack`), exposes the served wants,
   and queues the usual fsck + serving maintenance.

This makes a cold miss populate the cache with **one** upstream download and
no second pack generation. Safety properties:

- **Fallback on any failure.** Demux errors, spool backpressure, disk
  reservation failures, client disconnects, truncated upstream streams, and
  import errors all fall back to the existing background warm refetch. The
  pack trailer hash verified by `git index-pack` is the final integrity
  check on the spooled bytes.
- **Incremental responses are never imported.** Requests with `have` lines
  produce thin/incremental packs that are not self-contained; they are
  ineligible at planning time.
- **Blobless imports stay partial-safe.** A `filter blob:none` import writes
  the pack-level `.promisor` marker and the repo-level partial-hydration
  marker, so later full-object requests still force an unfiltered refetch.
  Conversely, a full-object tee import into a repo already marked partially
  hydrated is declined (the warm refetch path owns the authoritative
  `--refetch` that clears the marker).

Config: `git_remote.proxy_tee_import` (default `true`), env
`GIT_CACHE_GIT_REMOTE_PROXY_TEE_IMPORT`. It only applies when the
proxy-on-miss lane engages; the explicit proxy-off lane (per-request
`git-cache-use-proxy-on-miss: false`) still uses fetch-then-serve and stays
slow on cold misses by design.

## Option 2 (documented, not implemented): verbatim pack reuse + synchronous commit-graph

A complementary serve-side improvement:

- Add `pack.allowPackReuse=multi` to the served-repo config so `pack-objects`
  can copy verbatim byte ranges from multiple bitmapped packs instead of
  re-delta-ing them.
- Write the commit-graph synchronously right after a read-through fetch
  (much cheaper than the full repack) so the cold serve's object walk is
  faster.

Caveats, which is why this is not implemented yet:

- **Git version pinning is required.** `pack.allowPackReuse=multi` shipped in
  git 2.43 and had correctness fixes (wrong delta bases in reused chunks)
  through ~2.46. The deploy image installs git from the git-core PPA without
  pinning; before enabling `multi`, the image must pin/assert **git ≥ 2.46**.
- **Limited cold-miss benefit.** Verbatim reuse only applies to *bitmapped*
  packs, and a freshly fetched pack has no bitmap until background serving
  maintenance completes. It mainly helps serves between first fetch and
  maintenance, plus all later serves — expect ~20-40% off the serve half,
  not the 2x needed to match direct GitHub.

With option 1 in place, the cold-miss path no longer regenerates packs at
all, which further reduces the urgency of option 2.
