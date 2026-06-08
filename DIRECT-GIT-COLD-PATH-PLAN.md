# Direct Git Cold Path Plan

This branch starts from current `main` after the auth/direct Git PR merge. The
goal is to plan the next performance pass around the two changes we kept coming
back to during the LLVM investigation:

1. Preserve the client's shallow/blobless request shape when the cache has to
   read through to upstream.
3. Add a true cold-fast path so a first clone of a large repo can complete at
   GitHub-like speed while cache warming continues out of band.

The numbering intentionally follows the earlier discussion; this plan does not
try to cover the skipped item 2.

The auth model from the merged PR stays the same: the API layer proves repo
access with the selected upstream auth, then downstream direct Git code treats
auth as an already-established repo-level fact.

## Why This Exists

Hot direct clones are already fast once the cache has the wanted commit. The
problem is the first direct clone after the advertised upstream tip advances or
after local cache data has been removed. In that case, the cache currently blocks
the client while it imports enough upstream objects to serve `git upload-pack`.

For LLVM this can be much slower than cloning GitHub directly because the cache
does not yet fully mirror the client's request shape. A client command such as:

```text
git clone --depth=1 --filter=blob:none --no-checkout <url> <target>
```

only needs the advertised tip, a shallow boundary, and a blobless pack. If the
cache asks upstream for a raw object without the same depth semantics, Git may
send far more history than the client would have received from GitHub.

## Track 1: Preserve Shallow/Blobless Read-Through

### Intended Shape

Introduce a small direct Git request parser that produces one structured value:

```rust
struct UploadPackIntent {
    wants: Vec<CommitSha>,
    filter: Option<UploadPackFilter>,
    depth: Option<u32>,
    deepen_since: Option<u64>,
    deepen_not: Vec<String>,
    shallow: Vec<CommitSha>,
}

enum UploadPackFilter {
    BlobNone,
}
```

Start with the cases we need for the benchmark:

- `want <sha>`
- `filter blob:none`
- `deepen 1`
- existing flush/delim behavior in pkt-line requests

The existing `parse_want_lines()` and
`upload_pack_requests_blobless_filter()` helpers should become wrappers around
this parser or disappear. We want one parse pass and one request-shape object.

### Fetch Policy

When direct Git POST has an upstream ref comparison from the preceding GET:

- map wanted SHA values back to advertised refs when possible;
- prefer fetching the advertised ref, not the raw SHA;
- carry the client's depth and filter into the fetch;
- expose the fetched commit through hidden cache refs after local readiness
  checks pass.

For the LLVM benchmark, the first target command should produce an upstream
fetch shaped like:

```text
git fetch --no-tags --depth=1 --filter=blob:none -- \
  <upstream-url> +refs/heads/main:refs/cache/upstream/heads/main
```

If a want does not map to an advertised ref, keep the raw-SHA fallback, but still
carry safe client semantics where Git accepts them:

```text
git fetch --no-tags --depth=1 --filter=blob:none -- <upstream-url> <sha>
```

Raw-SHA fetch should be treated as a compatibility fallback, not the happy path
for normal branch clones.

### Avoid Implicit Lazy Fetch Surprises

The current repo is a partial/promisor cache, so commands such as `cat-file` can
cause Git to fetch missing promised objects implicitly. That makes latency hard
to reason about and can hide where upstream work is happening.

During direct read-through:

- do explicit upstream fetches first;
- use non-lazy existence/readiness checks where Git supports it;
- if we add `GIT_NO_LAZY_FETCH=1`, scope it to validation commands that should
  be pure local checks;
- log explicit fetch elapsed time separately from local validation elapsed time.

### Tests

Add tests before implementation changes:

- parser test for `deepen 1` plus `filter blob:none`;
- parser test that unsupported filters do not become `BlobNone`;
- direct Git read-through test that a wanted advertised ref uses ref fetch, not
  raw-SHA fetch;
- git wrapper test that `--depth=1` and `--filter=blob:none` are passed only
  after validation;
- regression test that direct clone still works without pre-materialize;
- regression test that locally ready wants do not contact upstream.

### Done Criteria

- Cold LLVM direct clone without pre-materialize no longer imports much more
  history than GitHub for `--depth=1 --filter=blob:none --no-checkout`.
- Hot direct clone remains around the current hot-cache profile.
- Auth-free public direct Git still works.
- Token-present direct Git still uses the request-scoped auth for upstream
  proof and fetch.
- There is still one direct Git POST path after repo access, not separate
  anonymous/authenticated implementations.

## Track 3: True Cold-Fast Mode

Track 1 may be enough for normal branch-tip shallow clones. If first-clone
latency is still meaningfully slower than GitHub, the cache needs a true
cold-fast mode.

### Recommended Shape

For cold misses, proxy the upstream Smart HTTP request to the origin first, then
warm the cache asynchronously.

The request flow becomes:

```text
direct Git GET
  -> prove repo access
  -> advertise upstream refs
  -> store short-lived proof handoff

direct Git POST
  -> validate repo and auth proof
  -> parse UploadPackIntent
  -> if local cache can serve cheaply: serve local upload-pack
  -> otherwise: stream upstream upload-pack response to client
  -> enqueue cache import/warm task using the same repo/auth/intent
```

This preserves the most important user-visible behavior: a first clone should
not be slower than asking GitHub directly merely because the cache is empty.

### Import Strategy Options

Option A: proxy then refetch in the background.

- simplest and safest;
- duplicates upstream work on cold miss;
- avoids pack parsing in the HTTP hot path;
- good first implementation if Track 1 still leaves gaps.

Option B: tee the upstream pack stream to disk while streaming it to the client.

- avoids duplicate upstream transfer;
- needs careful byte limits, process cleanup, and pack validation;
- should only happen after Option A proves the model.

Option C: redirect public cold clones to upstream.

- fastest and simplest for public GitHub;
- changes client-visible remote behavior;
- not suitable for private repos or deployments that require the cache host to
  remain the only Git remote;
- keep as a later, explicit operator option if ever needed.

Start with Option A if true cold-fast mode is needed.

### Security And Auth Rules

- The repo access gate still runs before proxying.
- Request-scoped upstream auth may be forwarded only to the upstream origin.
- Upstream auth must never be logged, persisted, included in argv, or written to
  cache metadata.
- Cache import must use the same auth context as the request that proved access.
- If cache warming fails after the client clone succeeds, log it as an async
  cache failure; do not retroactively fail the client.

### Operational Controls

Add configuration before enabling broadly:

```text
git_remote.cold_miss_mode = "local-read-through" | "upstream-proxy"
git_remote.cold_miss_proxy_repos = optional allowlist/pattern list
git_remote.background_import_concurrency = bounded integer
```

Default should remain conservative until benchmarks show the cold proxy path is
stable.

### Tests

- cold miss proxies a clone successfully when local repo has no wanted commit;
- proxy path forwards Git protocol v2 headers and request body correctly;
- proxy path preserves Basic upstream auth only for the upstream request;
- client disconnect cancels or bounds upstream/proxy resources;
- cache import runs asynchronously and is deduped per repo/ref/commit;
- failed async import does not corrupt existing cache state;
- hot cache still serves locally and does not proxy.

### Done Criteria

- Cold LLVM benchmark is close to direct GitHub for:

```text
git clone --depth=1 --filter=blob:none --no-checkout <url> <target>
```

- The second run for the same upstream tip is served locally and remains faster
  than direct GitHub.
- Background import/fsck logs show bounded work and no auth/token leakage.
- Main and feature branch benchmark results are recorded side by side before
  implementation is merged.

## Benchmark Plan

Use the existing side-by-side script in `~/dev` and keep GitHub as a baseline:

```text
python ~/dev/compare_git_clone_perf.py --runs 3
```

For cold comparisons, remove matching LLVM generation/manifests and local repo
state from the compared environments so both start from the same advertised
upstream commit. Record:

- upstream HEAD SHA;
- local cache state removed before the run;
- first-run latency;
- second-run latency;
- whether server logs show ref fetch, raw-SHA fetch, proxy, or local serve;
- whether any GitHub rate-limit or auth errors appear.

## Open Decisions

- Should Track 1 initially support only `deepen 1`, or arbitrary `deepen <n>`?
- Should `deepen-since` and `deepen-not` be parsed but rejected from
  read-through until explicitly implemented?
- Should raw-SHA read-through with depth be allowed for all wants, or only after
  repo access proof plus a failed ref mapping?
- Should `GIT_NO_LAZY_FETCH=1` be added as a git wrapper option or a separate
  validation method family?
- Should true cold-fast proxy mode be opt-in for all repos at first, or enabled
  only for known large public repos during preview testing?

## Suggested Implementation Order

1. Add `UploadPackIntent` parser and tests.
2. Add git wrapper support for depth/filter ref fetch with argument validation.
3. Use `UpstreamRefComparison` to fetch advertised refs for direct Git wants.
4. Add timing logs around explicit fetch, local readiness checks, and
   upload-pack spawn.
5. Deploy preview and compare cold/hot LLVM against direct GitHub and main.
6. If Track 1 is still not close enough, implement cold proxy mode behind a
   config flag.
7. Only after proxy mode is correct, consider tee-to-disk import optimization.
