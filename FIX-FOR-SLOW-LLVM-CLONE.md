# Fix Plan: Slow LLVM Direct Clone

## Summary

The LLVM regression came from auth-aware direct Git doing extra work that current
`main` did not do. The first fix attempt made direct Git advertise only locally
ready refs and reject cache misses. That recovered hot latency, but it removed
an important current-main behavior: `git clone` should work without a prior
`/v1/materialize`.

The current approach preserves main-like read-through and adds auth only as a
repo visibility gate.

## Implemented Behavior

### Direct Git GET

`GET /git/.../info/refs?service=git-upload-pack`:

- validates the repo host/path;
- parses optional upstream Basic auth;
- fetches upstream refs with the selected auth as the repo-access proof;
- synthesizes the Smart HTTP ref advertisement from those upstream refs;
- stores a short-lived GET-to-POST proof keyed by repo and exact auth
  fingerprint.

GET does not fetch objects. It only proves that the selected credentials can see
the repository and gives the Git client the same ref view it would get from
upstream.

### Direct Git POST

`POST /git/.../git-upload-pack`:

- parses optional upstream Basic auth;
- uses the matching GET proof if available;
- otherwise reruns the same lightweight upstream ref fetch;
- parses wants;
- serves locally ready wanted commits immediately;
- hydrates complete commit manifests when available;
- fetches missing wanted commits from upstream using the same request auth;
- requires fetched or hydrated commits to be ready for serving before exposing
  them;
- queues background `git fsck --connectivity-only` for newly imported commits;
- spawns `git upload-pack`.

This keeps one path for anonymous and credentialed requests. Object checks are
availability checks after repo access, not separate authorization checks.
For blobless clients, direct Git forwards `--filter=blob:none` to the upstream
read-through fetch so a blobless clone does not accidentally hydrate blobs into
the cache.

## Auth Rule

No token means anonymous upstream Git access. Token present means credentialed
upstream Git access. This PR does not downshift token-present public repos to
anonymous mode and does not use GitHub REST to classify visibility.

Repo access is the boundary. Once a request proves it can read the repo with the
selected auth, direct Git treats repo history as accessible for this PR. A
future stricter mode can add current-reachability proof, but this branch keeps
current `main` behavior and documents the tradeoff in code comments.

## Performance Checks

Expected profile:

- hot LLVM direct clone after materialize should stay near current `main`;
- cold LLVM direct clone should work without pre-materialization;
- direct Git should add at most the repo-access ref proof over current `main`;
- direct Git should not block the client on `git bundle create --all`;
- direct Git should trigger background fsck, not background generation
  bundling;
- generation verification should avoid re-indexing the full LLVM history when a
  local-repo fast path can verify the newly created bundle.

Smoke-test checklist after preview deploy:

- public LLVM `/v1/materialize` hot and cold;
- public LLVM direct clone hot and cold;
- private `0lut/outerflow` materialize and direct clone with GH CLI creds;
- logs show no auth/session tokens;
- logs show direct GET proving repo access and direct POST read-through using
  the same auth.
