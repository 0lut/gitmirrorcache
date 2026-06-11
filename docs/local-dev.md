# Local Development Runbook

This runbook uses local bare repositories as fake upstreams and the local filesystem object-store adapter.

## 1. Create A Fake Upstream

```sh
mkdir -p tmp/upstreams/github.com/acme
git init --bare tmp/upstreams/github.com/acme/widgets.git
git init tmp/work-widgets
cd tmp/work-widgets
git config user.email cache@example.invalid
git config user.name "Cache Test"
printf 'hello\n' > README.md
git add README.md
git commit -m initial
git branch -M main
git remote add origin ../upstreams/github.com/acme/widgets.git
git push origin main
git --git-dir ../upstreams/github.com/acme/widgets.git symbolic-ref HEAD refs/heads/main
```

## 2. Start The API

```sh
cd /path/to/gitmirrorcache
GIT_CACHE_CONFIG=config/local.example.toml cargo run -p git-cache-api
```

## 3. Materialize A Branch

```sh
curl -s http://127.0.0.1:8080/v1/materialize \
  -H 'content-type: application/json' \
  -d '{"repo":"github.com/acme/widgets","selector":{"branch":"main"}}'
```

Branch and default-branch materialization require the fake upstream to be reachable. Exact commit materialization uses cached manifests first. Do not include the removed `mode` field in request bodies.

## 4. Exercise Offline Cached Commit Behavior

1. Materialize `main` once.
2. Move `tmp/upstreams/github.com/acme/widgets.git` out of the way.
3. Materialize the returned commit SHA:

```sh
curl -s http://127.0.0.1:8080/v1/materialize \
  -H 'content-type: application/json' \
  -d '{"repo":"github.com/acme/widgets","selector":{"commit":"<sha>"}}'
```

The response should report `cache_verified`.

## 5. Disk And Object Store State

- Local hot repos: `cache/repos/`
- Reservations: `cache/reservations/`
- Object-store manifests and packs: `tmp/object-store-v3/repos/` (the runtime
  appends the object-store schema suffix to the configured root)

```sh
cargo run -p git-cache-cli -- disk-status
```
