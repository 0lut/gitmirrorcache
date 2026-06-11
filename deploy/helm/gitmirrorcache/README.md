# gitmirrorcache Helm Chart

Deploys the gitmirrorcache read-through Git caching service to Kubernetes.

The server runs as a StatefulSet with a persistent volume mounted at `/cache`
(the hot cache). An S3-compatible object store remains the durable source of
truth, so the cache volume is disposable: losing it only forces rehydration.
An hourly CronJob runs `git-cache compact --all`, mirroring the EventBridge
compaction rule from the AWS deployment.

## Install

```sh
helm install git-cache deploy/helm/gitmirrorcache \
  --set config.objectStore.s3.bucket=my-git-cache-bucket \
  --set aws.region=us-west-2
```

## S3 credentials

Prefer workload identity over static keys:

```yaml
serviceAccount:
  annotations:
    eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/gitmirrorcache
```

Or reference an existing Secret with `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY` keys:

```yaml
aws:
  region: us-west-2
  existingSecret: git-cache-aws
```

For S3-compatible stores (MinIO, Cloudflare R2, ...), set
`config.objectStore.s3.endpoint`.

## Upstream authentication

To warm and serve private repositories, point `upstreamAuth.existingSecret`
at a Secret containing a token (key `token` by default):

```yaml
upstreamAuth:
  existingSecret: git-cache-github-token
```

## Key values

| Value | Default | Description |
| --- | --- | --- |
| `image.repository` | `ghcr.io/0lut/gitmirrorcache` | Container image |
| `config.objectStore.kind` | `s3` | `s3` or `local` (testing only) |
| `config.objectStore.s3.bucket` | – | Required for `s3` |
| `config.allowedUpstreamHosts` | `[github.com]` | Upstream allowlist |
| `config.gitRemote.enabled` | `true` | Serve `/git/{host}/{owner}/{repo}.git` |
| `config.disk.quotaBytes` | 100 GiB | Hot-cache disk quota |
| `persistence.size` | `100Gi` | PVC size (keep ≥ disk quota) |
| `persistence.enabled` | `true` | Use a PVC; `false` falls back to emptyDir |
| `compaction.enabled` | `true` | Hourly `git-cache compact --all` CronJob |
| `configFile` | `""` | Optional full TOML config (see `config/production.example.toml`) |
| `config.extraEnv` | `[]` | Extra `GIT_CACHE_*` env vars |

See `values.yaml` for the full list.

## Using the cache

```sh
kubectl port-forward svc/git-cache-gitmirrorcache 8080:80
curl http://localhost:8080/healthz
git clone http://localhost:8080/git/github.com/<owner>/<repo>.git
```

## Sizing

Serving clones spawns `git pack-objects` children, which are CPU- and
memory-intensive (bounded by `config.maxConcurrentGitProcesses`, default 64).
The chart defaults to requests of 2 vCPU / 4 GiB with an 8 GiB memory limit —
treat that as the floor. The production ECS deployment runs 8 vCPU / 24 GiB;
scale CPU with concurrent clone traffic and tune
`config.maxConcurrentGitProcesses` to match the allocation.

## Scaling

Each replica keeps its own hot cache on its own PVC. Replicas coordinate only
through the object store; compaction/publish uses conditional PUTs on the
generation head, so multiple replicas and the compaction CronJob are safe to
run concurrently.
