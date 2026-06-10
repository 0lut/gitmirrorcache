# Clean-Slate Design: Read-Through Git Cache

Thought experiment: if `gitmirrorcache` didn't exist, how would I build a read-through
Git cache for CI-scale fetch/clone traffic today? Short answer: I'd converge on much of
the same shape (Smart HTTP front, bare repos as hot cache, object storage as durable
truth), but I'd make a few different foundational bets.

## 1. Requirements that force the architecture

- Clients are stock `git` — so the wire protocol (Smart HTTP, protocol v2) is the API.
  Any design must terminate `info/refs` + `git-upload-pack` correctly, including
  shallow (`depth`) and partial (`filter=blob:none`) negotiation.
- Upload-pack negotiation is stateful and have/want-dependent — responses are not
  cacheable as opaque HTTP bodies. You cannot build this as a dumb HTTP cache (Varnish
  in front of GitHub does not work). This single fact forces "cache = a real Git
  repository the server can run upload-pack against."
- Freshness: refs move constantly; objects are immutable. So the design splits into a
  **mutable, cheap-to-revalidate ref layer** and an **immutable, infinitely cacheable
  object layer**. Everything good in the design comes from exploiting that split.

## 2. The architecture I'd build

```
git client ──HTTP──> stateless edge (protocol termination, auth, validation)
                        │
                        ├── ref layer: ls-remote w/ TTL+SWR; serve advertisement
                        │
                        ├── object layer: bare repo on local NVMe/EBS (hot)
                        │      └── upload-pack runs against this
                        │
                        ├── durable layer: S3 — packs/bundles + ref snapshots
                        │      └── hydrate hot cache on node start / miss
                        │
                        └── cold miss: stream-proxy upstream upload-pack to the
                            client immediately; tee/queue a background warm
```

Same macro shape as gitmirrorcache. The interesting choices are below.

## 3. Where I'd bet differently

### a. In-process Git via gitoxide instead of shelling out to `git`
The current design wraps the `git` binary (subprocess semaphores, bounded readers,
kill_on_drop, argv-injection validation — a large fraction of AGENTS.md is defending
this boundary). With `gix` (gitoxide) maturing, I'd implement upload-pack serving
in-process:
- no argv surface → the whole flag-injection/NUL class of bugs disappears
- pack generation can stream straight from mmap'd packfiles to the socket
- per-request memory/CPU bounds are first-class instead of subprocess babysitting
Risk: gitoxide's server-side negotiation is the least-proven part; I'd keep a
`git upload-pack` subprocess fallback behind a flag during burn-in.

### b. Protocol-v2 ref advertisement as a first-class cached artifact
v2 lets the server answer `ls-refs` separately from fetch. I'd cache the ref
advertisement per repo with a short TTL + stale-while-revalidate, and support
*ref-prefix-scoped* revalidation (CI mostly wants one branch). That makes the common
"fetch main" path one upstream round-trip at most — often zero.

### c. bundle-uri / packfile-URI offload to S3+CDN
Modern git supports `bundle-uri`: the server tells the client "first download this
bundle from a URL, then do an incremental fetch from me." I'd publish base bundles to
S3 (optionally behind CloudFront) and advertise them. Effect: the heavy bytes of a
cold clone are served by S3/CDN, not by the cache node's CPU/NIC; the node only
serves the small top-up pack. This collapses the "cold miss proxy vs. warm cache"
latency gap that the current design handles with proxy-on-miss.
(Caveat: requires client git ≥2.38ish and `bundle.heuristic` support; keep the plain
path for old clients.)

### d. Content-addressed pack storage instead of generation chains

gitmirrorcache models durability as generation manifests + incremental bundle chains
+ hourly compaction. I'd instead store **content-addressed packs** with a tiny ref
manifest. The chain shape is an artifact of building on `git bundle`; flat packs are
what the data actually wants to be. Detailed design for future reference:

#### Storage layout (S3)

```
repos/{host}/{owner}/{repo}/
  packs/pack-{sha256-of-pack-bytes}.pack       # immutable, content-addressed
  packs/pack-{sha256}.idx                      # pack index (optional: derive on node)
  packs/pack-{sha256}.bitmap                   # reachability bitmap (base packs only)
  snapshots/{uuidv7}.json                      # snapshot manifest (immutable)
  HEAD.json                                    # tiny mutable pointer (CAS-updated)
```

A **snapshot manifest** is the only metadata object:

```json
{
  "schema_version": 1,
  "repo": "github.com/torvalds/linux",
  "snapshot": "01890b2e-…",            // uuidv7, time-ordered
  "refs": { "refs/heads/master": "1a2b…", "refs/tags/v6.9": "9f8e…" },
  "packs": [
    { "key": "packs/pack-aaaa….pack", "len": 3221225472, "kind": "base" },
    { "key": "packs/pack-bbbb….pack", "len": 8388608,    "kind": "delta" }
  ],
  "created_at": "…", "verified_at": "…", "git_version": "…", "fsck_mode": "…"
}
```

`HEAD.json` points at the latest verified snapshot id (compare-and-swap via
`put_if_absent`-style conditional write, or versioned key). Snapshots are immutable;
publishing never mutates an existing object.

#### Publish flow (replaces bundle_create + generation manifest)

1. After a fetch brings new objects into the bare repo, take the pack git already
   wrote during the fetch (`objects/pack/pack-*.pack`, newest), or run
   `git pack-objects --revs` over `new_tips --not old_tips` — no `git bundle create`
   cost, no prerequisites bookkeeping.
2. sha256 the pack bytes → key `packs/pack-{sha256}.pack`; `put_if_absent` (dedupe is
   free: identical packs across snapshots/nodes share one object; re-publish of the
   same content is a no-op).
3. Write a new snapshot manifest = previous manifest's pack list + new pack, with the
   current full ref set (refs are tiny; always store them complete — this removes the
   entire ref-manifest/commit-manifest split).
4. CAS `HEAD.json` from previous snapshot to new one. On CAS failure, reload and retry
   (single-writer-per-repo locking makes this rare).
5. Verification mirrors today's flow: download packs, `git index-pack --fsck-objects`
   or full `fsck`, then mark `verified_at`. The pack checksum **is** its name, so
   integrity = "sha256 of downloaded bytes equals key" — no separate
   `bundle_sha256` field, no len mismatch class of bugs.

#### Hydrate flow (replaces chain replay)

1. Read `HEAD.json` → snapshot manifest (2 small GETs).
2. Download all listed packs **in parallel** straight into `objects/pack/`
   (vs. today's strictly sequential chain: download bundle N, unbundle, download N-1…).
   Disk reservation = sum of `len` fields, known up front.
3. `git index-pack` each pack (parallel), or fetch the stored `.idx` alongside.
4. Write refs from the manifest (`git update-ref --stdin` batch), set HEAD. Done —
   no unbundle, no prerequisite ordering, no partial-chain states.

#### Compaction becomes optional, not correctness-critical

The chain design *requires* compaction (unbounded chain depth = unbounded hydrate
cost). With flat packs, a snapshot with 50 small delta packs still hydrates in one
parallel download wave. "Compaction" degrades to an optional background
`git repack -a -d --write-bitmap-index` (which #74 already runs for serving!) whose
output pack is published as a new single-pack snapshot. Garbage collection = delete
packs not referenced by any snapshot newer than a retention horizon, plus snapshots
older than the horizon — safe because snapshots are immutable and HEAD only moves
forward.

#### What gets deleted from the current codebase

- `bundle_create_all` / `bundle_create_incremental` + tips/prerequisite tracking
- `parent_generation` chains, chain-walk in `hydrate_generation`, chain-depth
  thresholds, inline + hourly compaction scheduling
- The delta-bundle-failed→full-bundle fallback path
- Per-ref `RefManifest` / `CommitManifest` objects (subsumed by the snapshot's
  complete ref map; "is commit C complete in cache" = "C reachable from any
  snapshot ref", answerable locally after hydrate)

#### Interaction with #74 and bundle-uri (c)

- #74's post-hydration `repack --write-bitmap-index` output is exactly the artifact
  to publish as a compacted base pack — serving maintenance and storage compaction
  become the same job.
- The bundle-uri base bundle is the base pack + its ref list wrapped in bundle
  framing; produce one artifact, serve it both ways (S3/CDN for bundle-uri,
  same bytes for hydration).
- Net effect post-#74: this is **not** needed for serving performance (bitmaps fix
  that); its remaining value is ops simplification (no compaction machinery),
  parallel cold hydration, and cross-snapshot dedupe. It is an S3-format fork —
  migration needs dual-read (read chains, write snapshots) for one retention cycle,
  or a one-shot backfill job that hydrates each repo from chains and republishes as
  a snapshot.

Simpler invariant overall: S3 holds immutable packs; manifests are pure metadata;
`HEAD.json` is the only mutable object; nothing on disk is authoritative.

### e. Same calls I'd keep (they're right)
- Bare repo per upstream repo on local disk as the only thing upload-pack touches.
- Object storage as source of truth; disk is disposable LRU with reservations+locks.
- Proxy-on-miss streaming for cold direct-Git traffic (it's the correct latency hack
  until bundle-uri coverage is universal).
- Request-scoped upstream auth, never in argv/logs/manifests; receive-pack hard-rejected.
- Single-writer-per-repo locking + in-flight dedupe (thundering CI herds are the
  workload).

## 4. What I'd deliberately not build (and why)

- **Dumb HTTP/CDN caching of upload-pack responses** — negotiation makes responses
  per-client; doesn't work.
- **A custom object database** — git's packfile format + bitmaps are extremely good;
  re-deriving them is years of work for negative value.
- **Mirror-everything cron (classic `git clone --mirror` farm)** — pull-through with
  background warming scales to "any repo anyone asks for" without an allowlist, and
  the materialize API (explicit warm) covers the prefetch case.
- **Multi-node shared hot cache (EFS/NFS)** — git on network filesystems is misery;
  shard repos→nodes by consistent hashing at the LB instead, each node owning its
  EBS/NVMe.

## 5. Honest assessment

Given the constraints (stock git clients, GitHub-scale repos, CI fetch storms,
AWS), most clean-slate designs converge on what this repo already is. The genuinely
different bets are (a) gitoxide in-process serving, (c) bundle-uri offload, and
(d) content-addressed packs replacing generation chains — each trades implementation
maturity for a simpler invariant or cheaper bytes. If I were starting today I'd
prototype (c) first: it's incremental, client-driven, and removes the most load for
the least risk.
