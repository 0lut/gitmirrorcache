# Deployment Notes

## Current AWS Deployment Path

Use **ECS on EC2 with host-mounted EBS** for large repository deployments.
Object storage remains the durable source of truth, and the EC2 EBS volume is a
hot cache mounted at `/cache` that survives task/container replacement on the
same host.

```sh
AWS_REGION=us-west-2 ENVIRONMENT=dev scripts/aws/bootstrap.sh
AWS_REGION=us-west-2 ENVIRONMENT=dev scripts/aws/deploy-ecs-ec2-ebs.sh
AWS_REGION=us-west-2 ENVIRONMENT=dev scripts/aws/smoke-test.sh
```

`bootstrap.sh` creates the shared S3 bucket and ECR repository. The ECS deploy
script creates or updates the runtime infrastructure:

- IAM roles for ECS task execution, task runtime, and the EC2 container instance
- ECS cluster, service, task definition, and CloudWatch log group
- internet-facing HTTP ALB and target group
- ECS-optimized EC2 instance
- non-root gp3 EBS volume mounted on the host at `/cache`
- Docker image built with `git-cache-api --features s3`

Common overrides:

```sh
APP_NAME=gitmirrorcache
ENVIRONMENT=dev
AWS_REGION=us-west-2
ECS_EC2_INSTANCE_TYPE=m7i.xlarge
ECS_EBS_SIZE_GIB=128
ECS_CPU=4096
ECS_MEMORY=8192
PUBLIC_BASE_URL=https://cache.example.com
GITHUB_TOKEN_SECRET_ARN=arn:aws:secretsmanager:us-west-2:123456789012:secret:github-token
```

If `PUBLIC_BASE_URL` is omitted, the deployment script uses the ALB DNS name and
sets `GIT_CACHE_PUBLIC_BASE_URL` to `http://<alb-dns-name>`.

## Why EC2/EBS Instead Of App Runner

App Runner was removed from the maintained deployment path. Large repositories
need a larger and more durable local hot cache than App Runner's ephemeral
filesystem provides. The Linux kernel and GCC validation runs showed that the
service needs to keep hydrated bare repositories and large Git bundle work files
on block storage to avoid repeatedly rehydrating or regenerating expensive cache
state.

ECS on EC2 with host-mounted EBS gives the service:

- controllable disk size and gp3 performance settings
- hot-cache persistence across task replacement on the same instance
- direct host volume mounting for `/cache`
- SSM access for operational inspection
- ordinary ECS service rollout semantics behind an ALB

S3 remains the durable cache source. The EBS volume is still disposable from a
correctness perspective; losing it should only force hydration from object
storage or upstream verification.

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
  --cluster gitmirrorcache-dev-ec2 \
  --services gitmirrorcache-dev-ec2-api \
  --query 'services[0].{Running:runningCount,Pending:pendingCount,Desired:desiredCount,Deployments:deployments}'
```

ALB target health:

```sh
tg_arn="$(aws elbv2 describe-target-groups \
  --names gitmirrorcache-dev-ec2-host-api \
  --query 'TargetGroups[0].TargetGroupArn' \
  --output text)"
aws elbv2 describe-target-health --target-group-arn "$tg_arn"
```

Recent logs:

```sh
aws logs tail /ecs/gitmirrorcache-dev/ec2-api --since 30m --format short
```

If an ECS rollout appears stuck while the old task is stopped, check the host for
a stale Docker container still holding port `8080`:

```sh
aws ssm send-command \
  --instance-ids i-xxxxxxxxxxxxxxxxx \
  --document-name AWS-RunShellScript \
  --parameters 'commands=["docker ps -a --format \"{{.ID}} {{.Image}} {{.Names}} {{.Status}} {{.Ports}}\"","sudo ss -ltnp | grep :8080 || true"]'
```

Stop only the stale prior-revision container if ECS has already replaced the
task and the stale process is blocking the new revision from binding `8080`.

## Worker Model

Run API/worker instances behind a load balancer. Each worker owns local block
storage mounted at `cache_root`; this storage is a hot cache only and can be
deleted without losing durable state.

Object storage is the source of truth for:

- generation manifests
- bundles
- commit manifests
- ref manifests
- session manifests
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

## Bundle Strategy And Compaction

The publish path writes a generation bundle and commit/ref manifests that prove
which commits are complete. Hydration downloads bundles to disk before fetching
them into the local bare repository, so large bundles are not loaded entirely
into memory.

Future compaction can publish a new full generation, verify it with `git fsck`,
move commit/ref manifests to the compacted generation, then prune old
generations after retention expires.

## Multi-Worker Safety

- Local repo corruption is handled by deleting local state and hydrating from object storage.
- Force-pushed branches update ref manifests after upstream verification; older commit manifests remain available until retention cleanup.
- Push endpoints are rejected at the HTTP layer and `git-receive-pack` is never served.
