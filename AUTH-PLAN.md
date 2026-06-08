# HTTPS Authorization Plan

## Summary

This system will support private GitHub repositories through HTTPS token
passthrough. GitHub remains the source of truth for authorization on every
private request. The cache is allowed to store Git objects, bundles, and
availability metadata, but it must never treat cached bytes as proof that a
caller is allowed to read those bytes.

The first implementation target is HTTPS tokens only:

- GitHub personal access tokens (classic and fine-grained), used as the HTTPS
  Git password.
- GitHub OAuth app access tokens, forwarded to GitHub using normal HTTPS Basic
  authentication patterns.

SSH private keys, SSH agent forwarding, GitHub App installation-token edge
cases, and long-lived credential exchange APIs are out of scope for this plan.

## Sources And Assumptions

GitHub documents that personal access tokens can be used in place of a
password for Git operations over HTTPS:

- https://docs.github.com/authentication/keeping-your-account-and-data-secure/creating-a-personal-access-token

GitHub's historical OAuth-over-HTTPS Git pattern uses Basic authentication with
the OAuth token and `x-oauth-basic`:

- https://github.blog/news-insights/easier-builds-and-deployments-using-git-over-https-and-oauth/

The cache should not need to understand whether a token is a PAT or an OAuth
app token in order to make an authorization decision. It should forward a
redacted, validated upstream Authorization header to GitHub and let GitHub
accept or reject it.

## Design Goals

1. Work naturally with `git clone` and `git fetch` over HTTPS.
2. Support GitHub PATs and GitHub OAuth app access tokens.
3. Avoid credential exchange or durable credential storage.
4. Re-authorize against GitHub per private request.
5. Share cached Git bytes across tenants only when each serving request is
   independently authorized by GitHub.
6. Keep `/git/...`, `/v1/materialize`, and `/v1/resolve` covered by the same
   authorization invariant.
7. Prevent cached private objects from leaking through arbitrary SHA wants,
   stale sessions, public endpoints, logs, manifests, or object-store keys.

## Non-Goals

1. Acting as an identity provider or authorization source of truth.
2. Persisting user credentials, public keys, private keys, or token-derived
   identities.
3. Implementing SSH private-key passthrough.
4. Supporting push. `git-receive-pack` remains disabled.
5. Guaranteeing private cache hits when GitHub is unavailable. For private or
   authenticated requests, GitHub unavailability is a serve failure.
6. Solving `/v1/materialize` and `/v1/resolve` service-to-service
   authentication in full. This plan defines upstream auth handling; caller auth
   to the cache service can remain deployment-specific for now.

## Core Invariants

For private or authenticated traffic:

```text
serve = upstream_authorized_now && requested_objects_are_authorized && objects_available_or_fetchable
```

Never:

```text
serve = object_cached
```

Additional invariants:

1. A cached object is only availability evidence.
2. Authorization is request-scoped and comes from GitHub.
3. Tokens are never written to object storage, manifests, local repo config,
   logs, metrics labels, trace spans, or argv.
4. Public and authenticated serving paths must be separated in code.
5. Protected serving repos must not enable arbitrary object wants.
6. Session URLs for protected materializations require a separate session bearer
   token on every Git HTTP request.

## Credential Handling

### Git Smart HTTP

For `/git/...`, the cache accepts the `Authorization` header sent by Git. In
normal Git HTTPS use this is Basic auth.

Examples:

```sh
git clone https://git-cache.example.com/git/github.com/acme/private-repo.git
```

Git may prompt:

```text
Username: <github-username>
Password: <github-pat>
```

Non-interactive PAT example:

```sh
GITHUB_TOKEN=github_pat_... \
git -c credential.helper='!f() { echo username=x-access-token; echo password="$GITHUB_TOKEN"; }; f' \
  clone https://git-cache.example.com/git/github.com/acme/private-repo.git
```

OAuth app token example using the historical GitHub pattern:

```sh
GITHUB_OAUTH_TOKEN=gho_... \
git -c credential.helper='!f() { echo username="$GITHUB_OAUTH_TOKEN"; echo password=x-oauth-basic; }; f' \
  clone https://git-cache.example.com/git/github.com/acme/private-repo.git
```

The cache should not log, parse for identity, or store these credentials. It
only translates the inbound header into a per-command GitHub upstream header.

### HTTP APIs

For `/v1/materialize` and `/v1/resolve`, keep cache-service caller auth separate
from upstream GitHub auth.

Recommended headers:

```http
Authorization: Bearer <cache-service-token>
Git-Cache-Upstream-Authorization: Basic <base64-github-credential>
```

`Authorization` is for the cache service. `Git-Cache-Upstream-Authorization` is
for GitHub only.

For deployments that do not yet require cache-service auth, the upstream header
still gives us a clean boundary and avoids overloading `Authorization`.

## Upstream Auth Representation

Add a redaction-safe type in `git-cache-core` or `git-cache-domain`:

```rust
pub enum UpstreamAuth {
    Anonymous,
    Basic { redacted: RedactedHeader, raw: SecretString },
}
```

Implementation notes:

1. Reject empty auth header values.
2. Reject values containing NUL or control characters.
3. Accept only `Basic ...` for this plan.
4. Do not decode unless needed for validation. GitHub can validate the Basic
   credential.
5. Redacted display should be scheme only, for example `Basic <redacted>`.
6. Keep raw values out of `Debug` output.

Per-command Git injection should use environment-based Git config:

```text
GIT_CONFIG_COUNT=1
GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader
GIT_CONFIG_VALUE_0=Authorization: Basic <redacted-at-log-boundary-raw-at-exec-boundary>
```

Never put the token in:

1. Remote URL.
2. Command argv.
3. Local repo config.
4. Object-store key.
5. Manifest payload.

## Public Versus Authenticated Requests

An inbound request with no upstream auth is public-mode.

Public-mode behavior:

1. Query GitHub anonymously when strict freshness is needed.
2. Existing public cache-hit optimizations can remain.
3. Existing public direct remote behavior can remain if it does not expose
   protected/private refs or objects.

Authenticated-mode behavior:

1. Always ask GitHub with the provided credential before serving.
2. Never fall back to cached private data if GitHub rejects or is unavailable.
3. Use protected serving configuration.
4. Create protected sessions requiring session bearer tokens.

Important: the cache cannot reliably know whether a repo is public or private
without asking GitHub. Therefore, if upstream auth is present, use
authenticated-mode even if the repo is public.

## `/git/...` Direct Remote

Route:

```text
GET  /git/{host}/{owner}/{repo}.git/info/refs?service=git-upload-pack
POST /git/{host}/{owner}/{repo}.git/git-upload-pack
```

`git-receive-pack` stays disabled for both public and authenticated traffic.

### GET `info/refs`

Authenticated flow:

1. Parse repo path.
2. Validate upstream host allowlist.
3. Extract inbound `Authorization`.
4. Run `git ls-remote --symref` against GitHub with that auth.
5. If GitHub returns 401/403, return 401/403 to the caller.
6. If GitHub is unavailable, return 503.
7. Synthesize a ref advertisement from GitHub's authorized refs.
8. Do not fetch objects during the GET.

The ref advertisement is an authorization result for the current request only.
It is not durable authorization metadata.

### POST `git-upload-pack`

Authenticated flow:

1. Parse repo path.
2. Validate upstream host allowlist.
3. Extract inbound `Authorization` again.
4. Re-run `git ls-remote --symref` with the same request credential, or use a
   very short in-memory GET/POST pair result keyed by a nonce if we later add
   such a nonce.
5. Parse want lines from the upload-pack request.
6. Validate every want against the current authorized ref set.
7. Fetch missing authorized objects from GitHub with the same auth.
8. Serve through an ephemeral protected repo view.

Initial conservative want policy:

1. Allow wants that exactly match currently authorized advertised ref tips.
2. Allow wants already proven reachable from currently authorized ref tips in
   the local cache.
3. For arbitrary wants not proven reachable, attempt a GitHub fetch with this
   auth. Serve only if GitHub provides the object and reachability is proven or
   the object is the fetched authorized target.
4. Otherwise reject with 403 or 404. Prefer 403 when the request is malformed
   from an auth perspective, and 404 when GitHub verified absence.

### Ephemeral Protected Serving Repo

Do not serve authenticated requests directly from the shared repo.

Use a per-request or short-lived per-session bare repo:

```text
cache/protected-sessions/{id}.git
```

It may point to the shared object database through alternates, but it must only
contain refs authorized for the current request.

Protected upload-pack config:

```ini
[uploadpack]
    allowFilter = false
    allowAnySHA1InWant = false
    allowReachableSHA1InWant = false
[transfer]
    hideRefs = refs/cache
```

Filtering can be re-enabled later only after we have tests proving it cannot
expand access beyond the authorized object graph.

## `/v1/resolve`

Purpose: resolve selectors against GitHub authorization without creating a Git
session or fetching object bundles.

Request:

```json
{
  "repo": "github.com/acme/private-repo",
  "selector": { "branch": "main" },
  "mode": "strict",
  "upstream_authorization": "required"
}
```

Headers:

```http
Authorization: Bearer <cache-service-token>
Git-Cache-Upstream-Authorization: Basic <base64-github-credential>
```

Response:

```json
{
  "repo": "github.com/acme/private-repo",
  "selector": { "branch": "main" },
  "commit": "abc123abc123abc123abc123abc123abc123abcd",
  "source": "upstream_authorized",
  "cache_available": true,
  "authorized_at": "2026-06-04T00:00:00Z"
}
```

Branch/default behavior:

1. Ask GitHub with the provided upstream auth.
2. Resolve to a commit if GitHub authorizes the ref.
3. Optionally report whether the commit is already available in the cache.
4. Do not hydrate, fetch, publish, or create a session.

Exact commit behavior:

1. For public/anonymous mode, existing exact commit behavior may remain.
2. For authenticated mode, a naked commit SHA is not enough unless GitHub
   itself can verify/fetch it with this credential.
3. Preferred request shape adds reachability context:

```json
{
  "repo": "github.com/acme/private-repo",
  "selector": {
    "commit": "abc123abc123abc123abc123abc123abc123abcd",
    "reachable_from": { "branch": "main" }
  },
  "mode": "strict",
  "upstream_authorization": "required"
}
```

If the reachability ref is authorized and the commit is reachable from it,
return `upstream_authorized`. If not, reject.

## `/v1/materialize`

Purpose: resolve authorization, ensure bytes exist, and create a protected Git
session that the caller can fetch from.

Request:

```json
{
  "repo": "github.com/acme/private-repo",
  "selector": { "branch": "main" },
  "mode": "strict",
  "upstream_authorization": "required"
}
```

Headers:

```http
Authorization: Bearer <cache-service-token>
Git-Cache-Upstream-Authorization: Basic <base64-github-credential>
```

Response:

```json
{
  "repo": "github.com/acme/private-repo",
  "commit": "abc123abc123abc123abc123abc123abc123abcd",
  "source": "upstream_authorized_cache_hit",
  "verified_at": "2026-06-04T00:00:00Z",
  "git_url": "https://git-cache.example.com/git/session/01J.../github.com/acme/private-repo.git",
  "ref": "refs/cache/sessions/01J...",
  "session_token": "gcs_...",
  "expires_at": "2026-06-04T01:00:00Z"
}
```

Branch/default flow:

1. Ask GitHub with upstream auth.
2. Resolve branch/default to commit.
3. If commit is cached and complete, hydrate local storage if needed.
4. If commit is missing, fetch the exact authorized ref from GitHub with the
   same auth.
5. Verify fetched commit matches the prior `ls-remote` result.
6. Publish generation/commit availability metadata.
7. Create a protected session manifest.
8. Return session URL, synthetic ref, and session bearer token.

Exact commit flow:

1. Require reachability context or prove through GitHub fetch.
2. Authorize the reachability ref with GitHub.
3. Prove the requested commit is reachable from the authorized ref.
4. Serve from cache or fetch as needed.
5. Create a protected session.

Protected session fetch:

```sh
git -c http.https://git-cache.example.com/.extraHeader="Authorization: Bearer $SESSION_TOKEN" \
  fetch "$GIT_URL" "$REF"
```

Session endpoint behavior:

1. Parse and validate `Authorization: Bearer <session_token>`.
2. Lookup the session manifest by id.
3. Compare a hash of the presented session token with the stored token hash.
4. Check expiration.
5. Serve only the session synthetic ref.
6. Use protected upload-pack config.

## Response Source Values

Replace or supplement the current `MaterializeSource` values for authenticated
mode:

```text
upstream_authorized_cache_hit
upstream_authorized_fetched
public_cache_hit
public_fetched
```

Avoid `cache_verified` for private/authenticated traffic because it is too easy
to misread as authorization.

## Data Model Changes

### MaterializeRequest

Current:

```rust
pub struct MaterializeRequest {
    pub repo: RepoKey,
    pub selector: Selector,
    pub mode: RequestMode,
}
```

Proposed:

```rust
pub struct MaterializeRequest {
    pub repo: RepoKey,
    pub selector: Selector,
    pub mode: RequestMode,
    pub upstream_authorization: UpstreamAuthorizationMode,
}

pub enum UpstreamAuthorizationMode {
    Anonymous,
    Required,
}
```

The credential itself is not in JSON. It comes from headers.

### Selector

Extend exact commit selectors to support reachability proof:

```rust
pub enum Selector {
    Commit(CommitSha),
    CommitReachableFrom {
        commit: CommitSha,
        reachable_from: ReachabilitySelector,
    },
    ShortCommit(ShortCommitSha),
    Branch(BranchName),
    DefaultBranch,
}

pub enum ReachabilitySelector {
    Branch(BranchName),
    DefaultBranch,
}
```

Short commits in authenticated mode should resolve through GitHub-authorized
refs, not through cached objects alone.

### SessionManifest

Current session manifests include repo, commit, synthetic ref, and expiration.

Add:

```rust
pub struct SessionManifest {
    pub id: SessionId,
    pub repo: RepoKey,
    pub commit: CommitSha,
    pub synthetic_ref: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub protection: SessionProtection,
}

pub enum SessionProtection {
    Public,
    BearerToken {
        token_hash: String,
        authorized_commits: Vec<CommitSha>,
        authorized_refs: Vec<String>,
    },
}
```

Only a hash of the session token is stored.

### Manifests

Generation, commit, and ref manifests remain availability metadata. They must
not include credentials, caller identity, token hashes, or authorization claims.

Ref manifests should be treated carefully for authenticated requests: they say
that a ref was observed and fetched at a point in time, not that any later
caller may see that ref.

## Git Wrapper Changes

Add methods that take optional upstream auth:

```rust
pub async fn ls_remote_heads_with_auth(
    &self,
    remote: &str,
    auth: &UpstreamAuth,
) -> Result<LsRemoteResult>;

pub async fn fetch_refs_with_auth(
    &self,
    repo_dir: &Path,
    remote_url: &str,
    refspecs: &[String],
    auth: &UpstreamAuth,
) -> Result<GitOutput>;
```

Implementation requirements:

1. Preserve existing argument sanitization rules.
2. Keep auth out of argv.
3. Redact auth in tracing.
4. Pass auth only to commands that contact GitHub.
5. Do not apply request auth to local bundle operations, upload-pack serving,
   fsck, rev-parse, update-ref, or config commands.

## Error Mapping

Recommended API/Git endpoint mappings:

```text
Missing upstream auth when required         401
Invalid upstream auth header format         400
GitHub rejects credential                   401 or 403
GitHub verifies repo/ref absent             404
GitHub unavailable                          503
Want not authorized by current proof         403
Object authorized but unavailable to cache   503 or 502
Push attempt                                 405
Malformed Git request                        400
```

Do not include GitHub response bodies if they might contain credentials or
provider-specific sensitive text. Return sanitized messages.

## Logging, Metrics, And Redaction

Never log:

1. `Authorization`.
2. `Git-Cache-Upstream-Authorization`.
3. Decoded Basic credentials.
4. Session bearer tokens.
5. Raw pkt-line payloads.

Safe labels:

```text
repo host
request mode
selector kind
public/authenticated path
cache hit/miss
github auth accepted/rejected/unavailable
source value
```

Metrics to add:

```text
git_cache_upstream_auth_requests_total{entrypoint,result}
git_cache_private_cache_hits_total{entrypoint}
git_cache_private_cache_denied_total{reason}
git_cache_private_fetches_total{entrypoint,result}
git_cache_session_auth_failures_total{reason}
```

Do not put repo names, owners, tokens, or session ids in metric labels.

## Rollout Plan

### Phase 1: Plumbing And Redaction

1. Add `UpstreamAuth` extraction in the API layer.
2. Add a redacted representation and tests proving no token leaks through
   `Debug` or error display.
3. Add per-command Git auth injection.
4. Keep current public behavior unchanged.

### Phase 2: Authenticated `/resolve`

1. Add `upstream_authorization` to request parsing.
2. Implement authenticated branch/default resolution with GitHub `ls-remote`.
3. Report cache availability without hydrating/fetching.
4. Reject authenticated exact commits without reachability context.

### Phase 3: Authenticated `/materialize`

1. Authorize branch/default through GitHub.
2. Fetch missing authorized refs with the request auth.
3. Publish availability manifests only.
4. Create protected session tokens.
5. Require session bearer token on session Git endpoints.

### Phase 4: Authenticated `/git/...`

1. For authenticated `info/refs`, synthesize refs from GitHub `ls-remote`.
2. For authenticated `git-upload-pack`, re-authorize and validate wants.
3. Build ephemeral protected serving repos.
4. Disable arbitrary SHA wants.
5. Fetch missing authorized objects with the request auth.

### Phase 5: Hardening

1. Add race tests for force-push between advertisement and POST.
2. Add token redaction regression tests.
3. Add cache-leak tests across callers.
4. Add load tests for repeated `ls-remote` and authenticated cache hits.
5. Add operational metrics and alerts.

## Test Matrix

Functional tests:

1. Public anonymous clone still works.
2. PAT-authenticated private clone works.
3. OAuth-token-authenticated private clone works.
4. `/resolve` private branch returns a commit only with valid upstream auth.
5. `/materialize` private branch returns a protected session only with valid
   upstream auth.
6. Protected session fetch works with session token.
7. Protected session fetch fails without session token.
8. Protected session fetch fails with wrong session token.

Leak prevention tests:

1. Caller A populates private cache; anonymous caller cannot fetch it.
2. Caller A populates private cache; caller B with invalid token cannot fetch
   it.
3. Caller A populates private cache; caller B with valid access can fetch from
   cache after GitHub authorizes B.
4. Arbitrary cached SHA wants are rejected unless currently authorized.
5. Cached exact commit is not served when GitHub is unavailable for an
   authenticated request.
6. Force-pushed-away private commit is not served by branch materialization
   unless reachable from a newly authorized ref or explicitly fetchable by
   GitHub with the request credential.

Secret handling tests:

1. Object-store keys contain no token material.
2. Manifest JSON contains no token material.
3. Local repo config contains no upstream auth header.
4. Git argv logging contains no token material.
5. Error responses contain no token material.
6. Tracing spans and metrics contain no token material.

Git protocol tests:

1. Both `info/refs` and `git-upload-pack` require upstream auth for private
   authenticated flows.
2. `git-receive-pack` remains rejected before auth-dependent work.
3. Protected upload-pack does not allow arbitrary SHA wants.
4. Want parsing handles multiple wants, capabilities, flush packets, and
   malformed pkt-lines safely.

## Open Questions

1. Should authenticated `/git/...` require auth on every request even for public
   repos when auth is supplied? Initial answer: yes.
2. Should `/v1/materialize` exact commit require reachability context
   unconditionally for authenticated mode? Initial answer: yes, unless GitHub
   can fetch the exact SHA with the request credential.
3. Should we keep `upstream_auth_token_env` for trusted single-tenant
   deployments? Initial answer: yes, but mark it incompatible with multi-tenant
   private serving unless explicitly configured as trusted mode.
4. How long should protected sessions live? Initial answer: short, likely 5 to
   15 minutes, not the public default of 1 hour.
5. Should authenticated direct Git use a GET/POST nonce to avoid duplicate
   `ls-remote` calls? Initial answer: not in v1; correctness first.

## Initial Code Touchpoints

Likely files:

```text
crates/git-cache-core/src/lib.rs
crates/git-cache-core/src/selector.rs
crates/git-cache-core/src/session.rs
crates/git-cache-api/src/lib.rs
crates/git-cache-domain/src/materializer.rs
crates/git-cache-domain/src/state.rs
crates/git-cache-git/src/lib.rs
crates/git-cache-objectstore/src/manifests.rs
```

High-risk current behaviors to split for authenticated mode:

1. Exact commit cache hit without upstream auth.
2. Session URLs that rely only on session id possession.
3. `uploadpack.allowAnySHA1InWant = true`.
4. Direct `/git/...` serving from shared repos.
5. Process-wide `upstream_auth_token_env`.

## Final Principle

The cache can make GitHub faster and more reliable for object transfer, but for
private multi-tenant traffic it must never make GitHub optional for
authorization.

