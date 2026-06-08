# Fix Plan: Slow LLVM Direct Clone

## Summary

The LLVM regression came from direct Git POST doing hidden cache-building work.
After a ref advertisement, upload-pack could fetch refs/objects, hydrate or
publish generation state, and scan large ref sets before serving a want. That
made a hot clone on this branch far slower than current `main`.

The fix is to split direct Git into two responsibilities:

- prove repo access and advertise refs;
- serve only objects already ready in the local repo.

Direct Git is no longer a hidden materializer.
If the local bare repo is absent, direct Git returns a cache miss instead of
initializing an empty repo or entering upload-pack negotiation.

## Implemented Behavior

### Direct Git GET

`GET /git/.../info/refs?service=git-upload-pack`:

- validates the repo host/path;
- parses optional upstream Basic auth;
- requires the local bare repo to exist;
- fetches upstream refs with the selected auth as the repo-access proof;
- synthesizes the ref advertisement from locally ready refs whose branch names
  still exist upstream, without fetching objects;
- stores a short-lived GET-to-POST proof keyed by repo and exact auth
  fingerprint.

### Direct Git POST

`POST /git/.../git-upload-pack`:

- parses optional upstream Basic auth;
- requires the local bare repo to exist;
- uses the matching GET proof if available;
- otherwise reruns the same lightweight upstream ref fetch;
- parses wants;
- requires wanted objects to exist locally;
- requires commit wants to have their tree object ready;
- spawns `git upload-pack`.

It does not fetch packs, run `fetch_all_heads`, hydrate generation bundles,
publish generations, or use manifests as upload-pack work items.

If an authorized object is not locally ready, direct Git returns a fast cache
miss:

```text
authorized object `<sha>` is not available in the local cache
```

If upstream has advanced beyond the local cache, direct Git advertises the last
locally ready branch tip instead of advertising the unavailable upstream tip.
`/v1/materialize` or a warmer is responsible for moving the cache forward.

## Auth Rule

No token means anonymous upstream Git access. Token present means credentialed
upstream Git access. This PR does not downshift token-present public repos to
anonymous mode and does not use GitHub REST to classify visibility.

Repo access is the boundary. Once a request proves it can read the repo with the
selected auth, direct Git treats object presence as availability, not a second
authorization phase. This is an explicit tradeoff for the shared repo model; use
separate upstream repositories for histories that require stronger isolation.

## Materialize Is The Build Path

`/v1/materialize` remains responsible for expensive cache work:

- resolve the current upstream ref;
- hydrate from verified manifests when appropriate;
- fetch upstream when cache state is missing;
- publish generations;
- create the session response.

Large rebuilds should be observable and rate-limited through materialize or
background warmers, not triggered by a client clone POST.

## Generation Verification Must Not Rebuild The World

LLVM also showed that publishing a new incremental generation can hurt later hot
requests if verification replays the entire parent chain. The
publisher/verifier now tries a local-repo fast path first:

- require the local repo to have the pending generation's tip objects;
- require every parent generation to already have verified metadata;
- verify the just-created bundle file during foreground publication, or download
  only the pending bundle when recovering an already pending generation;
- check bundle length and SHA-256;
- run `git bundle verify` against the local repo.

If those conditions are not met, automatic background verification leaves the
generation pending. Explicit recovery/compaction verification still has the
original full-chain temporary repo fallback. This keeps cold recovery behavior
while avoiding multi-gigabyte `index-pack` work for the common HTTP materialize
case.

## Performance Checks

Expected profile:

- hot LLVM direct clone after materialize should stay near current `main`;
- direct Git POST should not perform upstream pack fetch/index-pack work;
- cold direct clone with missing local objects should fail quickly;
- explicit materialize may still take the cost of real cache rebuilding.

Smoke-test checklist after deploy:

- public LLVM `/v1/materialize` hot and cold;
- public LLVM direct clone hot and ref-cold/object-cold;
- private `0lut/outerflow` materialize and direct clone with GH CLI creds;
- logs show no auth/session tokens;
- logs show direct POST readiness checks but no fetch/hydrate/publish work.
