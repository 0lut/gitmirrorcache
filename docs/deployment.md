# Deployment Notes

## Current AWS Deployment Path

Use **ECS on Graviton EC2 with host-mounted EBS** for large repository deployments.
Object storage remains the durable source of truth, and the EC2 EBS volume is a
hot cache mounted at `/cache` that survives task/container replacement on the
same host.

```sh
scripts/aws/bootstrap.sh
scripts/aws/deploy-and-smoke.sh
```

The AWS helper scripts default to the current EC2/EBS stack discovered in
`us-west-2`: `ENVIRONMENT=dev-arm` and `NAME_PREFIX=gitmirrorcache-arm`.

`bootstrap.sh` creates the shared S3 bucket and ECR repository. The ECS deploy
script creates or updates the runtime infrastructure:

- IAM roles for ECS task execution, task runtime, and the EC2 container instance
- ECS cluster, service, task definition, and CloudWatch log group
- hourly EventBridge rule that runs `git-cache compact --all` as a one-off ECS task
- internet-facing HTTP ALB and target group
- Amazon Linux 2023 ECS-optimized EC2 instance
- non-root gp3 EBS volume mounted on the host at `/cache`
- Docker image built with `git-cache-api` and `git-cache` CLI S3 support

Common overrides:

```sh
APP_NAME=gitmirrorcache
ENVIRONMENT=dev-arm
NAME_PREFIX=gitmirrorcache-arm
AWS_REGION=us-west-2
ECS_EC2_INSTANCE_TYPE=m8g.2xlarge
ECS_CPU_ARCHITECTURE=ARM64
ECS_EC2_AMI_ID=ami-0ac01d3c8b7a34f9d
DOCKER_PLATFORM=linux/arm64
ECS_EBS_SIZE_GIB=128
ECS_EBS_IOPS=8000
ECS_EBS_THROUGHPUT=500
ECS_CPU=8192
ECS_MEMORY=24576
GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES=8
PUBLIC_BASE_URL=https://cache.example.com
GITHUB_TOKEN_SECRET_ARN=arn:aws:secretsmanager:us-west-2:123456789012:secret:github-token
ECS_COMPACTION_SCHEDULE_EXPRESSION='rate(1 hour)'
ECS_COMPACTION_MEMORY_RESERVATION=4096
GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD=10
ECS_ALB_HEALTH_CHECK_INTERVAL_SECONDS=5
ECS_ALB_HEALTH_CHECK_TIMEOUT_SECONDS=2
ECS_HEALTH_CHECK_GRACE_PERIOD_SECONDS=15
```

If `PUBLIC_BASE_URL` is omitted, the deployment script uses the ALB DNS name and
sets `GIT_CACHE_PUBLIC_BASE_URL` to `http://<alb-dns-name>`.

For `dev-*` and preview environments, `deploy-and-smoke.sh` defaults to
5-second ALB health checks, a 15-second ECS health-check grace period, and
5-second service-stability polling. Those defaults improve startup detection
without shortening the ALB request-drain window or ECS container stop timeout.
If a disposable environment can tolerate interrupting in-flight requests during
deploy, set `ECS_ALB_DEREGISTRATION_DELAY_SECONDS` or
`ECS_CONTAINER_STOP_TIMEOUT_SECONDS` explicitly.

## Amazon Linux 2023 Host AMI

The maintained ECS host default is pinned to the latest Amazon Linux 2023
ECS-optimized ARM64 AMI verified in `us-west-2` on June 8, 2026:

```sh
ECS_EC2_AMI_ID=ami-0ac01d3c8b7a34f9d
```

AWS metadata at pin time:

- image name: `al2023-ami-ecs-hvm-2023.0.20260527-kernel-6.1-arm64`
- image version: `2023.0.20260527`
- ECS agent: `1.103.2`
- ECS runtime: `Docker version 25.0.14`

To intentionally advance the host image later, query AWS's current recommendation
and update the pinned `ECS_EC2_AMI_ID` in the deploy script and this document:

```sh
aws ssm get-parameter \
  --region us-west-2 \
  --name /aws/service/ecs/optimized-ami/amazon-linux-2023/arm64/recommended \
  --query Parameter.Value \
  --output text
```

Changing the AMI ID only affects newly launched EC2 instances. The completed
AL2-to-AL2023 migration and validation history is recorded in
[PR #48](https://github.com/0lut/gitmirrorcache/pull/48).

## Why EC2/EBS Instead Of App Runner

App Runner was removed from the maintained deployment path. Large repositories
need a larger and more durable local hot cache than App Runner's ephemeral
filesystem provides. The Linux kernel and GCC validation runs showed that the
service needs to keep hydrated bare repositories and large Git bundle work files
on block storage to avoid repeatedly rehydrating or regenerating expensive cache
state.

ECS on Graviton EC2 with host-mounted EBS gives the service:

- controllable disk size and gp3 performance settings
- hot-cache persistence across task replacement on the same instance
- direct host volume mounting for `/cache`
- SSM access for operational inspection
- ordinary ECS service rollout semantics behind an ALB

S3 remains the durable cache source. The EBS volume is still disposable from a
correctness perspective; losing it should only force hydration from object
storage or upstream verification.

The application appends the v2 schema suffix to the configured object-store
namespace at runtime. For example, `S3_PREFIX=repos` is served from `repos-v2`.
The ECS deploy script grants task IAM access to that runtime prefix while still
passing the base prefix to the container.

## Preview Commit Stacks

Use preview stacks to deploy any branch, tag, or commit without touching the
production `main` stack. Production keeps using `NAME_PREFIX=gitmirrorcache-arm`
and `S3_PREFIX=repos`; every preview gets a derived stack name, an isolated S3
prefix, and a versioned route on the shared preview ALB.

### Local CLI

Run preview deploys from a shell with AWS CLI credentials. The preview wrapper
resolves the requested branch, tag, or commit with local Git and derives the
preview version from that commit:

```sh
AWS_REGION=us-west-2 scripts/aws/deploy-preview.sh HEAD
scripts/aws/deploy-preview.sh my-branch
scripts/aws/deploy-preview.sh d35c30fab123
```

That command computes:

- `VERSION_ID=$(git rev-parse --short=12 "my-branch^{commit}")`
- `NAME_PREFIX=gmc-p-$VERSION_ID`
- `ENVIRONMENT=preview-$VERSION_ID`
- `S3_PREFIX=previews/$VERSION_ID/repos`
- `IMAGE_TAG=$VERSION_ID`
- `ECS_PUBLIC_PATH_PREFIX=/v/$VERSION_ID`

The application still appends the runtime schema suffix, so preview cache
objects land below `previews/$VERSION_ID/repos-v2`. By default, the preview
stack uses the shared production-style bucket and ECR repository derived from
`PREVIEW_SHARED_NAME_PREFIX=gitmirrorcache-arm`. It also uses a static shared
preview ALB named `$PREVIEW_SHARED_NAME_PREFIX-preview-alb` and creates a
per-version listener rule for `/v/$VERSION_ID/*`. That rule rewrites the URL to
strip `/v/$VERSION_ID` before forwarding, so the service still receives normal
`/healthz`, `/v1/materialize`, and `/git/...` paths while clients use stable
versioned URLs:

```txt
http://<preview-alb>/v/$VERSION_ID/healthz
http://<preview-alb>/v/$VERSION_ID/v1/materialize
http://<preview-alb>/v/$VERSION_ID/git/github.com/octocat/Hello-World.git
```

Override the shared bucket, ECR repository, or ALB explicitly when needed:

```sh
PREVIEW_S3_BUCKET=gitmirrorcache-arm-123456789012-us-west-2 \
PREVIEW_ECR_REPOSITORY=gitmirrorcache-arm \
PREVIEW_ALB_NAME=gitmirrorcache-arm-preview-alb \
scripts/aws/deploy-preview.sh my-branch
```

Set `GITHUB_TOKEN_SECRET_ARN` if preview tasks should receive the upstream
GitHub token from Secrets Manager.

Preview deploys set `ECR_PUSH_LATEST=false`,
`ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS=true`,
`ECS_EC2_INSTANCE_TYPE=m8g.2xlarge`, `ECS_PRECHECK_VCPU_QUOTA=true`,
`ECS_EBS_DELETE_ON_TERMINATION=true`, `ECS_COMPACTION_ENABLED=false`,
`ECS_LOG_RETENTION_DAYS=3`, `BOOTSTRAP_FAST_EXISTING=true`,
`PREVIEW_SHARED_ALB=true`, a 15-second ECS health-check grace period, faster ECS
service polling, and shorter ALB target health-check intervals unless you
override them. This keeps previews isolated, disposable, faster to redeploy, and
less noisy while still exercising the ECS, EC2/EBS, ALB listener-rule, IAM, ECR,
S3, and smoke-test path. The quota preflight fails before creating preview
infrastructure if launching the preview instance would exceed the account's EC2
on-demand vCPU quota.

Every preview deploy writes ordered phase timings to stdout. The key phases
include shared bootstrap, IAM, networking, shared ALB/listener-rule upsert, EC2
launch/ECS registration, Docker build and push, task registration, ECS service
stabilization, smoke test, manifest upload, and total preview deployment time.
To capture the same timing table from lower-level scripts, set
`DEPLOY_TIMING_FILE=/path/to/timings.tsv` before invoking `deploy-and-smoke.sh`
or `deploy-ecs-ec2-ebs.sh`.

Destroy a preview with the same ref:

```sh
scripts/aws/destroy-preview.sh my-branch
```

If the branch is gone, pass the derived version or full commit SHA directly.
The script normalizes it to the 12-character preview version:

```sh
VERSION_ID=d35c30fab123 scripts/aws/destroy-preview.sh
```

Destroy removes the preview ECS service, cluster, shared ALB listener rule,
target group, EC2 instance, available preview EBS volume, task definitions,
EventBridge compaction rule, log group, preview IAM roles, and the preview image
tag. It also cleans up a legacy per-preview ALB if that version was deployed
before shared preview ingress was enabled. It removes the deployment manifest by
default and preserves durable cache objects unless you opt in:

```sh
DELETE_DATA=true VERSION_ID=d35c30fab123 scripts/aws/destroy-preview.sh
```

## Deployment Findings

The large-repository investigation found two important cache behaviors:

1. Hydrated local repositories can already contain requested ancestor commits.
   Those exact-commit requests should be indexed from known complete generations
   without creating a new bundle.
2. A cold request for an older commit can produce a full bundle that also
   contains branch tips and other descendants. The publish path now writes
   commit manifests for local cache-ref tips included in that bundle, so later
   descendant requests can be metadata-only `cache_verified` operations.

The deployed verification used a clean repo and requested `tip~2`, then
`tip~1`, then `tip`. After the cold `tip~2` request, both descendant requests
returned `cache_verified` and the bundle count stayed at one.

Previously published generations are not retroactively repaired. If an old
revision created a full bundle without descendant commit manifests, a clean
end-to-end retest requires clearing that repo's test cache prefix/local hot
cache or using a fresh repo.

## Operational Checks

Health and metrics:

```sh
curl -fsS "$PUBLIC_BASE_URL/healthz"
curl -fsS "$PUBLIC_BASE_URL/metrics"
```

ECS rollout state:

```sh
aws ecs describe-services \
  --cluster gitmirrorcache-arm-ec2 \
  --services gitmirrorcache-arm-ec2-api \
  --query 'services[0].{Running:runningCount,Pending:pendingCount,Desired:desiredCount,Deployments:deployments}'
```

ALB target health:

```sh
tg_arn="$(aws elbv2 describe-target-groups \
  --names gitmirrorcache-arm-ec2-host-api \
  --query 'TargetGroups[0].TargetGroupArn' \
  --output text)"
aws elbv2 describe-target-health --target-group-arn "$tg_arn"
```

Recent logs:

```sh
aws logs tail /ecs/gitmirrorcache-arm/ec2-api --since 30m --format short
```

Recent hourly compaction logs:

```sh
aws logs tail /ecs/gitmirrorcache-arm/ec2-api --since 2h --format short \
  --log-stream-name-prefix compaction
```

Compaction schedule:

```sh
aws events describe-rule --name gitmirrorcache-arm-compact-hourly
aws events list-targets-by-rule --rule gitmirrorcache-arm-compact-hourly
```

If an ECS rollout appears stuck while the old task is stopped, check the host for
a stale Docker container still holding port `8080`:

```sh
AWS_REGION=us-west-2 \
ENVIRONMENT=dev-arm \
NAME_PREFIX=gitmirrorcache-arm \
ECS_INSTANCE_ID=i-xxxxxxxxxxxxxxxxx \
scripts/aws/ecs-host-diagnostics.sh
```

Stop only the stale prior-revision container if ECS has already replaced the
task and the stale process is blocking the new revision from binding `8080`.
Use the checked-in recovery script rather than one-off SSM commands:

```sh
AWS_REGION=us-west-2 \
ENVIRONMENT=dev-arm \
NAME_PREFIX=gitmirrorcache-arm \
ECS_INSTANCE_ID=i-xxxxxxxxxxxxxxxxx \
ECS_STALE_CONTAINER_ID=<docker-container-id> \
CONFIRM_STOP=true \
scripts/aws/stop-stale-ecs-container.sh
```

The script verifies the container belongs to the expected ECS task family and
container name before stopping it.

## Worker Model

Run API/worker instances behind a load balancer. Each worker owns local block
storage mounted at `cache_root`; this storage is a hot cache only and can be
deleted without losing durable state.

Object storage is the source of truth for:

- generation manifests
- bundles
- commit manifests
- ref manifests
- lease objects

## Credentials

Set `upstream_auth_token_env = "GITHUB_TOKEN"` in config and provide that
environment variable to the process. In ECS, set `GITHUB_TOKEN_SECRET_ARN` so the
deploy script wires the token as a task secret. The API injects the token through
Git config environment variables:

- no token in argv
- no token in manifests
- no token in structured logs from command arguments

## Metrics And Limits

- `/metrics` exposes Prometheus-style counters.
- `rate_limit_per_minute` applies a simple global materialize limit.
- `max_git_output_bytes` bounds captured Git stdout/stderr.
- `git_timeout_seconds` bounds Git process lifetime.
- `GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES` bounds simultaneous Git subprocesses,
  including active upload-pack streams.
- `ECS_EBS_IOPS` and `ECS_EBS_THROUGHPUT` provision gp3 hot-cache performance.

## Bundle Strategy And Compaction

The publish path writes a generation bundle and commit/ref manifests that prove
which commits are complete. Hydration downloads bundles to disk before fetching
them into the local bare repository, so large bundles are not loaded entirely
into memory.

The ECS deployment registers an hourly EventBridge rule that runs
`git-cache compact --all` as a separate one-off ECS task. The compaction task
uses the same image, S3 prefix, task role, and host `/cache` mount as the API
service, but has no port mapping. It uses a host-volume `flock` at
`/cache/git-cache-compaction.lock` so a later hourly tick exits successfully if a
previous compaction is still running.

Compaction walks each repo's current generation chain. If the chain exceeds
`GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD` (default `10`), it hydrates the
chain, publishes a new full generation, verifies it with `git fsck`, repoints
commit/ref manifests to the compacted generation, and prunes old generation
objects that are not still needed by pending verification.

Useful overrides:

```sh
ECS_COMPACTION_RULE_NAME=gitmirrorcache-arm-compact-hourly
ECS_COMPACTION_SCHEDULE_EXPRESSION='rate(1 hour)'
ECS_COMPACTION_SCHEDULE_STATE=ENABLED
ECS_COMPACTION_LOCK_PATH=/cache/git-cache-compaction.lock
ECS_COMPACTION_MEMORY_RESERVATION=4096
GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD=10
```

## Multi-Worker Safety

- Local repo corruption is handled by deleting local state and hydrating from object storage.
- Force-pushed branches update ref manifests after upstream verification; older commit manifests remain available until retention cleanup.
- Push endpoints are rejected at the HTTP layer and `git-receive-pack` is never served.
