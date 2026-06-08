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

Implemented the HTTPS auth slice from `AUTH-PLAN.md`: request-scoped Basic
authorization can be forwarded to upstream git commands without putting
credentials in argv, logs, local repo config, object-store keys, or manifests.
Credentialed materialization creates bearer-protected session URLs.

The final simplification for this PR is repo-level access first, then
main-like materialize/resolve/direct serving. The API parses upstream auth and
checks only caller intent such as "auth required"; the domain proves repo/ref
access by contacting upstream with the selected auth. There is no GitHub REST
provider layer, no token-present-to-anonymous downshift, and no parallel
authenticated materializer implementation.

## Decisions not in the spec

### 1. Auth type lives in `git-cache-core`

`UpstreamAuth` is in core so API, domain, and git wrapper code share one parser
and redacted `Debug` implementation. It accepts only `Basic ...` for this plan,
rejects empty/control/NUL-containing headers, and exposes the raw header only to
the git wrapper execution boundary.

### 2. `/v1/resolve` is lightweight for all callers

`/v1/resolve` now consistently returns `ResolveResponse` with the resolved
commit, source label, `cache_available`, and `authorized_at`. It does not create
sessions and does not fall through to materialize for anonymous callers.

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

### T1. Direct `/git/...` is repo-gated, not fully ephemeral

The plan asks for an ephemeral protected serving repo for authenticated direct
Git. This implementation does not yet build that separate direct-remote repo.
Instead, direct `info/refs` proves repo-level access with the request's selected
upstream auth and direct `git-upload-pack` serves from the shared repo after
local readiness checks. This follows the simplification rule that repo access is
the authorization boundary. Deployments that need stricter isolation for
rewritten or hidden history should keep that history in a separate upstream repo
or revisit ephemeral direct serving repos later.

### T2. Direct upload-pack read-through is repo-authorized

Direct Git POST must not run object-level upstream reachability proof. Its job
is to parse wants after repo access is proven, serve already-ready commits,
hydrate complete commit manifests when available, fetch missing wanted commits
from upstream using the same request auth when read-through is enabled, require
commit wants to have their tree object before exposure, publish newly imported
generations, and spawn `git upload-pack`.

This intentionally accepts repo-level access as sufficient for all objects in
the repo-scoped cache. It preserves current `main` behavior where `git clone`
can read through without prior materialization, while avoiding the duplicated
authenticated/unauthed method families and per-object authorization machinery.

### T3. Exact commits use repo-level access

Bare exact and short commit selectors are allowed for credentialed and anonymous
requests after the repo access check. The tradeoff is explicit: a caller who can
read a repo may materialize cached history from that repo even if a commit is no
longer reachable from an advertised upstream ref. A future hardening option can
add a "current reachability required" policy, but it is not the default for this
PR.

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
for direct upload-pack wants. Commit wants are not exposed unless both the
commit and tree are available locally.

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

### D5. Preserve auth-free public access with one repo-access model

The auth model remains two separate gates: service auth is deployment/app
access control, while upstream repo auth proves that the caller can reach a
repository through upstream Git. This code currently has no in-app service-auth
gate, so auth-free deployments still work. `/v1/*` keeps upstream repo
credentials in `Git-Cache-Upstream-Authorization` so a service `Authorization`
header can coexist. Direct Git still uses `Authorization: Basic ...` as repo
credentials because that is what Git credential helpers naturally send.

For this PR, repo access is the security boundary. After a request proves it can
read the upstream repo with either anonymous auth or supplied credentials,
materialize, resolve, and direct upload-pack stop asking whether the path was
"authenticated" or "anonymous". This intentionally avoids separate authorized
method families.

### D6. Domain materialization plans access and target together

Raw `UpstreamAuth` stays out of `MaterializeRequest` because it is transport
context with secret-bearing data, not JSON request data. The API parses auth and
passes it through `Materializer::using_upstream_auth`; the domain then builds a
`MaterializePlan` containing a `RepoAccessContext` and a target commit/ref.

Branch and default-branch selectors prove access by resolving the upstream tip
with the selected auth. Exact commit selectors first run a lightweight
`ls_remote_default_branch` repo-access check, then use the main-like exact
commit flow. Short commits fetch upstream refs with the selected auth before
resolving. The old authenticated materialization method family was removed.

### D7. Direct Git wants are read-through availability checks after repo proof

Partial clones can later ask upload-pack for blob/tree objects that are not ref
tips. Under the simplified repo-boundary model, direct Git POST no longer runs
per-object upstream graph proof. After GET or POST proves repository access, the
domain treats object presence as cache state: if a wanted commit is already
ready it is served, if a complete commit manifest exists it is hydrated, and if
read-through is enabled it is fetched from upstream using the same request auth.
Commit wants still require `commit_ready_for_serving` before upload-pack can
expose them, so a commit without its tree is not served.

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

### D9. Direct Git POST preserves main-like read-through

AWS smoke logs for `llvm/llvm-project` showed generation verification conflicts
for the current `main` commit while the host cache already had the commit and
tree locally. The surprising part was direct clone POST behavior: after the
synthesized upstream advertisement, `ensure_wants_available` fetched and
published every branch that differed from local public `refs/heads/*` before it
checked whether the requested want was already an advertised, complete cached
tip. Large repos with many active branches could therefore stall on unrelated
fetch/publish work and race with existing generation manifests.

After an AWS control run against current `main`, public LLVM direct clones were
~1.5s while the first auth-aware iteration was ~45s. Several follow-up
optimizations tried to recover that latency by making direct Git advertise only
locally ready refs and reject local cache misses. That preserved hot latency,
but it broke an important property of current `main`: `git clone`
should work without a prior `/v1/materialize`.

The current simplification keeps the repo-access check boring and leaves
availability to the main-like direct Git read-through path. Direct Git GET
proves that the selected auth can read the upstream repo and advertises the
current upstream refs without fetching objects. Direct Git POST then parses the
wants, hydrates complete commit manifests when available, otherwise fetches the
wanted commit from upstream using the same request-scoped auth, publishes a
generation for newly imported commits, configures the serving repo, and spawns
`git upload-pack`.

### D10. Repo access proof uses Git transport, not provider REST

The revised auth model separates the future service-auth gate from the current
upstream repo-access gate. Service auth answers "can this caller use the cache
service at all" and is still out of scope for this branch. Repo auth answers
"can this request reach this upstream repo" and is proven by ordinary upstream
Git operations using the selected `UpstreamAuth`.

No token means anonymous Git access. Token present means credentialed Git
access. The implementation does not classify public/private repositories with
GitHub REST and does not downshift token-present requests to anonymous mode. A
bad supplied token is therefore visible to the caller instead of silently
falling back to public access.

For GitHub, GitLab, Bitbucket, and other allowed HTTPS hosts, this pass keeps
one provider-neutral mechanism: `ls-remote`/Smart HTTP through the existing git
wrapper. A future provider layer can introduce `GitHubOrigin`, `GitLabOrigin`,
`BitbucketOrigin`, or `PrivateGitServerOrigin` if measurements show a real need,
but this PR avoids adding that complexity.

### D11. Direct proof cache is only a GET-to-POST auth handoff

Direct Git GET fetches upstream refs to prove repo access, then synthesizes the
Smart HTTP advertisement from that upstream state. It stores the comparison for
a short TTL keyed by repo and the exact auth fingerprint. Direct Git POST may
use the matching entry to avoid a second upstream ref call. If the handoff is
absent, expired, or keyed to different credentials, POST re-runs the same
lightweight upstream ref fetch before read-through serving.

The proof cache deliberately does not authorize individual objects, persist
across process restarts, downshift credentialed requests to anonymous proofs, or
authorize across credentials. It is a hot-path optimization for the stateless Git
GET/POST pair, not a repository visibility cache.

### D12. Direct upload-pack has one read-through entrypoint

Domain serving has one `handle_upload_pack` path for parsing wants, hydrating or
fetching missing wanted commits, serving repo configuration, and spawning
`git upload-pack`. The optional upstream-ref comparison argument remains only as
API context for the GET-to-POST handoff; the domain no longer has separate
authenticated, proof-specific, or fallback-fetch upload-pack implementations.

### D13. Materialize keeps main-like branch behavior

The branch/default materialize path is intentionally close to current `main`:
resolve the upstream tip with the selected auth, serve immediately if the commit
and tree are already local, otherwise fetch the target branch, verify the fetched
SHA matches the earlier upstream tip, publish the generation, and create a
session. Request-scoped auth changes which upstream credentials are used and
whether the session is protected; it does not fork the branch materializer.

### D14. Exact commits use repo-level authorization

For the materialize API, repo access is now treated as sufficient authorization
for exact commit selectors. Exact commit materialization first proves repo access
with `ls_remote_default_branch`, then uses cached manifests/local generation
indexes where possible, or fetches upstream refs with the same auth when needed.
The protected session still requires its bearer token and is pinned to the
materialized commit.

The tradeoff is that a caller with repo access can materialize a cached commit
for that repo even when the commit is not currently reachable from an advertised
upstream ref. That matches the simpler repository-boundary model we want here:
Git history is effectively repo-scoped, and operators should keep truly
sensitive histories in separate repositories rather than relying on hidden or
rewritten commits inside the same repo. The code carries a TODO to make
"current upstream reachability" an explicit optional policy if deployments need
that stricter behavior later.

### D15. Resolve uses the same selector policy

`/v1/resolve` now always returns the lightweight `ResolveResponse` shape after
the same repo-access gate used by materialize. It no longer falls through to
materialize for anonymous requests, so callers should not expect a session URL
from resolve. The response reports the resolved commit, whether that commit is
locally cache-available, and the source label.

The selector policy now matches materialize: repo access is sufficient for exact
commit selectors, and `reachable_from` is not required by default. Direct Git
upload-pack follows the same repo-boundary policy and differs only in outcome:
it streams from local cache when ready, hydrates or fetches the wanted commit
when read-through is needed, and fails only when the authorized repo cannot
provide the requested object or read-through is disabled.

### D16. Remaining simplification direction

The remaining cleanup should be small and mechanical: keep `RepoAccessContext`
as an access/session/source label, keep auth parsing at API boundaries, keep Git
auth env composition in the git wrapper, and avoid adding provider-specific
helper families until there is a measured need. Materialize and resolve should
continue to look as much like current `main` as possible, with request-scoped
auth carried as upstream Git credentials and session/publication policy.

### D17. API materialize no longer pre-runs the worker coordinator

AWS LLVM benchmarking exposed a bad interaction between the HTTP materialize
path and `UpdateCoordinator::read_through`: a branch request whose commit/tree
were already present locally could still join or start warmer-style generation
work before the unified materializer saw the hot local state. For LLVM this
looked like an idle client connection or an LLVM-sized pending generation
verification instead of a fast session response.

The API now calls `Materializer::materialize` directly after rate limiting and
upstream-auth header checks. The worker coordinator remains available for cron,
event hints, and explicit warming, but it is no longer in front of interactive
HTTP materialize/resolve handling. A regression test covers the hot local branch
case by warming the bare repo, calling `/v1/materialize`, and asserting that no
generation bundle or pending generation verification is written.

### D18. Pending generation verification has a local-repo fast path

LLVM benchmarking also exposed a second, older cost: after publishing a new
incremental generation, verification fetched the whole generation chain into a
temporary repo. For a repo with a large verified parent generation, that meant
downloading and `index-pack`ing gigabytes of already-verified history before
later hot requests could get a fair share of CPU and disk.

Verification now first tries the normal HTTP-publish case: if the local repo
still has the new generation's tip objects and every parent generation is
already verified, the publisher verifies the just-created bundle file with
`git bundle verify`, records its size/SHA-256, and writes verified generation
metadata immediately. That proves the bundle prerequisites without unpacking
the full parent chain again or re-downloading the bundle from object storage.

If a generation is already pending, the background verifier uses the same local
proof and downloads only that pending bundle. If the local repo is missing,
incomplete, or the parent chain is not already verified, automatic background
verification leaves the generation pending instead of falling back to full-chain
replay. Explicit verification paths used by compaction/recovery still keep the
original full-chain fallback. Regression tests cover both the synchronous
publish path and a child pending generation whose verified parent bundle has
been deleted.
