# README Fact-Check Audit

Audit date: 2026-06-11
Audited ref: `origin/main` at `981db08a1516`
README commit: `981db08` (`docs: refresh README with env var reference and deployment options`)

This report captures a fact check of `README.md` as of the audited ref. It is
intentionally an audit artifact only; it does not rewrite the README. The goal
is to make the README fixes easy for a follow-up maintainer to apply.

## Summary

Most operational details in the README match the current code and tests:

- the materialization API routes and strict request body shape are accurate
- the direct Git remote route exists and rejects receive-pack
- shallow and blobless branch clone/fetch flows are covered by tests
- the listed config defaults mostly match `AppConfig::from_env`
- the Helm chart, Dockerfile, CLI commands, and test commands exist

The main issues are product-scope overclaims. In particular, the README says the
service works with "any Smart HTTP Git server" and says clients can use it "like
any other HTTP remote." The current implementation is narrower: it supports a
read-only upload-pack path for allowlisted `host/owner/repo` repos where the
upstream URL can be derived as `https://{host}/{owner}/{repo}.git`, plus local
filesystem fixture upstreams via `upstream_root`.

## Recommended Priority

1. Narrow the "any Smart HTTP Git server" wording.
2. Qualify "everything after that is served locally."
3. Qualify "like any other HTTP remote" to the supported read-only branch clone
   and fetch surface.
4. Clarify the object store and per-request auth wording.

## Findings

### P1: "Any Smart HTTP Git server" is overbroad

README lines 3-6 describe support for "GitHub, GitLab, Bitbucket, or any Smart
HTTP Git server." That is broader than the implementation.

Evidence:

- `RepoKey::parse` requires exactly three path segments: `host/owner/repo`.
  It rejects more than three segments, path prefixes, empty segments, slashes
  inside owner/repo beyond that shape, and non-ASCII/special path characters.
  See `crates/git-cache-core/src/repo.rs`.
- `RepoKey::local_bare_path` and URL construction assume `host/owner/repo`.
  See `crates/git-cache-core/src/repo.rs` and
  `crates/git-cache-domain/src/materializer/repo.rs`.
- `Materializer::upstream_url` constructs only
  `https://{host}/{owner}/{repo}.git` when `upstream_root` is not set.
  It cannot represent HTTP-only upstreams, custom ports, path prefixes,
  GitLab subgroup paths, query-string gateways, or arbitrary Smart HTTP repo
  locations.
- The HTTP route is `/git/{*repo_path}`, but `repo_from_git_path` normalizes
  that route back into a `RepoKey`, so the catch-all route does not imply
  arbitrary upstream path support.

External protocol reference:

- Git's HTTP protocol describes repository URLs as standard HTTP URLs with a
  path component, and Smart HTTP appends `info/refs` and `git-upload-pack` under
  that `$GIT_URL`: https://www.kernel.org/pub/software/scm/git/docs/gitprotocol-http.html

Suggested README wording:

```md
A read-only Git fetch cache for allowlisted HTTP(S) upstreams that can be
addressed as `host/owner/repo` repositories, such as GitHub-style remotes.
```

Or, if generic Smart HTTP support is desired, fix the implementation first by
adding a validated upstream URL/repo-path model that can represent arbitrary
Smart HTTP repository paths without weakening the git-argument safety boundary.

### P1: "Everything after that is served locally" is too absolute

README lines 20-22 say a cold miss proxies upstream while warming the cache, and
"Everything after that is served locally." That needs qualification.

Evidence:

- Direct Git `GET /info/refs?service=git-upload-pack` calls
  `materializer.upstream_refs(&repo)` and uses `ls_remote_heads` to prove repo
  access and synthesize the advertisement. That contacts upstream even when the
  local object cache is hot.
- Branch and default-branch materialization verify upstream refs by design.
- POST upload-pack can serve locally when the local cache is ready, but the
  code first checks readiness and can proxy or fall back to local read-through
  behavior depending on config and request shape.

Suggested README wording:

```md
After the initial object cache is warm, upload-pack responses for cached wants
can be served from the local bare repo. Ref advertisement and branch/default
branch materialization still verify upstream refs.
```

### P2: "Like any other HTTP remote" should be narrowed

README lines 36-44 say to clone or fetch through the Git remote like any other
HTTP remote. Normal read-only branch clone/fetch flows work, including shallow
and blobless flows, but the surface is not a full replacement for every Git HTTP
remote behavior.

Evidence:

- Receive-pack is rejected by design.
- `upstream_refs` uses `ls_remote_heads`, so the synthesized advertisement is
  heads-focused. Tags are not guaranteed. The `ls_remote_tags_behavior` test in
  `crates/git-cache-api/tests/git_client_advanced.rs` explicitly treats tags as
  optional: `ls-remote --tags` must not error, but it may return no tags.
- Mirror clone behavior is treated as possibly unsupported in tests.
- The direct remote advertises and handles the upload-pack flow, not the full
  Git Smart HTTP service matrix.

Suggested README wording:

```md
Clone or fetch branch refs through the read-only Git remote using standard Git
clients:
```

Add a short limitations note:

```md
The direct Git remote is read-only and optimized for branch clone/fetch flows.
Tag advertisement and mirror-clone semantics are not guaranteed.
```

### P2: Object store wording is production-biased

README lines 7-8 say "an S3-compatible object store is the durable source of
truth." That is true for production-style deployments, but not for the default
environment-backed configuration or local development.

Evidence:

- `GIT_CACHE_OBJECT_STORE_KIND` defaults to `local`.
- S3 requires building with the `s3` feature.
- The Dockerfile and Helm chart default to S3, but the local example config and
  env defaults use a local filesystem object store.

Suggested README wording:

```md
The configured object store, local filesystem for development or S3-compatible
storage for deployments, is the durable source of truth.
```

### P3: Per-request direct Git auth is Basic-only

README lines 62-74 say direct Git uses the normal HTTP authorization header.
That is directionally correct, but the implementation only accepts Basic for
request-scoped upstream auth.

Evidence:

- `UpstreamAuth::parse_header` rejects non-Basic schemes.
- `GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV` can inject a deployment-wide Bearer token
  via Git config env, but that is separate from per-request direct Git auth.

Suggested README wording:

```md
For private repositories, pass request-scoped Basic credentials. The
materialize API uses `git-cache-upstream-authorization`; direct Git reads the
HTTP `Authorization` header.
```

### P3: Schema suffix comments drift between files

This is not a README issue, but it may confuse maintainers while updating the
README.

Evidence:

- Runtime suffix is currently `v3`, and README says `repos` stores under
  `repos-v3`.
- `config/local.example.toml`, `config/minio.example.toml`,
  `config/production.example.toml`, and `deploy/helm/gitmirrorcache/values.yaml`
  still contain comments saying "v2" in places.

Suggested follow-up:

- Update example config comments and Helm values comments from `v2` to `v3`, or
  reword them to say "current schema suffix" without naming the version.

## Claims Verified As Accurate

The following README claims were checked against code and/or tests and appear
accurate as of `981db08a1516`.

- Routes: `/healthz`, `/metrics`, `/v1/materialize`, `/v1/resolve`, and
  `/git/{*repo_path}` exist.
- `GIT_CACHE_GIT_REMOTE_ENABLED=true` enables the direct Git route.
- Direct Git receive-pack is rejected at the API layer.
- Direct Git uses `Authorization` for request-scoped upstream auth; the
  materialization API uses `git-cache-upstream-authorization`.
- Request bodies for `MaterializeRequest` deny unknown top-level fields and
  accept only `repo`, `selector`, and optional `upstream_authorization`.
- Selectors are `commit`, `short_commit`, `branch`, and `default_branch`.
- `git-cache-use-proxy-on-miss: false` disables proxy-on-miss for that request.
- Defaults in the README's config tables generally match `AppConfig::from_env`,
  including bind address, cache root, object-store kind/root, allowed hosts,
  rate limit, git timeout, output byte limit, git remote defaults, disk defaults,
  compaction defaults, shutdown defaults, and concurrency defaults.
- The Helm chart exists at `deploy/helm/gitmirrorcache`, renders locally, uses a
  StatefulSet, and defines an hourly compaction CronJob.
- The Dockerfile is multi-stage and builds both `git-cache-api` and `git-cache`.
- CLI commands shown in the README exist.
- The opt-in integration test modules named in the README exist and are gated
  by `RUN_GITHUB_INTEGRATION=1`.

## Verification Performed

Commands run from the repository root:

```sh
git fetch origin main --prune
git switch --detach origin/main
cargo test --workspace
cargo test -p git-cache-objectstore --features s3
GIT_CACHE_CONFIG=config/local.example.toml cargo run -p git-cache-api
curl -s http://127.0.0.1:8080/healthz
helm template git-cache deploy/helm/gitmirrorcache \
  --set config.objectStore.s3.bucket=my-git-cache-bucket \
  --set aws.region=us-west-2
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_astral_uv
```

Results:

- `cargo test --workspace`: passed.
- `cargo test -p git-cache-objectstore --features s3`: passed.
- Local API startup with `config/local.example.toml`: `/healthz` returned
  `200` with `{"ok":true}`.
- Helm template command rendered successfully.
- Live GitHub `integration_tests.test_astral_uv`: passed against
  `github.com/astral-sh/uv` on branch `main`.

The live integration run observed `astral-sh/uv` `main` at commit
`175e78a616fda66af02d026eed07667ffde7c799` at run time.

Not run:

- `RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_git_remote_public`
  was not run because it shallow-clones several large public repositories
  (`torvalds/linux`, `llvm/llvm-project`, `gcc-mirror/gcc`, and `astral-sh/uv`).
  The test exists and matches the README's opt-in command, but this audit did
  not spend that network/disk budget.

## Suggested Fix Shape

For a README-only follow-up PR, make these scoped edits:

1. Replace "any Smart HTTP Git server" with a precise statement about
   allowlisted GitHub-style `host/owner/repo` HTTP(S) upstreams.
2. Replace "Everything after that is served locally" with a note that cached
   upload-pack wants can be served locally, while ref advertisement and branch
   selectors still verify upstream refs.
3. Change "like any other HTTP remote" to "using standard Git clients for
   read-only branch clone/fetch flows."
4. Add a "Current limitations" note covering read-only behavior, Basic-only
   per-request auth, `host/owner/repo` repository shape, and best-effort tags.
5. Reword object-store language to include local filesystem development mode.
6. Sweep schema suffix comments so they do not say `v2` when runtime is `v3`.

For a code follow-up PR that makes the original README wording true, the main
work item is a broader, validated upstream repository model. It would need to
represent arbitrary Smart HTTP repository URLs safely while preserving the
repo's git-argument validation rules and cache-path safety boundaries.
