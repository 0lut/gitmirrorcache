# Git Fetch Cache

Read-only Git fetch cache for GitHub-style upstreams. The service keeps object storage as the durable source of truth and treats local block storage as a disposable hot cache.

## What Works

- `POST /v1/materialize` for exact commits, branch selectors, and default branch selectors.
- Known-complete exact commits are served from cache without contacting upstream.
- Branch and default-branch requests verify upstream with `git ls-remote` before serving.
- Optional read-through Git remote at `/git/{host}/{owner}/{repo}.git`.
- `git-receive-pack` is rejected and never advertised.
- Local object-store adapter and feature-gated S3-compatible adapter.
- Disk reservations, LRU eviction, repo locks, protected repos, stale temp cleanup, and `git-cache disk-status`.
- Cron/read-through/event update orchestration with per-repo leases and in-flight request dedupe.
- Metrics at `/metrics` and a simple global materialize rate limit.
- Optional upstream bearer token injection through Git config environment variables, without putting secrets in argv or manifests.

## Run Locally

```sh
cargo test --workspace
GIT_CACHE_CONFIG=config/local.example.toml cargo run -p git-cache-api
curl -s http://127.0.0.1:8080/healthz
```

The local config expects fake upstream bare repositories under `./tmp/upstreams/{host}/{owner}/{repo}.git`. See [docs/local-dev.md](docs/local-dev.md).

## API Example

```sh
curl -s http://127.0.0.1:8080/v1/materialize \
  -H 'content-type: application/json' \
  -d '{"repo":"github.com/org/repo","selector":{"branch":"main"}}'
```

The response contains the verified commit, source, and timestamp. When the
read-through Git remote is enabled, fetch or clone through `/git/...`:

```sh
git fetch http://127.0.0.1:8080/git/github.com/org/repo.git refs/heads/main
```

Full commit IDs belong in the `commit` selector. Abbreviated hashes are accepted
through `short_commit`; they are resolved with Git first and all manifests still
store only the canonical full object ID.

Request bodies are strict: send only supported fields such as `repo`,
`selector`, and `upstream_authorization`. The legacy `mode` field is no longer
accepted.

## CLI

```sh
cargo run -p git-cache-cli -- config
cargo run -p git-cache-cli -- disk-status
```

## GitHub Integration Test

There is an opt-in Python integration test against `github.com/astral-sh/uv`.
It uses only Python's standard library and shells out to `cargo` and `git`:

```sh
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_astral_uv
```

See [integration_tests/README.md](integration_tests/README.md).

## S3 Adapter

The object-store crate includes a feature-gated S3-compatible adapter:

```sh
cargo test -p git-cache-objectstore --features s3
```

Runtime S3 wiring is enabled by building `git-cache-api` with the `s3` feature and setting `GIT_CACHE_OBJECT_STORE_KIND=s3`, `GIT_CACHE_S3_BUCKET`, and `GIT_CACHE_S3_PREFIX`.

## Deployment

The maintained AWS deployment path is ECS on Graviton EC2 with host-mounted EBS
for the local hot cache and S3 for durable cache objects. See
[docs/deployment.md](docs/deployment.md).
