# Integration Tests

These tests are intentionally opt-in because they hit real GitHub repositories
and may take time to fetch and bundle repository objects.

They use only Python's standard library and shell out to `cargo` and `git`.

## Materialize API tests (`test_astral_uv`)

```sh
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_astral_uv
```

Optional overrides:

```sh
GIT_CACHE_TEST_REPO=github.com/astral-sh/uv \
GIT_CACHE_TEST_BRANCH=main \
RUN_GITHUB_INTEGRATION=1 \
python3 -m unittest -v integration_tests.test_astral_uv
```

What the tests do:

- build and start `git-cache-api` on a random localhost port
- materialize `github.com/astral-sh/uv` `main` with a branch selector
- compare the returned commit to `git ls-remote`
- fetch the branch through the direct `/git/...` remote
- resolve an abbreviated `short_commit` selector to the canonical full commit
- delete local hot-cache repos and verify exact commit materialization rehydrates from object storage with `cache_verified`
- materialize the upstream default branch

To run the same Python API tests against Docker/MinIO instead of the local
filesystem object store:

```sh
docker compose -f docker-compose.minio.yml up -d minio
docker compose -f docker-compose.minio.yml run --rm createbuckets
RUN_GITHUB_INTEGRATION=1 \
GIT_CACHE_USE_MINIO_BACKEND=1 \
GIT_CACHE_S3_ENDPOINT=http://127.0.0.1:9000 \
GIT_CACHE_S3_BUCKET=gitmirrorcache-test \
GIT_CACHE_S3_ACCESS_KEY=minioadmin \
GIT_CACHE_S3_SECRET_KEY=minioadmin \
python3 -m unittest -v integration_tests.test_astral_uv
```

In MinIO mode, the tests assert the local object-store directory is not used,
the configured MinIO bucket prefix is non-empty, and cached pack objects were
written to MinIO.

## Direct Git remote tests (`test_git_remote_public`)

```sh
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_git_remote_public
```

What the tests do:

- build and start `git-cache-api` with the always-on `/git/` remote
- for each high-commit repo (`torvalds/linux`, `llvm/llvm-project`, `gcc-mirror/gcc`, `astral-sh/uv`):
  - `git ls-remote` via the cache and compare to the upstream HEAD
  - `git clone --depth 1` via the cache and verify the cloned HEAD matches upstream

## AWS dev correctness/speed matrix (`test_aws_dev_git_matrix`)

This suite targets an already-deployed AWS dev instance and records JSONL timing
and correctness evidence for public GitHub repos:

```sh
RUN_AWS_DEV_GIT_MATRIX=1 \
GIT_CACHE_AWS_DEV_BASE_URL=http://<dev-alb-dns>.us-west-2.elb.amazonaws.com \
GIT_CACHE_AWS_DEV_RESET_LOCAL_CACHE=1 \
python3 -m unittest -v integration_tests.test_aws_dev_git_matrix
```

By default it covers `astral-sh/uv`, `astral-sh/ruff`, `torvalds/linux`, and
`llvm/llvm-project` with:

- upstream and cache `ls-remote` correctness
- direct GitHub `--depth 1 --filter=blob:none --no-checkout` baseline timing
- cold proxy-on-miss, hot proxy repeat, cold read-through opt-out, and hot
  read-through repeat against the cache
- request-scoped Basic auth runs when `GIT_CACHE_AWS_DEV_BASIC_AUTH`,
  `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token` is available
- blobless-to-full depth-1 transition checks for `uv` and `ruff`
- `git-receive-pack` rejection

`GIT_CACHE_AWS_DEV_RESET_LOCAL_CACHE=1` uses
`scripts/aws/remove-cache-repo.sh` to delete only the local EBS hot-cache repo
before cold lanes; it does not delete S3 durable cache state. Set
`GIT_CACHE_AWS_DEV_RESULTS=/path/results.jsonl` to choose the report path.
Use `GIT_CACHE_AWS_DEV_TIER=heavy` only for explicit large-repo full-history
walk checks.

To run only the heavier local read-through lanes with proxy-on-miss forced off:

```sh
RUN_AWS_DEV_GIT_MATRIX=1 \
GIT_CACHE_AWS_DEV_BASE_URL=http://<dev-alb-dns>.us-west-2.elb.amazonaws.com \
GIT_CACHE_AWS_DEV_RESET_LOCAL_CACHE=1 \
GIT_CACHE_AWS_DEV_SKIP_STANDARD=1 \
GIT_CACHE_AWS_DEV_TIER=heavy \
GIT_CACHE_AWS_DEV_COMMAND_TIMEOUT=7200 \
python3 -m unittest -v integration_tests.test_aws_dev_git_matrix
```

Heavy mode sends `git-cache-use-proxy-on-miss: false` for full-history
`--filter=blob:none --no-checkout` clones on every configured repo, verifies
HEAD plus a bounded history walk, and also runs a blobless full checkout for
`uv` and `ruff`. Add `GIT_CACHE_AWS_DEV_DIRECT_HEAVY_BASELINE=1` to also
measure the same heavy shapes directly from GitHub for comparison.

## LFS smoke tests (`test_lfs_smoke`)

```sh
RUN_GITHUB_INTEGRATION=1 python3 -m unittest -v integration_tests.test_lfs_smoke
```

Tests Git LFS behavior through the cache: verifies the git protocol layer
works for LFS repos (pointer files clone correctly), the LFS batch API
returns 405, and the upstream-URL workaround lets `git lfs pull` succeed.

Optional overrides:

```sh
GIT_CACHE_LFS_TEST_REPO=github.com/git-lfs/test-assets \
RUN_GITHUB_INTEGRATION=1 \
python3 -m unittest -v integration_tests.test_lfs_smoke
```

Requires `git-lfs` installed (`git lfs install`). The test class is
automatically skipped if `git-lfs` is not available.

## Docker / MinIO object-store tests

These tests use Docker Compose to run MinIO locally and exercise the S3-compatible
object-store adapter plus domain materialization over local hot-cache disk storage.

```sh
docker compose -f docker-compose.minio.yml up -d --wait
GIT_CACHE_S3_INTEGRATION=1 \
GIT_CACHE_S3_ENDPOINT=http://127.0.0.1:9000 \
GIT_CACHE_S3_BUCKET=gitmirrorcache-test \
GIT_CACHE_S3_ACCESS_KEY=minioadmin \
GIT_CACHE_S3_SECRET_KEY=minioadmin \
cargo test -p git-cache-objectstore --features s3 minio_

GIT_CACHE_S3_INTEGRATION=1 \
GIT_CACHE_S3_ENDPOINT=http://127.0.0.1:9000 \
GIT_CACHE_S3_BUCKET=gitmirrorcache-test \
GIT_CACHE_S3_ACCESS_KEY=minioadmin \
GIT_CACHE_S3_SECRET_KEY=minioadmin \
cargo test -p git-cache-domain --features s3-tests minio_
```
