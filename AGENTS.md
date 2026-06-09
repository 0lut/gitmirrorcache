# Agent Guidelines - gitmirrorcache

Keep this file short and current. Prefer the repo's checked-in docs, scripts,
and tests over ad-hoc operational steps.

## Git Boundaries

- Any public `git-cache-git` method that forwards caller input to `git` must
  validate at the top of the method before `self.run(...)` or `spawn(...)`.
- Use the narrowest helper: `reject_ref_arg`, `reject_revision_arg`,
  `reject_config_key`, `reject_remote_url`, `reject_refspec`,
  `reject_fetch_filter`, `reject_fetch_depth`, or `reject_nul`.
- External strings must reject empty values, leading `-` where they become git
  args, and NUL bytes. Refs reject `:`; revisions may allow `HEAD:path`;
  refspecs may allow `+` and `:`.
- Put `--` before positional args wherever git accepts it.
- Add or update tests for rejected flag-looking input and NUL bytes when adding
  a wrapper or accepting new query/body/header data.

## Current Cache Contract

- `MaterializeRequest` is intentionally small: `repo`, `selector`, and optional
  `upstream_authorization`. Request bodies deny unknown fields; do not revive
  the removed `mode` or session contract.
- HTTP materialize/resolve should call `Materializer::materialize` or
  `Materializer::resolve` directly after rate limiting and auth checks. Keep the
  worker coordinator for cron, event hints, and explicit warming.
- Exact commit materialization should use complete cached generation metadata
  before contacting upstream. Branch and default-branch materialization must
  verify upstream refs.
- Direct Git uses `/git/{host}/{owner}/{repo}.git`, rejects receive-pack, and
  serves from the shared bare repo under `cache_root/repos/...`.
- Direct Git GET proves repo access via `ls-remote` only. POST read-through uses
  the same request-scoped auth and must preserve shallow/blobless intent
  (`depth`, `blob:none`) when fetching wants.
- `git-cache-use-proxy-on-miss` is the only cold-miss proxy opt-in. Proxy only
  HTTP(S) upstreams, bound streamed bytes, forward auth only to upstream, then
  queue a bounded background warm. The proxy readiness/background warm paths
  should not hydrate generation manifests.

## Runtime Safety

- Production code must not panic for recoverable errors. Never use
  `.expect()`/`.unwrap()` on `Mutex::lock()` outside `#[cfg(test)]`; map poison
  to `GitCacheError::Internal` or return a safe default.
- Bound memory: use `ObjectStore::head()` for metadata, stream large bundles or
  packs through disk with `ObjectStore::put_file()`, pass `max_keys` to
  `list_prefix` when a full listing is unnecessary, and keep subprocess output
  behind `read_bounded()`.
- Bound resources: every git subprocess spawn must acquire the `Git` semaphore,
  streaming upload-pack responses must hold the permit until exit, and every
  `tokio::process::Command` child needs `kill_on_drop(true)`.
- Prefer explicit disk reservation release; never call `temp_path()` after
  `release()`.

## Deployments

- Use checked-in AWS scripts instead of raw AWS/SSM/Docker command sequences.
  The maintained path is ECS on Graviton EC2 with host-mounted EBS and S3:

```sh
AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm scripts/aws/deploy-and-smoke.sh
```

- Preview stacks go through `scripts/aws/deploy-preview.sh <ref>` and
  `scripts/aws/destroy-preview.sh <ref>`; they use isolated S3 prefixes and
  shared preview ALB routes.
- If a rollout is stuck because an old task still owns host port `8080`, inspect
  with `scripts/aws/ecs-host-diagnostics.sh`, then use
  `scripts/aws/stop-stale-ecs-container.sh` with `ECS_STALE_CONTAINER_ID` and
  `CONFIRM_STOP=true`.
