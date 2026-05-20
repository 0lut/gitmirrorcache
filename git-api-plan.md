# Git API Plan: Read-Through Smart HTTP Remote

## Goal

Expose this service as a normal read-only Git remote so agents can run standard
Git commands against the cache server instead of GitHub:

```sh
git clone --no-tags --branch main http://cache.example/git/github.com/org/repo.git work
git -C work fetch --no-tags origin main
git -C work fetch --no-tags origin <full-commit-sha>
```

Materialization should be internal to fetch/clone operations. Callers should not
need to call `/v1/materialize` before using Git.

The primary purpose is GitHub offload: many one-off agents should be able to
clone/fetch the same repo, branch, or commit while GitHub sees at most one
refresh/cache-miss request per freshness window or missing object.

## Non-goals

- Push support.
- `git-receive-pack`.
- Per-agent GitHub verification.
- A separate pinned URL model for normal agent operation.
- Exact "always latest" branch semantics on every clone if that would require a
  GitHub request for every agent.

## User-facing model

The stable remote URL is:

```text
/git/{host}/{owner}/{repo}.git
```

Examples:

```sh
git clone --no-tags --branch main \
  http://cache.example/git/github.com/org/repo.git repo

git init repo
git -C repo remote add origin \
  http://cache.example/git/github.com/org/repo.git
git -C repo fetch --no-tags origin <full-commit-sha>
git -C repo checkout --detach <full-commit-sha>
```

`/v1/materialize` can remain as an admin/warmup API, but it should not be
required for agents.

## Smart HTTP endpoints

Add a direct Git route alongside the existing session route:

```rust
.route("/git/{*repo_path}", any(git_repo))
```

Supported:

```text
GET  /git/github.com/org/repo.git/info/refs?service=git-upload-pack
POST /git/github.com/org/repo.git/git-upload-pack
```

Rejected:

```text
GET  /git/github.com/org/repo.git/info/refs?service=git-receive-pack
POST /git/github.com/org/repo.git/git-receive-pack
```

The existing session endpoint can stay for compatibility:

```text
/git/session/{session_id}/{host}/{owner}/{repo}.git
```

## Request lifecycle

### Clone or branch fetch

For `git clone --branch main` or `git fetch origin main`, the client first asks
for advertised refs:

```text
GET /git/github.com/org/repo.git/info/refs?service=git-upload-pack
```

The server should:

1. Parse and validate `RepoKey` from the URL.
2. Reject `git-receive-pack`.
3. Ensure the repo has a local bare cache directory.
4. Check branch/default-ref freshness in manifests.
5. If cached refs are fresh enough, advertise from local cache only.
6. If refs are missing/stale, acquire a repo/ref lease.
7. One request refreshes from GitHub, publishes manifests/bundles, and updates
   public refs.
8. Concurrent requests wait for the same refresh result instead of independently
   contacting GitHub.
9. Run `git upload-pack --stateless-rpc --advertise-refs .` against the local
   cache repo.

### Object transfer

For pack transfer:

```text
POST /git/github.com/org/repo.git/git-upload-pack
```

The server should:

1. Parse `want <oid>` lines from the upload-pack request body.
2. For each wanted object:
   - serve immediately if available locally;
   - hydrate from object-store generation manifests if known;
   - on unknown full commit SHA, acquire a lease and perform one upstream
     read-through fetch/verification;
   - fail if the object is unknown and upstream cannot verify it.
3. Run `git upload-pack --stateless-rpc .` against the local cache repo.

This makes `git fetch origin <full-commit-sha>` work without a prior JSON call.

## GitHub offload policy

The service must avoid turning every agent clone into a GitHub `ls-remote` or
`fetch`.

Recommended default policy:

```toml
[git_remote]
enabled = true
branch_refresh_ttl_seconds = 300
default_branch_refresh_ttl_seconds = 300
commit_read_through = true
concurrent_refresh = "wait"
```

Semantics:

- Branch refs are refreshed at most once per TTL per repo/ref.
- Cached refs are advertised without contacting GitHub.
- Missing commit objects are fetched/verified once behind a lease.
- Concurrent identical misses wait for the in-flight result.
- If GitHub is unavailable:
  - fresh cached branch refs continue to serve;
  - stale branch refs can either serve stale or fail, depending on config;
  - known cached commits continue to serve;
  - unknown commits fail.

This accepts the core tradeoff:

```text
always-latest branch on every clone -> GitHub request per clone
GitHub offload                     -> bounded freshness window
```

For high-volume agent workloads, bounded freshness is the desired behavior.

## Ref model

The direct remote should advertise normal Git refs:

```text
HEAD
refs/heads/main
refs/heads/feature-x
```

The cache can continue using internal refs:

```text
refs/cache/upstream/heads/main
refs/cache/upstream/heads/feature-x
```

After a verified refresh, update public refs in the served bare repo:

```text
refs/heads/main -> verified commit
HEAD            -> default branch
```

Hide internal refs from clients:

```text
uploadpack.hideRefs=refs/cache
transfer.hideRefs=refs/cache
```

Start with the existing bare cache repo as the served repo. If ref isolation
becomes necessary, introduce a separate served bare repo that uses
`objects/info/alternates` to point at the internal object cache.

## Exact commit fetches

Git clients do not have a plain `git clone <remote> <commit>` form. For exact
commits, agents should use:

```sh
git init repo
git -C repo remote add origin http://cache.example/git/github.com/org/repo.git
git -C repo fetch --no-tags --depth=1 origin <full-commit-sha>
git -C repo checkout --detach <full-commit-sha>
```

To support this reliably, configure served repos for SHA wants:

```text
uploadpack.allowAnySHA1InWant=true
```

This should only be enabled for validated, allowlisted repos. The server still
controls hydration/read-through before invoking `upload-pack`, so unknown SHAs
do not cause unbounded upstream work.

If broader exposure is a concern, use the stricter
`uploadpack.allowReachableSHA1InWant=true` and ensure materialized commits are
reachable from advertised public refs. The tradeoff is that force-pushed or
detached cached commits may not be fetchable by raw SHA.

## Concurrency behavior

Git clients do not understand cache-specific `503 update in progress` semantics
well. For Git endpoints, prefer waiting behind a single-flight refresh:

```text
agent 1    -> cache miss -> acquires lease -> refreshes GitHub
agents N   -> same miss  -> wait/reuse result
all agents -> receive pack from cache
```

The JSON API can keep returning explicit busy responses if desired, but the Git
remote should optimize for transparent clone/fetch success.

## Streaming requirement

The current upload-pack wrapper buffers stdout into memory. That is not
appropriate for real clones because packfiles can be large.

The direct Git remote should stream:

```text
HTTP request body -> git upload-pack stdin
git stdout        -> HTTP response body
git stderr        -> bounded logs/metrics
```

Keep timeouts, process cleanup, and bounded stderr. Avoid a global
`max_git_output_bytes` cap for packfile stdout on clone/fetch responses.

## Implementation steps

1. Add `git_remote` config for enabling the direct remote, ref freshness TTL,
   stale behavior, and SHA want policy.
2. Add `/git/{*repo_path}` in `git-cache-api`.
3. Extract shared Smart HTTP upload-pack handling from the existing session
   handler.
4. Add a direct `git_repo` handler that:
   - rejects receive-pack;
   - parses repo path;
   - calls read-through materialization for `info/refs`;
   - inspects wanted SHAs for `git-upload-pack`;
   - serves from the local bare cache repo.
5. Add domain methods:
   - `ensure_repo_advertisable(repo)`;
   - `ensure_refs_fresh(repo, policy)`;
   - `ensure_wants_available(repo, wants)`;
   - `sync_public_refs(repo)`.
6. Configure served repos with:
   - public `refs/heads/*`;
   - correct `HEAD`;
   - hidden `refs/cache/*`;
   - SHA-want policy.
7. Change upload-pack serving to stream pack output.
8. Add integration tests that run real `git clone` and `git fetch <sha>`
   against the Axum server.

## Test plan

Add tests for:

- `git clone --branch main http://server/git/github.com/org/repo.git`.
- Repeated clones reuse cache and do not trigger repeated upstream fetches.
- Concurrent clones produce one upstream refresh.
- `git fetch origin <full-commit-sha>` works for cached commits.
- Unknown commit fetch performs one read-through verification.
- Cached commit fetch works while upstream is unavailable.
- Branch clone behavior when refs are stale and upstream is unavailable,
  covering both serve-stale and fail-closed policies.
- `git-receive-pack` is never advertised and push attempts are rejected.
- Internal `refs/cache/*` are not advertised.
- Large pack responses stream instead of buffering into memory.

## Result

The service behaves like a normal read-only Git remote:

```sh
git clone http://cache.example/git/github.com/org/repo.git
git fetch origin main
git fetch origin <full-commit-sha>
```

Materialization happens automatically inside clone/fetch handling. GitHub is
used only for cache misses, stale ref refreshes, and unknown commit
verification, with leases/single-flight behavior to prevent thundering herds.
