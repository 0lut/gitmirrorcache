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
+ hourly compaction. I'd instead store **packs content-addressed by the set of tips
they close over** (e.g. `pack-{hash(tips)}.pack` + reachability bitmap), with a tiny
manifest mapping refs→tips→packs. Hydration = download N packs into `objects/pack/`,
write refs, done — no chain replay, and "compaction" is just `git repack`/`gix pack`
producing a new base pack and garbage-collecting unreferenced ones. Simpler invariant:
S3 holds packs; manifests are pure metadata; nothing on disk is authoritative.

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
