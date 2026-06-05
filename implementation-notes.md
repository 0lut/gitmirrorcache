# Implementation Notes: Read-Through Git Remote

## Overview
Implementing the spec from `git-api-plan.md` (PR #13): a read-through Smart HTTP
remote that allows `git clone`/`git fetch` directly against the cache server
without needing a prior `/v1/materialize` call.

---

## Decisions not in the spec

### 1. Reusing the existing bare cache repo as the served repo
The spec says "Start with the existing bare cache repo as the served repo" and
only introduce a separate served bare repo if ref isolation becomes necessary.
We follow this guidance: the direct Git remote serves from the same
`cache_root/repos/{host}/{owner}/{repo}.git` bare repo that the materializer
already manages. Public `refs/heads/*` and `HEAD` are written into the same
repo. Internal `refs/cache/*` are hidden via `git config` on the repo.

### 2. Streaming upload-pack via Axum's `StreamBody`
The spec requires streaming pack output. The existing `upload_pack_stateless_rpc`
buffers stdout into a `Vec<u8>`. For the direct remote, we add a new
`upload_pack_stream` method on the `Git` wrapper that returns an async reader
(the child process stdout) instead of buffering. The API layer wraps this in
an Axum `Body::from_stream()`.  We keep the old buffered method for the session
endpoint for backward compatibility.

### 3. Buffered ref advertisement
`git upload-pack --advertise-refs` output is typically small (a few KB). We
reuse the existing buffered `advertise_refs()` helper and prepend the
`# service=git-upload-pack` header in-memory (a fixed 34-byte prefix).
We initially tried streaming but the child process lifetime issue (see T2)
made buffering the simpler and more correct approach for this small payload.

### 4. Config structure for `git_remote`
The spec suggests a `[git_remote]` TOML section. We add a `GitRemoteConfig`
struct with `enabled: bool` (default `false`). Branch ref check policy and
commit read-through behavior follow the spec defaults. The config is optional;
when absent, the direct remote is disabled and the router doesn't register
the `/git/{*repo_path}` route.

### 5. Upstream ref comparison uses `git ls-remote --heads --symref`
For comparing upstream refs before advertising, we use a single
`git ls-remote --heads --symref <upstream>` call. This gets all branch heads
and the symbolic HEAD in one round-trip. We parse both symrefs and branch SHAs
from the output.

### 6. Public ref sync strategy
After verifying upstream refs, we update the bare cache repo's public refs:
- `refs/heads/<branch>` → verified commit SHA
- `HEAD` → symref to default branch
We use `git update-ref` for branch heads and `git symbolic-ref HEAD` for the
default branch. This is done inside the lease to prevent races.

### 7. Concurrency: direct domain-layer synchronization
The spec says concurrent requests should wait for the same upstream work. For the
direct Git remote, we bypass the `UpdateCoordinator` and call the domain's
`ensure_repo_advertisable()` directly. This method does upstream ref comparison
and targeted fetch internally, which is simpler than routing through the
coordinator for the branch-comparison-then-fetch flow. The coordinator remains
in use for the existing `/v1/materialize` endpoint.

### 8. Want-line parsing
For `POST /git-upload-pack`, we parse `want <oid>` lines from the pkt-line
formatted request body. We check each wanted OID against local objects and
object-store manifests, hydrating as needed before invoking upload-pack.

### 9. `uploadpack.allowAnySHA1InWant=true`
We configure this on all served repos so that `git fetch origin <sha>` works.
The spec notes this should only be on "validated, allowlisted repos" — our
repos are already validated via `allowed_upstream_hosts`, so this is safe.

### 10. Integration tests against real public repos
The spec mentions tests with `git clone --branch main`, `git fetch <sha>`, etc.
We add integration tests that spin up the Axum server and run real git commands.
For high-commit repos, we looked for public repos with 200k+ commits.

Candidates:
- `astral-sh/uv` — already in the test suite
- `torvalds/linux` — ~1.2M+ commits, the canonical huge repo
- `chromium/chromium` — extremely large, but impractical for CI (very slow)
- `gcc-mirror/gcc` — ~300k+ commits
- `llvm/llvm-project` — ~500k+ commits

We use `torvalds/linux` and `llvm/llvm-project` as the high-commit test targets.
For CI practicality, we do shallow operations (ls-remote, single-branch fetch)
rather than full clones.

---

## Tradeoffs

### T1. No separate served repo
Using the same bare repo for internal cache refs and public refs is simpler but
means we need `hideRefs` config to prevent leaking `refs/cache/*`. If future
features need stronger isolation, we'll need to refactor to use alternates.

### T2. Buffered advertise-refs, streaming upload-pack
Initially we tried streaming both `advertise-refs` and `upload-pack` output.
However, child process lifetime management caused issues: the `UploadPackProcess`
struct holds the child with `kill_on_drop(true)`, and when it drops at the end of
the handler scope, the child is killed before the response stream is consumed by
the client. For `advertise-refs` (output is small — just ref lines) we switched
to the existing buffered `advertise_refs()` helper. For `upload-pack` (output can
be arbitrarily large pack data), we use a `ChildGuardStream` wrapper that holds
the child process handle alongside the `ReaderStream`, keeping the process alive
for the duration of the HTTP response body.

### T2b. Multi-threaded tokio runtime needed for git integration tests
`#[tokio::test]` uses a single-threaded runtime by default. Because the tests
run blocking `git` CLI commands via `std::process::Command::output()`, they block
the only tokio thread and starve the Axum server spawned on the same runtime.
We use `#[tokio::test(flavor = "multi_thread")]` and `spawn_blocking` for all
git CLI calls in integration tests.

### T3. Always-compare branch policy
The spec explicitly chose "branches always latest" over TTL-based staleness.
This means every branch clone/fetch does at least one `ls-remote` round-trip to
GitHub. For high-traffic scenarios, the single-flight dedup amortizes this.

### T4. Full `ls-remote --heads` rather than per-branch
We compare all branch heads in one `ls-remote` call rather than checking
individual branches. This is slightly more data transferred but avoids multiple
round-trips and lets us update all stale branches in one fetch pass.

---

## Things I changed from existing code

### C1. New `Git::upload_pack_stream` method
Added alongside the existing buffered `upload_pack_stateless_rpc`. Returns a
streaming reader + child process handle for the API to manage.

### C2. New `Git::ls_remote_heads` method
Returns parsed branch→SHA map from `git ls-remote --heads --symref`.

### C3. `GitRemoteConfig` added to `AppConfig`
Optional field with `#[serde(default)]`. Zero impact on existing configs.

### C4. Direct route added conditionally
The `/git/{*repo_path}` route is only registered when `git_remote.enabled = true`.
Existing deployments are unaffected.

---

# Implementation Notes: Incremental Generations & Compaction

## Overview
Implementing the plan from PR #23: generation publishing now creates delta
bundles when a repository has a previous generation head, and generation chains
can be compacted into a new full-bundle root generation.

## Decisions not in the spec

### 1. Keep old notes and append this section
PR #23's diff deleted the prior `implementation-notes.md`, but the request for
this implementation explicitly asked for a running notes file. I preserved the
previous notes and appended a new section instead of replacing the file.

### 2. Compaction API returns `CompactionReport`
The plan's contract summary listed `compact_generation_chain` as returning
`Option<GenerationId>`, while the detailed section introduced `CompactionReport`.
I used `Option<CompactionReport>` for the domain API so callers get the new
generation plus old depth, old generations, and approximate bytes reclaimed.

### 3. Dry-run compaction reserves a synthetic generation ID
`git-cache compact --dry-run` reports the generation ID that would be used if it
ran now. It does not write bundles or manifests. Running the real compaction
later will allocate a different UUIDv7.

### 4. Repointing current manifests only
Compaction rewrites canonical commit manifests, canonical ref manifests, and the
default-branch manifest when they point at a compacted generation. Historical
`ref-updates` observation manifests are left intact because they are append-only
audit/history records keyed by the old generation.

### 5. Commit list ordering during compaction
Hydration walks chains from head to root, but compacted manifests store commits
in root-to-head order for readability and deterministic tests.

## Tradeoffs

### T1. Approximate `bytes_reclaimed`
The object-store trait does not expose metadata, so compaction computes
`bytes_reclaimed` by reading old bundles and summing their byte lengths. This is
accurate for local/S3 object bytes but costs extra reads during compaction.

### T2. Head update after generation publish
The generation bundle and per-commit/ref manifests are published through the
existing atomic-ish `GenerationPublish` helper, then `RepoGenerationHead` is
written afterward. If the final head write fails, the generation still exists and
can be found by commit/ref manifests; the next publish treats the old head as the
delta base and may duplicate some objects rather than corrupting state.

### T3. Fallback resets the chain
When incremental bundle creation fails, the code deletes any partial bundle file,
creates a full bundle, sets `parent_generation: None`, and resets
`tip_commits` to the current commit. This favors safe recovery over preserving
the old chain when the local hot cache cannot prove it has the old tips.

## Things I changed from the plan

### C1. CLI `--all` enumerates generation-head manifests
There is no repository registry, so `git-cache compact --all` lists
`repos/*/manifests/generation-head.json` in the object store and compacts those
repos.

### C2. Inline compaction ignores "nothing to compact"
When `[compaction].inline = true`, publish calls compaction after writing the new
head and treats `Ok(None)` as a no-op. Errors still propagate because inline
compaction is part of the publish path when enabled.

### C3. `Git::run` already supports dynamic args
The git wrapper's `run` accepts any `IntoIterator<Item = impl AsRef<OsStr>>`, so
no separate `run_vec` helper was needed for `bundle_create_incremental`.

---

# Implementation Notes: Verified S3 Generations and Deferred Fsck

## Overview

We are changing the cache contract so clone/materialize requests do not block on
full-repo `git fsck --connectivity-only`. Durable S3 cache data is trusted only
when there is a verified sidecar manifest for the exact bundle bytes. Request
paths should serve from local/upstream-fetched commits immediately, then enqueue
background verification/publication for future cold workers.

## Decisions not in the spec

### 1. Use a verified sidecar instead of rewriting generation manifests

I added `VerifiedGenerationManifest` as a sidecar at
`repos/{repo}/generations/{generation}/verified.json`. This lets durable commit
and ref manifests mean "verified" while keeping existing generation manifest
shape mostly stable during the refactor.

### 2. Check SHA-256 from disk after object-store download

Hydration now downloads the bundle to disk, computes SHA-256 with a bounded
1 MiB buffer, checks byte length and digest, then runs `git fetch` from the
bundle. This avoids loading large bundles into memory and follows the repo's
bounded-allocation rules.

### 3. Keep one publish-time fsck temporarily

The first implementation slice still keeps `publish_generation`'s fsck because
that is currently what makes the new verified sidecar truthful. The remaining
work is to move that verification into an async background publisher so the
request path no longer pays for it.

### 4. Create `TODOs.md` as active tracker

The repo already had `TODO.md`, but the request explicitly asked to refresh
`TODOs.md`. I created `TODOs.md` as the active tracker for this local-agent
work rather than rewriting the historical milestone file.

## Tradeoffs

### T1. Runtime applies a physical v2 object-store namespace

The code still uses existing logical object keys (`repos/...`,
`pending-generations/...`) inside the object-store adapter. Runtime store wiring
now adds a physical v2 suffix to the configured namespace so a deploy
automatically writes to a clean keyspace:

- local root `./tmp/object-store` resolves to `./tmp/object-store-v2`;
- S3 prefix `repos` resolves to `repos-v2`;
- already-suffixed values such as `repos-v2` are left unchanged.

### T2. Verified hydrate rejects missing sidecars

Cold hydration now requires `verified.json`. That is intentional for the clean
v2 model: unverified S3 data should not be rescued by request-path fsck. In a
fresh v2 deployment, missing verified manifests will cause fallback paths to use
upstream/local fetch and enqueue publication.

## Final deferred-fsck implementation notes

### 5. Pending generation publishes are verifier work items

The request path now uploads the generated bundle and writes a pending work item
under `pending-generations/{repo}/{generation}.json`. It does **not** write
canonical generation, commit, ref, default, or generation-head manifests. This
keeps the v2 invariant clear: canonical manifests are durable verified state;
pending publishes are internal work queue state.

### 6. Canonical manifests are verifier-gated

The background verifier downloads the pending bundle chain into a scratch bare
repo, fetches each bundle, runs `git fsck --connectivity-only`, computes bundle
length/SHA-256, writes `verified.json`, then publishes canonical generation,
commit/ref/default, and generation-head manifests. Hydration reads only canonical
generation manifests and still requires the verified sidecar.

### 7. Request paths serve local refs instead of waiting for manifests

Branch/default materialization after upstream validation now creates sessions
from the locally fetched `refs/cache/upstream/heads/*` ref. This preserves fast
request responses while durable manifests appear asynchronously after verifier
success.

### 8. Bounded background verification

A new `max_concurrent_generation_verifications` config field bounds concurrent
background verifier tasks with a semaphore. The default is `1` to avoid multiple
large-repo fscks competing for CPU/disk IO. Example configs were updated.

### 9. Verification publication ordering

For a pending generation, the verifier publishes the verified sidecar before the
canonical generation manifest, then commit/ref manifests, then generation-head.
Tests that assert generation-head state now wait for the head because commit
manifests can become visible slightly before the head write.

### 10. Compaction follows the same pending path

Compaction no longer writes an unverified canonical generation manifest. It
publishes its compacted bundle as a pending generation, runs verification, then
repoints canonical manifests and updates the head.

## Tradeoffs added during final implementation

### T3. Pending generation recovery is request-driven

The verifier is enqueue-on-publish, but API startup no longer scans the global
`pending-generations/` prefix. That eager scan could turn a deploy into minutes
of unsolicited LLVM-scale bundle verification and mask otherwise-hot direct Git
requests. Foreground materialize and direct Git want handling now check for a
matching pending generation before fetching from upstream, so a process exit
after writing pending metadata is recovered by the next request for that commit.

### T4. Configured object-store namespace remains the base name

Operators can keep existing config values such as `prefix = "repos"` or
`root = "./tmp/object-store"`. The domain state builder appends the v2 suffix at
runtime and avoids double-suffixing if a config already contains `-v2` or `v2`.

---

# Implementation Notes: HTTPS Authorization

## Overview

Implemented the first HTTPS auth slice from `AUTH-PLAN.md`: request-scoped
Basic authorization can be forwarded to upstream git commands without putting
credentials in argv, logs, local repo config, object-store keys, or manifests.
Authenticated materialization now creates bearer-protected session URLs. Direct
`/git/...` now has a repo-access gate before upload-pack serving: all HTTPS
providers first use anonymous Smart HTTP protocol v2 to prove public
reachability, GitHub private requests fall back to REST only when that public
probe fails, and GitLab/Bitbucket/generic private requests fall back to
request-scoped Git protocol proof.

## Decisions not in the spec

### 1. Auth type lives in `git-cache-core`

`UpstreamAuth` is in core so API, domain, and git wrapper code share one parser
and redacted `Debug` implementation. It accepts only `Basic ...` for this plan,
rejects empty/control/NUL-containing headers, and exposes the raw header only to
the git wrapper execution boundary.

### 2. Keep anonymous `/v1/resolve` compatibility

Existing `/v1/resolve` behaved like materialize and returned a session URL. I
kept that behavior for anonymous requests to avoid breaking current tests and
callers. Authenticated `/v1/resolve` uses the new no-session `ResolveResponse`
shape with `cache_available` and `authorized_at`.

### 3. Protected sessions reuse the existing session repo layout

Protected sessions still create a per-session bare repo with alternates to the
shared repo. The difference is session manifest protection, bearer-token
validation on every session Git request, and stricter upload-pack config:
`allowFilter=false`, `allowAnySHA1InWant=false`, and
`allowReachableSHA1InWant=false`.

### 4. Session tokens are returned once

The raw `gcs_...` session token is returned only in the materialize response.
Only its SHA-256 hash is stored in the session manifest. Public sessions keep
`session_token: None` and do not require bearer auth.

## Tradeoffs

### T1. Direct authenticated `/git/...` is repo-gated, not fully ephemeral

The plan asks for an ephemeral protected serving repo for authenticated direct
Git. This implementation does not yet build that separate direct-remote repo.
Instead, direct upload-pack proves repo-level access at the API boundary and
then serves from the shared repo. Authenticated GitHub requests prove access via
REST before entering the domain path; GitLab/Bitbucket/generic private requests
prove access with authenticated `git ls-remote`; public requests use anonymous
Smart HTTP proof. This closes the old "cached object proves access" hole for
anonymous callers while avoiding duplicated authorized/unauthed domain methods.
The full ephemeral-repo isolation still belongs in a later hardening pass.

### T2. Anonymous direct Git now also gets upstream proof for wants

To prevent authenticated private cache entries from leaking to anonymous exact
SHA fetches, anonymous `/git/.../git-upload-pack` also checks upstream before
serving non-advertised wants. Public exact-SHA fetches still work when upstream
can provide the object anonymously.

### T3. Authenticated exact commits require reachability context

Bare exact and short commit selectors are rejected in authenticated mode. The
new selector shape `{"commit":"...","reachable_from":{"branch":"main"}}`
authorizes the branch/default ref first, fetches that ref with the same auth,
and proves ancestry locally before creating a protected session.

### T4. Process-wide `upstream_auth_token_env` remains trusted-deployment only

I left the existing process-wide bearer-token injection in place for older
single-tenant/trusted deployments. Request-scoped auth is separate and overrides
per-command git config env when present.

## Things I changed from the plan

### C1. Added auth-scoped git execution context

Request-scoped upstream auth lives on a cloned `Git` execution context created
with `with_upstream_auth(remote_url, auth)`. The ordinary git wrapper methods
(`ls_remote_heads`, `fetch_branch`, `fetch_refs`, `fetch_object`, and
`fetch_all_heads`) remain the only command builders; the scoped context injects
`http.https://.../.extraHeader` through `GIT_CONFIG_*` env. Tokens stay out of
argv and debug logging.

### C2. Added bearer-protected session manifests

`SessionManifest` now has `SessionProtection::{Public,BearerToken}`. Session
Git endpoints parse `Authorization: Bearer ...`, hash the presented token, and
reject missing/wrong tokens with 401.

### C3. Added protected direct ref advertisements

Authenticated direct `info/refs` uses a reduced synthesized capability set that
omits `filter`, `allow-tip-sha1-in-want`, and
`allow-reachable-sha1-in-want`.

### C4. Added focused regression coverage

New coverage asserts Basic auth redaction, protected session token issuance,
missing/wrong session-token rejection, protected session upload-pack success,
and preservation of existing public direct remote behavior.

## Merge conflict resolution notes

### M1. Kept main's runtime-cache completeness checks

While merging `origin/main`, I preserved its `commit_ready_for_serving` checks
for direct upload-pack wants. Authenticated wants still require current upstream
proof, but commit wants are not exposed unless both the commit and tree are
available locally.

### M2. Protected session repos do not hide their synthetic ref

The protected session repo contains only the authorized synthetic session ref
and uses alternates for objects. Hiding `refs/cache` in that repo also hides the
synthetic `refs/cache/sessions/...` ref from stateless upload-pack, so protected
sessions keep the stricter `allow*InWant=false` config but do not set
`hideRefs=refs/cache`.

### M3. Session-token test uses a real Git client flow

After the merge, the single handcrafted upload-pack POST could stop after the
negotiation `NAK` under the stricter protected config. The regression now uses
`git clone` with `http.extraHeader` injected through `GIT_CONFIG_*`, which is
closer to the workflow users actually run and keeps the token out of argv.

## Auth-context refactor notes

### R1. Keep permission proof separate from command auth plumbing

I removed the duplicate direct-remote `*_with_auth` method family. The API now
parses upstream credentials once, creates `materializer.using_upstream_auth(...)`,
and then calls the ordinary methods such as `upstream_refs`,
`handle_upload_pack`, `compare_upstream_refs`, and `fetch_changed_refs`.

### R2. Share branch fetch/publish after authorization

Authenticated branch materialization still proves the branch tip with GitHub
before serving, but the actual fetch, moved-tip check, default-branch update,
and generation publish now go through the same `ensure_branch_from_verified_tip`
helper used by the public branch path.

## Devin PR bug-report follow-up notes

### D1. Gateway bearer auth report did not reproduce on this branch

Devin's PR reported that anonymous API requests could fail if a gateway placed
`Authorization: Bearer ...` on the request. Our current API upstream-auth path
only reads `Git-Cache-Upstream-Authorization`, so the bug did not reproduce
here. I added a regression test to keep gateway bearer auth from being parsed as
upstream GitHub credentials on API endpoints.

### D2. Protected session token entropy report reproduced

The protected session token was `gcs_` plus UUID v7, which exposed timestamp
structure and provided less entropy than the plan expected. I reproduced this
with a token-shape test that required `gcs_` plus 32 bytes of lowercase hex,
then changed token generation to concatenate two UUID v4 byte arrays before
hex encoding.

### D3. PR #46 follow-up comments were reproduced before fixes

The authenticated `/v1/resolve` rate-limit bypass reproduced with a handler test
that consumed the quota first and expected 429 before host validation/upstream
work. The upstream timeout mapping issue reproduced with a git-wrapper unit test
that expected `GitCacheError::Timeout` to survive `run_upstream` error mapping.
The GIT_CONFIG clobber issue reproduced with a fake git executable that recorded
its environment; the fix now composes different-host entries and replaces a
matching same-host entry so request credentials still take precedence. The
protected-session tree check was not open as a PR #46 thread, but the code issue
reproduced with a synthetic commit object whose tree was absent.

### D4. Static-review flag triage after PR #46 fixes

The follow-up static-review flags included several stale positives after D3:
authenticated resolve is rate-limited, GIT_CONFIG auth entries compose instead
of clobbering, upstream timeout errors keep their type, and session token entropy
uses two UUID v4 byte arrays. Two small hardening items were still useful and
had focused repro tests: `MaterializeResponse` debug output now redacts
`session_token`, and session Bearer parsing accepts case-insensitive schemes
while ignoring unrelated auth schemes so public sessions are not rejected by
cached Basic credentials. The direct Git shared-repo / request-scoped proof
concerns remain known hardening tradeoffs pending ephemeral repo isolation, with
the request-scoped proof fallback tightened in D5.

### D5. Preserve auth-free public access while tightening direct want proof

The auth model remains two separate gates: service auth is deployment/app
access control, while upstream repo auth is request-scoped proof that the caller
can reach a repo/ref. This code currently has no in-app service-auth gate, so
auth-free deployments still work. `/v1/*` keeps upstream repo credentials in
`Git-Cache-Upstream-Authorization` so a service `Authorization` header can
coexist. Direct Git still uses `Authorization: Basic ...` as repo credentials
because that is what Git credential helpers naturally send, so any future
in-app service-auth gate needs a separate convention or a gateway that does not
forward its own service token as the direct Git upstream credential.

A static-review finding correctly reproduced one multitenant edge: if a
non-advertised want already existed in the shared repo, `git fetch <sha>` or the
fallback could return success without proving the current anonymous request was
allowed to receive that object. The direct want path now records whether the
object existed before the upstream fetch. Pre-existing cached wants must be
reachable from refs fetched from upstream for the current request credential;
newly fetched wants may still rely on upstream providing the object. Regression
tests cover both sides: anonymous wants reject locally cached unadvertised
commits, while cached commits reachable from current public upstream refs still
work without auth.

### D6. Domain materialization now plans repo access before serving

Raw `UpstreamAuth` stays out of `MaterializeRequest` because it is transport
context with secret-bearing data, not JSON request data. The domain now turns a
request plus upstream auth context into a `RepoAccess`/`MaterializePlan` first:
`RepoAccess::Public` for empty auth and `RepoAccess::Upstream` for request
credentials. Materialization then serves that plan without asking whether it is
on an authenticated path again. This keeps the auth-free public option while
making the principle explicit: once code reaches the serving/materialization
stage, it is operating on an already checked repo intent and may only fail if
upstream/cache state changes underneath it.

The API layer also uses helper predicates on `MaterializeRequest` and
`UpstreamAuthorizationMode` instead of repeating direct enum comparisons. The
old authenticated materialization method family was collapsed into shared
helpers that ensure a branch tip, ensure a reachable commit, and create either a
public or protected session based on `RepoAccess`.

### D7. Direct Git blob wants use graph proof, not commit-only proof

Partial clones can later ask upload-pack for blob objects that are not advertised
as ref tips. The tightened direct-want check originally proved only commit wants,
which rejected cached public blobs during checkout. Non-advertised wants now get
checked against the exact upstream tips from the current advertisement with a
bounded `git rev-list --objects` pass. This keeps the private-object guard: a
cached object is not served merely because it exists locally; it must be in the
currently authorized upstream graph or be newly provided by upstream for that
request.

### D8. Materializer split keeps behavior intact while reducing local coupling

`materializer.rs` is now a facade over focused modules for planning, repo
operations, generation publishing/compaction, direct Git serving, session
creation/cleanup, manifest access, executor wiring, and small shared utilities.
This was intentionally a structural refactor: it did not change the public/auth
model or add ephemeral direct-Git repos. The direct Git and generation modules
are still the largest pieces, but their responsibilities are now separated
enough for smaller follow-up refactors.

Session creation now goes through one `create_session_with_access` path with a
small `SessionAccess` enum, so public and bearer-protected sessions share the
readiness checks, repo preparation, manifest write, and response construction.
Manifest reads/writes used by materialization now go through `ManifestStore`;
raw prefix scans still exist for cleanup/compaction, but key deserialization is
centralized in the wrapper. Materializer tests were split into behavior modules
so future changes can add focused coverage without growing one monolithic test
file again.

### D9. Direct Git wants fetch only what the requested object needs

AWS smoke logs for `llvm/llvm-project` showed generation verification conflicts
for the current `main` commit while the host cache already had the commit and
tree locally. The surprising part was direct clone POST behavior: after the
synthesized upstream advertisement, `ensure_wants_available` fetched and
published every branch that differed from local public `refs/heads/*` before it
checked whether the requested want was already an advertised, complete cached
tip. Large repos with many active branches could therefore stall on unrelated
fetch/publish work and race with existing generation manifests.

Direct Git ref comparison now uses internal upstream cache refs
`refs/cache/upstream/heads/*`, which are the refs materialization maintains.
For an advertised want, the path serves an already-complete cached object
without fetching unrelated changed refs. If the advertised object is missing or
incomplete and has no complete commit manifest, the path fetches only the
advertised branch or branches that point at that exact object. A regression test
first reproduced the old behavior by adding an unrequested changed side branch:
the old code fetched `refs/heads/side` for a `main` want, while the fixed path
does not publish or fetch that side branch.

After an AWS control run against current `main`, public LLVM direct clones were
~1.5s while the first version of this fix was ~45s. The regression was not the
targeted fetch; it was doing full upstream ref proof twice per direct clone:
once for `info/refs`, then again for `git-upload-pack` POST. A follow-up fix
made anonymous POST trust wants reachable from public `refs/heads/*`, which are
published by prior anonymous materialization/fetches, and skip that second
upstream comparison.

That follow-up briefly refreshed every ready public ref from every anonymous
GET advertisement. AWS smoke testing still showed LLVM direct clones around
15s and Linux around 7s, because the GET path scaled with the size of the
upstream branch advertisement. We removed that GET-side sweep: anonymous GETs
still fetch a fresh upstream advertisement so branch movement is detected, but
they do not walk/update local public refs. POST uses existing public refs for
the fast path and falls back to request-scoped upstream proof when they are
missing or stale. Authenticated direct Git also avoids populating public refs.

One more AWS split test isolated the remaining LLVM direct-clone cost:
`info/refs` and GitHub `ls-remote` were ~1-2s, and a protected session clone was
~1.2s, but direct clone POST stayed around 11s. That came from checking commit
wants with `for-each-ref --contains` over LLVM's public branch set. The common
shallow-clone want is the advertised default-branch tip, so the fast path now
answers exact public-tip wants from the already-loaded `refs/heads/*` tip list
and only uses reachability for cached ancestors and non-commit wants.

### D10. Repo authorization is now an API gate for direct Git POST

The revised auth model separates the future service-auth gate from the current
upstream repo-access gate. Service auth answers "can this caller use the cache
service at all" and is still out of scope for this branch. Repo auth answers
"can this request reach this upstream repo" and now happens before
materialize/resolve/direct upload-pack serving.

Requests first perform an anonymous Smart HTTP v2 public probe:
`GET https://{host}/{owner}/{repo}.git/info/refs?service=git-upload-pack` with
`Git-Protocol: version=2`. This does not send the request token. If the probe
returns `200`, the request is treated as public even when Basic auth was
present, so materialize/resolve/direct Git continue with
`UpstreamAuth::Anonymous` and public session behavior.

If the public probe fails and request auth is present for GitHub, the API uses
GitHub REST as the private-repo fallback: it reads repository metadata and the
default branch ref with the request token. After that succeeds, domain code
treats the repo as authorized for this request and does not run a second
per-object upstream proof for direct upload-pack wants. This keeps the
implementation explicit without carrying a second `*_authorized` method family
through the materializer.

GitLab, Bitbucket, and generic HTTPS origins use the same anonymous Smart HTTP
public probe. When that fails and request auth is present, they fall back to
authenticated `git ls-remote` because we do not yet have provider-specific REST
adapters for them. This intentionally keeps provider support open while
preserving the no-token public path.

Anonymous requests deliberately do not use provider REST APIs. The public,
auth-free mode remains available, and anonymous proof stays on the provider's
Git transport so large public clones do not consume REST quota. For anonymous
direct POSTs, the hot path serves only wants already proven reachable from
locally published public refs/manifests. If that proof is missing or stale, the
request falls back to anonymous `git ls-remote`/fetch proof or fails rather
than trusting object existence in the shared repo.

This is intentionally a first provider layer rather than a full provider
interface. The next shape should move these decisions behind explicit origin
types, likely something like `GitHubOrigin`, `GitLabOrigin`,
`BitbucketOrigin`, and `PrivateGitServerOrigin`. The existing `RepoKey` remains
the three-segment `host/owner/name` shape in this branch, so GitLab nested
groups still need that later origin/key model.

### D11. Anonymous direct Git can restore public proof from matching manifests

AWS LLVM smoke testing exposed a ref-cold/object-warm state: the local bare repo
held large packs, but did not have `refs/heads/*` or
`refs/cache/upstream/heads/*`. Current `main` served this quickly because it
trusted object existence, but that is the SHA-guess leak the auth work is meant
to close. The auth branch therefore needs a bridge that restores serving proof
without treating raw object presence as authorization.

Anonymous direct Git POST now checks persisted public ref manifests before it
fetches an advertised tip from upstream. A ref manifest can publish public
serving refs only when its `refs/heads/<branch> -> commit` mapping exactly
matches the current upstream advertisement for this request. In that case the
materializer hydrates the verified generation if needed, restores both
`refs/cache/upstream/heads/<branch>` and public `refs/heads/<branch>`, updates
`HEAD` for the advertised default branch, and then continues through the normal
local public-reachability path.

Stale public ref manifests are still useful, but only as hidden fetch
negotiation bases. Before materialize/direct-Git fetches a newer advertised
branch tip, it may hydrate the last verified generation for that branch and
restore only `refs/cache/upstream/heads/<branch>`. It does not restore public
`refs/heads/<branch>` for stale manifests, because stale refs must not become
anonymous serving proof.

This deliberately is not a visibility cache. If GitHub/GitLab/Bitbucket now
advertises a different tip, the manifest is stale for this request and can only
help Git negotiate a smaller fetch. Public serving refs are restored only for
commits whose public ref mapping is still current.
