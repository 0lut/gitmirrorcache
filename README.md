# Git Fetch Cache

Read-only Git fetch cache for GitHub-style upstreams. The service keeps object storage as the durable source of truth and treats local block storage as a disposable hot cache.

## What Works

- `POST /v1/materialize` for exact commits, strict branches, and strict default branch requests.
- Known-complete exact commits are served from cache without contacting upstream.
- Strict branch/default requests verify upstream with `git ls-remote` before serving.
- Session URLs expose pinned synthetic refs through `git-upload-pack`.
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
  -d '{"repo":"github.com/org/repo","selector":{"branch":"main"},"mode":"strict"}'
```

The response contains a short-lived Git URL and a synthetic ref:

```sh
git fetch "$git_url" "$ref"
```

Full commit IDs belong in the `commit` selector. Abbreviated hashes are accepted
through `short_commit`; they are resolved with Git first and all manifests still
store only the canonical full object ID.

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
