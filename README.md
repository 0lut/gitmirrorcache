# gitmirrorcache

[![Latest release](https://img.shields.io/github/v/tag/0lut/gitmirrorcache?sort=semver&label=release)](https://github.com/0lut/gitmirrorcache/tags)
[![Container image](https://img.shields.io/badge/ghcr.io-0lut%2Fgitmirrorcache-blue?logo=docker)](https://github.com/0lut/gitmirrorcache/pkgs/container/gitmirrorcache)

A read-only Git fetch cache that sits between clone-heavy automation — CI
runners, coding agents, sandboxes, build farms — and allowlisted HTTPS
upstreams addressable as `host/owner/repo` (GitHub-style remotes, including
top-level GitLab and Bitbucket repos). Instead of hammering the upstream with
thousands of identical clones, clients fetch from the cache: an S3-compatible object store is the durable source of truth, and
local disk is just a disposable hot layer that can be rebuilt at any time. The
result is faster clones, fewer upstream rate-limit headaches, and a cache you
can throw away without losing anything.

It exposes two interfaces:

- A **materialization API** (`/v1/materialize`, `/v1/resolve`) for warming and
  inspecting cache state by commit, branch, or default branch.
- A **read-through Git remote** at `/git/{host}/{owner}/{repo}.git` that
  standard Git clients can clone and fetch from over Smart HTTP, including
  shallow and blobless fetches. Pushes (`git-receive-pack`) are rejected.

On a cold miss, the server proxies upstream's response straight to the client
(so first-clone latency stays close to a direct clone) while warming the cache
in the background. Once the cache is warm, pack data is served from the local
bare repo; ref advertisements and branch/default-branch selectors still verify
refs against upstream so clients never see stale tips.

## Quick Start

```sh
cargo test --workspace
GIT_CACHE_CONFIG=config/local.example.toml cargo run -p git-cache-api
curl -s http://127.0.0.1:8080/healthz
```

The local config enables the direct Git remote and expects fake upstream bare
repositories under `./tmp/upstreams/{host}/{owner}/{repo}.git`. See
[docs/local-dev.md](docs/local-dev.md).

## Using the Cache

Clone or fetch through the Git remote like any other HTTP remote:

```sh
git clone http://127.0.0.1:8080/git/github.com/org/repo.git
git clone --depth 1 http://127.0.0.1:8080/git/github.com/org/repo.git
git fetch http://127.0.0.1:8080/git/github.com/org/repo.git refs/heads/main
```

Or drive the materialization API directly:

```sh
curl -s http://127.0.0.1:8080/v1/materialize \
  -H 'content-type: application/json' \
  -d '{"repo":"github.com/org/repo","selector":{"branch":"main"}}'

curl -s http://127.0.0.1:8080/v1/resolve \
  -H 'content-type: application/json' \
  -d '{"repo":"github.com/org/repo","selector":{"default_branch":true}}'
```

Selectors are `commit`, `short_commit`, `branch`, and `default_branch`. Request
bodies are strict: only `repo`, `selector`, and `upstream_authorization` are
accepted.

For private repositories, pass credentials per request. The materialize API
uses a cache-specific header; direct Git uses the normal HTTP authorization
header:

```sh
curl -s http://127.0.0.1:8080/v1/materialize \
  -H 'content-type: application/json' \
  -H 'git-cache-upstream-authorization: Basic <base64-user-colon-token>' \
  -d '{"repo":"github.com/org/private","selector":{"branch":"main"},"upstream_authorization":"required"}'

git -c http.extraHeader='Authorization: Basic <base64-user-colon-token>' \
  fetch http://127.0.0.1:8080/git/github.com/org/private.git refs/heads/main
```

Clients can opt out of cold-miss proxying per request with the
`git-cache-use-proxy-on-miss: false` header.

## Configuration

The server reads either a TOML config file (set `GIT_CACHE_CONFIG` to its path;
see `config/*.example.toml`) or individual environment variables. When
`GIT_CACHE_CONFIG` is set, the file wins and the other configuration variables
are ignored — except S3 credentials (`GIT_CACHE_S3_ACCESS_KEY`,
`GIT_CACHE_S3_SECRET_KEY`, `GIT_CACHE_S3_SESSION_TOKEN`,
`GIT_CACHE_S3_REGION`), which are always read from the environment and never
from the file.

### Core

| Variable | Default | What it does |
| --- | --- | --- |
| `GIT_CACHE_CONFIG` | – | Path to a TOML config file. If set, other `GIT_CACHE_*` variables are ignored, except the S3 credential variables noted above. |
| `GIT_CACHE_BIND_ADDR` | `127.0.0.1:8080` | Address and port the HTTP server listens on. |
| `GIT_CACHE_ROOT` | `./cache` | Directory for the local hot cache (bare repos, temp files, repo index). |
| `GIT_CACHE_ALLOWED_UPSTREAM_HOSTS` | `github.com` | Comma-separated allowlist of upstream hosts the cache will talk to (e.g. `github.com,gitlab.com,git.internal.example`). |
| `GIT_CACHE_UPSTREAM_ROOT` | – | Optional local directory of bare upstream repos, used instead of real network upstreams (mainly for tests and local dev). |
| `GIT_CACHE_RATE_LIMIT_PER_MINUTE` | `120` | Global rate limit for materialize requests. |

### Object store

| Variable | Default | What it does |
| --- | --- | --- |
| `GIT_CACHE_OBJECT_STORE_KIND` | `local` | `local` (filesystem) or `s3`. S3 requires building with the `s3` feature. |
| `GIT_CACHE_OBJECT_STORE_ROOT` | `./tmp/object-store` | Root directory for the `local` object store. |
| `GIT_CACHE_S3_BUCKET` | – | S3 bucket name. Required when kind is `s3`. |
| `GIT_CACHE_S3_PREFIX` | `repos` | Key prefix inside the bucket. A schema-version suffix is appended automatically (e.g. `repos` stores under `repos-v3`). |
| `GIT_CACHE_S3_ENDPOINT` | – | Custom S3 endpoint for compatible stores (MinIO, Cloudflare R2, ...). Enables path-style addressing. |
| `GIT_CACHE_S3_REGION` | falls back to `AWS_REGION` / `AWS_DEFAULT_REGION` | AWS region for the bucket. |
| `GIT_CACHE_S3_ACCESS_KEY` / `GIT_CACHE_S3_SECRET_KEY` | – | Static S3 credentials. If unset, the standard AWS credential chain is used (env vars, profiles, IAM roles, workload identity). |
| `GIT_CACHE_S3_SESSION_TOKEN` | falls back to `AWS_SESSION_TOKEN` | Session token for temporary credentials. |

### Git remote

| Variable | Default | What it does |
| --- | --- | --- |
| `GIT_CACHE_GIT_REMOTE_ENABLED` | `true` | Serve the read-through Git remote at `/git/{host}/{owner}/{repo}.git`. |
| `GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH` | `true` | Fetch missing commits from upstream during a client request instead of failing. |
| `GIT_CACHE_GIT_REMOTE_PROXY_ON_MISS_BY_DEFAULT` | `true` | On a cold miss, proxy upstream's upload-pack response to the client immediately and warm the cache in the background. |
| `GIT_CACHE_GIT_REMOTE_PROXY_TEE_IMPORT` | `true` | While proxying a cold miss, tee the response into the local cache instead of re-fetching upstream afterwards. |
| `GIT_CACHE_GIT_REMOTE_BACKGROUND_IMPORT_CONCURRENCY` | `1` | How many background cache-warm imports may run at once. |

### Git subprocess

| Variable | Default | What it does |
| --- | --- | --- |
| `GIT_CACHE_GIT_BINARY` | `git` | Path to the `git` binary. |
| `GIT_CACHE_GIT_TIMEOUT_SECONDS` | `120` | Timeout for individual Git subprocess invocations. |
| `GIT_CACHE_MAX_GIT_OUTPUT_BYTES` | 16 MiB | Upper bound on captured Git subprocess output. |
| `GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES` | `64` | Semaphore limiting concurrent Git subprocesses (the main CPU/memory knob). |
| `GIT_CACHE_ASYNC_MATERIALIZE_CONCURRENCY` | `2` | Concurrency for background materialization work. |
| `GIT_CACHE_USE_GITOXIDE` | `true` | Use in-process gitoxide for local read-only Git operations; disable as a kill switch to fall back to the `git` binary. |
| `GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV` | – | Name of another env var holding a deployment-wide upstream token, injected via Git config env (never argv or manifests). |

### Disk

| Variable | Default | What it does |
| --- | --- | --- |
| `GIT_CACHE_DISK_QUOTA_BYTES` | 10 GiB | Hot-cache disk quota; LRU eviction keeps usage under this. The Helm chart sets this to 100 GiB to match its default 100Gi PVC — keep the quota at or below the volume size. |
| `GIT_CACHE_DISK_MIN_FREE_BYTES` | 1 GiB | Minimum free disk space to preserve on the cache volume. |
| `GIT_CACHE_DISK_ACCESS_FLUSH_SECS` | `60` | How often buffered repo-access timestamps are flushed to the on-disk index. |

### Compaction and shutdown

| Variable | Default | What it does |
| --- | --- | --- |
| `GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD` | `10` | Generation chain depth that triggers compaction into a single base pack. |
| `GIT_CACHE_COMPACTION_INLINE` | `false` | Run compaction inline after publishes instead of relying on the scheduled job. |
| `GIT_CACHE_COMPACTION_RETENTION_SECS` | 24h | How long superseded generations are kept before the retention sweep may delete them. |
| `GIT_CACHE_SHUTDOWN_READINESS_DELAY_SECONDS` | `5` | After SIGTERM, how long `/healthz` fails before draining, so load balancers stop routing traffic. |
| `GIT_CACHE_SHUTDOWN_DRAIN_TIMEOUT_SECONDS` | `60` | Maximum time to drain in-flight requests before exiting. |

## Deployment

### Helm (Kubernetes)

The project ships as a Helm chart that lives in this repository at
[`deploy/helm/gitmirrorcache`](deploy/helm/gitmirrorcache) (it is not
published to a chart registry yet, so install it from a checkout). It runs the
server as a StatefulSet with a persistent volume for the hot cache and an
hourly CronJob for compaction:

```sh
git clone https://github.com/0lut/gitmirrorcache.git
helm install git-cache gitmirrorcache/deploy/helm/gitmirrorcache \
  --set config.objectStore.s3.bucket=my-git-cache-bucket \
  --set aws.region=us-west-2
```

See the [chart README](deploy/helm/gitmirrorcache/README.md) for credentials,
sizing, and scaling guidance.

### AWS (ECS on EC2)

The maintained AWS path is ECS on Graviton EC2 with host-mounted EBS for the
hot cache and S3 for durable storage. The checked-in wrapper builds, deploys,
and smoke-tests the stack:

```sh
AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm \
  scripts/aws/deploy-and-smoke.sh
```

Preview stacks for any branch, tag, or commit can be deployed behind a shared
ALB with `scripts/aws/deploy-preview.sh`. See
[docs/deployment.md](docs/deployment.md).

### Docker

Prebuilt multi-arch images are published to `ghcr.io/0lut/gitmirrorcache` on
`v*` release tags — the badge at the top of this README always shows the
newest release:

```sh
docker pull ghcr.io/0lut/gitmirrorcache:latest   # most recent release
docker pull ghcr.io/0lut/gitmirrorcache:1.2.3    # pin an exact version
```

A multi-stage [`Dockerfile`](Dockerfile) builds the server and CLI from
source; configure either with the environment variables above. For local S3
testing there is a MinIO compose file:

```sh
docker compose -f docker-compose.minio.yml up -d
```

### Bare metal / anything else

It's a single binary plus the `git` CLI. Build with
`cargo build --release -p git-cache-api --features s3`, point it at a config
file or env vars, and put it behind whatever process supervisor you like.

## CLI

```sh
cargo run -p git-cache-cli -- config
cargo run -p git-cache-cli -- disk-status
cargo run -p git-cache-cli -- warm github.com/org/repo main
cargo run -p git-cache-cli -- optimize github.com/org/repo
cargo run -p git-cache-cli -- compact --all --dry-run
cargo run -p git-cache-cli -- compact --repo github.com/org/repo
```

## Testing

```sh
cargo test --workspace
```

Opt-in integration tests run against real GitHub repositories using only the
Python standard library:

```sh
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_astral_uv
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_git_remote_public
```

See [integration_tests/README.md](integration_tests/README.md) for MinIO/S3
variants. The S3 adapter itself is feature-gated:

```sh
cargo test -p git-cache-objectstore --features s3
```
