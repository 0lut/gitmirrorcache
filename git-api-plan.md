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
clone/fetch the same repo, branch, or commit while GitHub avoids serving full
packfiles except when the cache is missing data or a branch has advanced.

For branch operations, the service should still guarantee the latest upstream
commit. It can do that with a lightweight upstream ref comparison, such as
`git ls-remote`, before deciding whether a real fetch is necessary.

## Non-goals

- Push support.
- `git-receive-pack`.
- A separate pinned URL model for normal agent operation.
- TTL-based branch freshness.
- Pulling repository objects from GitHub just to discover whether a branch has
  changed.

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
4. Ask GitHub for the current advertised refs without pulling repository
   objects, for example with `git ls-remote --heads --symref`.
5. Compare upstream branch heads and default branch metadata with the cache's
   current public refs/manifests.
6. If all relevant upstream heads match local cache state, advertise from local
   cache immediately.
7. If any advertised branch is missing or has advanced, acquire a repo/ref lease.
8. One request fetches only the changed/missing refs from GitHub, publishes
   manifests/bundles, and updates public refs.
9. Concurrent requests wait for the same comparison/fetch result instead of
   independently pulling GitHub.
10. Run `git upload-pack --stateless-rpc --advertise-refs .` against the local
   cache repo.

Because standard Smart HTTP ref advertisement does not reliably tell the server
which branch the client will choose, the safe default is to compare the upstream
ref advertisement for the served repo before advertising local refs. This keeps
branch results latest without pulling pack data unless a branch actually moved.

### Object transfer

For pack transfer:

```text
POST /git/github.com/org/repo.git/git-upload-pack
```

The server should:

1. Parse `want <oid>` lines from the upload-pack request body.
2. For each wanted object:
   - serve immediately if available locally or known in object storage;
   - hydrate from object-store generation manifests if known;
   - on unknown full commit SHA, acquire a lease and perform one upstream
     read-through fetch/verification, then populate the cache;
   - fail if the object is unknown and upstream cannot verify it.
3. Run `git upload-pack --stateless-rpc .` against the local cache repo.

This makes `git fetch origin <full-commit-sha>` work without a prior JSON call.

## GitHub offload policy

Branch and commit selectors have different correctness rules.

Branches are mutable, so direct branch clone/fetch must compare against GitHub
before advertising refs. That comparison should be a lightweight ref lookup, not
a repository pull. If the upstream head matches the cached head, no GitHub
objects are fetched.

Commit IDs are immutable. A full commit SHA can use the cache as long as the
commit is present locally or in object storage. Only missing commits require an
upstream read-through fetch/verification, and successful read-through must
populate the cache for future requests.

Recommended default policy:

```toml
[git_remote]
enabled = true
branch_ref_check = "always"
branch_update_mode = "compare_then_fetch"
commit_read_through = true
concurrent_upstream_work = "wait"
```

Semantics:

- Branch refs are compared against GitHub before advertisement.
- If upstream branch heads match cached refs, serve from cache without fetching.
- If upstream branch heads differ, fetch only changed/missing refs and publish
  the new cache state.
- Cached commit objects are served without contacting GitHub.
- Missing commit objects are fetched/verified once behind a lease and then
  cached.
- Concurrent identical misses wait for the in-flight result.
- If GitHub is unavailable:
  - branch operations fail closed unless a future explicit stale-serving mode is
    added, because latest branch state cannot be verified;
  - known cached commits continue to serve;
  - unknown commits fail.

This accepts the core tradeoff:

```text
branches always latest -> lightweight upstream ref comparison before advertise
GitHub offload         -> avoid GitHub pack/object transfer unless cache misses
```

For high-volume agent workloads, this still unloads the expensive part of GitHub
traffic: repeated pack generation and object transfer. Single-flight behavior
should coalesce bursty concurrent comparisons/fetches so a thundering herd of
agents does not turn into a thundering herd of upstream Git operations.

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

After verified upstream comparison/fetch, update public refs in the served bare
repo:

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

Exact commit behavior:

1. If the commit exists in the local bare repo, serve it.
2. If the commit is known in object-storage manifests, hydrate it locally and
   serve it.
3. If the commit is unknown, perform an upstream read-through fetch/verification
   behind a lease.
4. If upstream verifies and provides the commit, publish manifests/bundles and
   serve it.
5. If upstream cannot verify the commit, fail.

If broader exposure is a concern, use the stricter
`uploadpack.allowReachableSHA1InWant=true` and ensure materialized commits are
reachable from advertised public refs. The tradeoff is that force-pushed or
detached cached commits may not be fetchable by raw SHA.

## Concurrency behavior

Git clients do not understand cache-specific `503 update in progress` semantics
well. For Git endpoints, prefer waiting behind single-flight upstream work:

```text
agent 1    -> branch ref comparison/fetch -> acquires lease
agents N   -> same repo/ref work          -> wait/reuse result
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

1. Add `git_remote` config for enabling the direct remote, branch ref comparison
   policy, commit read-through, and SHA want policy.
2. Add `/git/{*repo_path}` in `git-cache-api`.
3. Extract shared Smart HTTP upload-pack handling from the existing session
   handler.
4. Add a direct `git_repo` handler that:
   - rejects receive-pack;
   - parses repo path;
   - compares upstream branch refs before `info/refs` advertisement;
   - fetches changed/missing branch refs only when the comparison shows a move;
   - inspects wanted SHAs for `git-upload-pack`;
   - serves from the local bare cache repo.
5. Add domain methods:
   - `ensure_repo_advertisable(repo)`;
   - `compare_upstream_refs(repo)`;
   - `fetch_changed_refs(repo, comparison)`;
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
- Repeated branch clones compare upstream refs but do not fetch objects when the
  branch head is unchanged.
- Concurrent branch clones coalesce upstream comparison/fetch work.
- Branch clone fetches and publishes a new generation when GitHub advertises a
  newer branch head.
- `git fetch origin <full-commit-sha>` works for cached commits.
- Unknown commit fetch performs one read-through verification and populates the
  cache.
- Cached commit fetch works while upstream is unavailable.
- Branch clone fails closed when upstream is unavailable and latest branch state
  cannot be verified.
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

Materialization happens automatically inside clone/fetch handling. Branch
operations always compare against GitHub's advertised refs and fetch only when
the cached branch is behind. Commit operations use cached data whenever present
and perform read-through cache population only for missing commits.
