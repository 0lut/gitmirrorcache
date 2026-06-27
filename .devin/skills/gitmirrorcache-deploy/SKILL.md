---
name: gitmirrorcache-deploy
description: Deploy gitmirrorcache to AWS ECS/EC2/EBS and recover stale ECS containers safely
triggers:
  - user
  - model
---

Use this skill when asked to deploy gitmirrorcache, check a deployment, or
recover a stuck ECS rollout.

Requirements: persistent VM, `AWS_Access_Key`, AWS CLI v2, network access to
AWS us-west-2, and authorization to mutate live AWS infrastructure.

This privileged operations runbook owns AWS deployment and recovery procedures.
Follow the shared repository rules in [AGENTS.md](../../../AGENTS.md);
local-only testing runbooks live under
[`.agents/skills`](../../../.agents/skills/).

## AWS credentials

The `AWS_Access_Key` secret env var holds a 2-line CSV (header + key,secret).
Export it in each new shell before any AWS command (no `eval` — parse and
assign directly):

```sh
IFS=, read -r AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY <<< "$(printf '%s' "$AWS_Access_Key" | tail -n 1)"
export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
```

Region is us-west-2. Derive the account ID at runtime instead of hard-coding
it: `aws sts get-caller-identity --query Account --output text`. The dev
account's EC2 vCPU quota is 32 — check running instances before launching
large instance types (VcpuLimitExceeded otherwise).

## AWS CLI v2 required

The preview shared-ALB listener rule uses `aws elbv2 ... --transforms`, which
AWS CLI v1 does not support. Check `aws --version` reports `aws-cli/2.x`; if a
v1 binary shadows v2, find the v2 install with `command -v -a aws` and put its
directory first on `PATH` (e.g. `/usr/local/bin` on the Devin VM,
`/opt/homebrew/bin` on macOS).

## Standard deployment

1. Confirm the working tree is clean and based on latest `origin/main`; a
   tracking `main` branch is not required if the checkout is otherwise clean and
   at `origin/main`.
2. Use the checked-in deployment wrapper, not one-off AWS/Docker commands:

   ```sh
   AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm scripts/aws/deploy-and-smoke.sh
   ```

3. Verify the ECS service and task definition:

   ```sh
   aws ecs describe-services \
     --region us-west-2 \
     --cluster gitmirrorcache-arm-ec2 \
     --services gitmirrorcache-arm-ec2-api \
     --query 'services[0].{Running:runningCount,Pending:pendingCount,Desired:desiredCount,TaskDefinition:taskDefinition,Deployments:deployments}'
   ```

## Preview deployments

```sh
AWS_REGION=us-west-2 scripts/aws/deploy-preview.sh <ref>
VERSION_ID=<12-char-commit> scripts/aws/destroy-preview.sh   # teardown
```

- VERSION_ID is the first 12 chars of the commit SHA; the stack is named
  `gmc-p-<VERSION_ID>` and served at the shared preview ALB under
  `/v/<VERSION_ID>` (e.g. `http://gitmirrorcache-arm-preview-alb-<id>.us-west-2.elb.amazonaws.com/v/<VERSION_ID>/healthz`).
- Set `ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS=true` to skip the local Docker
  build when the ECR tag already exists (e.g. after `build-image-cross.sh`).
- **IMAGE_TAG mismatch gotcha**: `build-image-cross.sh` pushes the ECR image
  tagged with the SHORT commit sha, but `deploy-preview.sh` defaults `IMAGE_TAG`
  to the 12-char VERSION_ID. To reuse a cross-built image, pass it explicitly
  or the deploy silently falls back to a slow QEMU docker build:
  ```sh
  IMAGE_TAG=$(git rev-parse --short HEAD) ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS=true \
    AWS_REGION=us-west-2 scripts/aws/deploy-preview.sh $(git rev-parse HEAD)
  ```
- API logs: `aws logs tail /ecs/gmc-p-<VERSION_ID>/ec2-api --region us-west-2 --since 15m --format short`
- A killed/partial preview deploy still leaves billable resources (EC2
  instance, ALB rule); clean up with `VERSION_ID=<id> scripts/aws/destroy-preview.sh`.

### Cold-cache perf testing on a preview stack

A fresh preview stack guarantees a cold EBS cache (isolated stack + S3 prefix),
making it the right environment for read-through perf measurements. Benchmarks
must opt out of proxy-on-miss (`-c http.extraHeader="git-cache-use-proxy-on-miss: 0"`)
or they measure the proxy, not the cache; use many-ref repos (astral-sh/ruff,
astral-sh/uv) as stress tests. Assert on CloudWatch logs, not just wall time —
see "Cold-Cache Perf Testing" in
[testing-git-remote](../../../.agents/skills/testing-git-remote/SKILL.md) for
the expected log lines. Reference numbers (m8g.2xlarge preview, full clone of
astral-sh/ruff, ~727 refs): batched cold ≈ 37s, warm ≈ 9s, GitHub direct ≈ 9s.
If cold is in the hundreds of seconds, the per-want fetch storm may have
regressed.

## arm64 image build (cross-compile)

On a Linux x86 host, use the checked-in cross-compile wrapper (~2.5 min
total): it compiles the Rust binaries natively with the aarch64 cross-linker,
assembles a runtime-only image from `Dockerfile.cross` via buildx, and pushes
to ECR.

```sh
AWS_REGION=us-west-2 scripts/aws/build-image-cross.sh
# then deploy reusing the pushed tag:
ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS=true IMAGE_TAG=<tag> \
  AWS_REGION=us-west-2 scripts/aws/deploy-and-smoke.sh
```

Prereqs: `gcc-aarch64-linux-gnu`, the `aarch64-unknown-linux-gnu` rustup
target, AWS CLI v2. If `apt-get install gcc-aarch64-linux-gnu` 404s, run
`sudo apt-get update` first (stale package index). The script registers
the qemu binfmt handler itself if missing. `PUSH=false` does a local-only
`--load` build (no AWS access needed). The script is Linux-only; on macOS
(Apple Silicon) build the full Dockerfile natively:
`docker buildx build --platform linux/arm64 -f Dockerfile .`

## Stuck rollout recovery

If the service is stuck with a new task pending and the old target draining,
inspect the ECS host for stale containers holding port `8080`.

Use the diagnostics script:

```sh
AWS_REGION=us-west-2 \
ENVIRONMENT=dev-arm \
NAME_PREFIX=gitmirrorcache-arm \
ECS_INSTANCE_ID=<ec2-instance-id> \
scripts/aws/ecs-host-diagnostics.sh
```

Never issue a raw one-off `docker stop` over SSM. Use the recovery script:

```sh
AWS_REGION=us-west-2 \
ENVIRONMENT=dev-arm \
NAME_PREFIX=gitmirrorcache-arm \
ECS_INSTANCE_ID=<ec2-instance-id> \
ECS_STALE_CONTAINER_ID=<docker-container-id> \
CONFIRM_STOP=true \
scripts/aws/stop-stale-ecs-container.sh
```

This recovery is destructive because it stops a running container. If the user
has not explicitly confirmed the exact container stop, ask before running it.

After recovery, continue monitoring the deployment and run:

```sh
AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm scripts/aws/smoke-test.sh
```
