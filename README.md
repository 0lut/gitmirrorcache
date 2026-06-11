# Git Fetch Cache

Read-only Git fetch cache for GitHub-style upstreams. The service keeps object
storage as the durable source of truth and treats local block storage as a
disposable hot cache. It exposes both a materialization API for cache manifests
and, when enabled, a Smart HTTP Git remote for read-through fetch and clone
traffic.

## What Works

- `POST /v1/materialize` for `commit`, `short_commit`, `branch`, and
  `default_branch` selectors.
- `POST /v1/resolve` verifies a selector to a canonical commit and reports
  whether cache state is already available.
- Known-complete exact commits are served from cache without contacting
  upstream. Exact commit materialization also hydrates known generation heads
  before falling back to upstream fetches.
- Branch, default-branch, and short-commit requests verify upstream refs with
  `git ls-remote` before serving.
- Optional read-through Git remote at `/git/{host}/{owner}/{repo}.git` for
  `info/refs` and `git-upload-pack`, including shallow and blobless fetches.
- Direct Git cold misses proxy upstream upload-pack responses immediately by
  default, then warm the cache in the background; clients can opt out per
  request with the `git-cache-use-proxy-on-miss` header.
- `git-receive-pack` is rejected and never advertised.
- Local object-store adapter and feature-gated S3-compatible adapter.
- Disk reservations, LRU eviction, repo locks, protected repos, stale temp
  cleanup, and `git-cache disk-status`.
- Per-repo leases, in-flight request dedupe, generation publishing/hydration,
  read-through updates, and hourly compaction support.
- Metrics at `/metrics`, a simple global materialize rate limit, bounded Git
  output/streams, and a semaphore for Git subprocess concurrency.
- Request-scoped upstream authorization for protected repos, plus optional
  deployment-wide upstream token injection through Git config environment
  variables without putting secrets in argv or manifests.
- ECS-on-Graviton EC2/EBS deployment scripts, Amazon Linux 2023 host defaults,
  shared-ALB preview stacks, smoke tests, diagnostics, and stale-container
  recovery tooling.

## Run Locally

```sh
cargo test --workspace
GIT_CACHE_CONFIG=config/local.example.toml cargo run -p git-cache-api
curl -s http://127.0.0.1:8080/healthz
```

The local config enables the direct Git remote and expects fake upstream bare
repositories under `./tmp/upstreams/{host}/{owner}/{repo}.git`. See
[docs/local-dev.md](docs/local-dev.md).

## API Examples

```sh
curl -s http://127.0.0.1:8080/v1/materialize \
  -H 'content-type: application/json' \
  -d '{"repo":"github.com/org/repo","selector":{"branch":"main"}}'
```

The materialize response contains `repo`, the verified canonical `commit`,
`source`, and `verified_at`. To check a selector without forcing
materialization, use `/v1/resolve`:

```sh
curl -s http://127.0.0.1:8080/v1/resolve \
  -H 'content-type: application/json' \
  -d '{"repo":"github.com/org/repo","selector":{"default_branch":true}}'
```

When the read-through Git remote is enabled, fetch or clone through `/git/...`:

```sh
git fetch http://127.0.0.1:8080/git/github.com/org/repo.git refs/heads/main
git clone http://127.0.0.1:8080/git/github.com/org/repo.git
git clone --depth 1 http://127.0.0.1:8080/git/github.com/org/repo.git
```

Full commit IDs belong in the `commit` selector. Abbreviated hashes are accepted
through `short_commit`; they are resolved with Git first and all manifests still
store only the canonical full object ID.

Request bodies are strict: send only supported fields such as `repo`,
`selector`, and `upstream_authorization`. The legacy `mode` field is no longer
accepted.

For a protected materialize or resolve request, require request-scoped upstream
authorization and pass Basic credentials in the cache-specific header:

```sh
curl -s http://127.0.0.1:8080/v1/materialize \
  -H 'content-type: application/json' \
  -H 'git-cache-upstream-authorization: Basic <base64-user-colon-token>' \
  -d '{"repo":"github.com/org/private","selector":{"branch":"main"},"upstream_authorization":"required"}'
```

For direct Git, use the normal Git HTTP authorization header:

```sh
git -c http.extraHeader='Authorization: Basic <base64-user-colon-token>' \
  fetch http://127.0.0.1:8080/git/github.com/org/private.git refs/heads/main
```

On a cold direct-Git miss, the server proxies upload-pack to the upstream by
default (then warms the cache in the background) so client latency stays close
to a direct clone. Set `GIT_CACHE_GIT_REMOTE_PROXY_ON_MISS_BY_DEFAULT=false`
(config: `git_remote.proxy_on_miss_by_default`) to disable, or override per
request with the `git-cache-use-proxy-on-miss` header:

```sh
git -c http.extraHeader='git-cache-use-proxy-on-miss: false' \
  clone http://127.0.0.1:8080/git/github.com/org/repo.git
```

## CLI

```sh
cargo run -p git-cache-cli -- config
cargo run -p git-cache-cli -- disk-status
cargo run -p git-cache-cli -- warm github.com/org/repo main
cargo run -p git-cache-cli -- optimize github.com/org/repo
cargo run -p git-cache-cli -- compact --all --dry-run
cargo run -p git-cache-cli -- compact --repo github.com/org/repo
```

## Integration Tests

There is an opt-in Python integration test against `github.com/astral-sh/uv`.
It uses only Python's standard library and shells out to `cargo` and `git`:

```sh
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_astral_uv
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_git_remote_public
```

See [integration_tests/README.md](integration_tests/README.md) for MinIO/S3
variants and larger direct-Git clone coverage.

## S3 Adapter

The object-store crate includes a feature-gated S3-compatible adapter:

```sh
cargo test -p git-cache-objectstore --features s3
```

Runtime S3 wiring is enabled by building `git-cache-api` with the `s3` feature
and setting `GIT_CACHE_OBJECT_STORE_KIND=s3`, `GIT_CACHE_S3_BUCKET`, and
`GIT_CACHE_S3_PREFIX`. Runtime object-store namespaces are suffixed with `-v2`,
so `GIT_CACHE_S3_PREFIX=repos` stores cache objects under `repos-v2`.

## Deployment

The maintained AWS deployment path is ECS on Graviton EC2 with host-mounted EBS
for the local hot cache and S3 for durable cache objects. The checked-in deploy
wrapper builds, deploys, and smoke-tests the stack:

```sh
AWS_REGION=us-west-2 \
ENVIRONMENT=dev-arm \
NAME_PREFIX=gitmirrorcache-arm \
scripts/aws/deploy-and-smoke.sh
```

The deployment scripts default to an Amazon Linux 2023 ECS-optimized ARM64 host
AMI, register hourly compaction as a one-off ECS task, and include diagnostics
for stuck rollouts. Script-driven preview stacks can deploy any branch, tag, or
commit behind a shared preview ALB:

```sh
AWS_REGION=us-west-2 scripts/aws/deploy-preview.sh HEAD
scripts/aws/destroy-preview.sh HEAD
```

See [docs/deployment.md](docs/deployment.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
