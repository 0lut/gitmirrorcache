---
name: gitmirrorcache-deploy
description: Deploy gitmirrorcache to AWS ECS/EC2/EBS and recover stale ECS containers safely
triggers:
  - user
  - model
---

Use this skill when asked to deploy gitmirrorcache, check a deployment, or
recover a stuck ECS rollout.

## AWS credentials

The `AWS_Access_Key` secret env var holds a 2-line CSV (header + key,secret).
Export it in each new shell before any AWS command:

```sh
eval "$(python3 -c "import os; lines=os.environ['AWS_Access_Key'].strip().splitlines(); k,s=lines[-1].split(','); print(f'export AWS_ACCESS_KEY_ID={k}; export AWS_SECRET_ACCESS_KEY={s}')")"
```

Account 411474713009, region us-west-2. EC2 vCPU quota is 32 — check running
instances before launching large instance types (VcpuLimitExceeded otherwise).

## AWS CLI v2 required

The preview shared-ALB listener rule uses `aws elbv2 ... --transforms`, which
AWS CLI v1 does not support. Install CLI v2 (lands at /usr/local/bin/aws) and
run deploy scripts with `PATH=/usr/local/bin:$PATH` if v1 is also installed.

## Standard deployment

1. Confirm the working tree is clean and switch to `main`.
2. Pull latest `main`.
3. Use the checked-in deployment wrapper, not one-off AWS/Docker commands:

   ```sh
   AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm scripts/aws/deploy-and-smoke.sh
   ```

4. Verify the ECS service and task definition:

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
  build when the ECR tag already exists (e.g. after a buildbox build).
- API logs: `aws logs tail /ecs/gmc-p-<VERSION_ID>/ec2-api --region us-west-2 --since 15m --format short`

## Native arm64 image build on a buildbox

Local arm64 builds run under QEMU and are very slow. Build natively instead
(~6 min on c8g.2xlarge), and ALWAYS terminate the buildbox afterwards:

```sh
AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm \
  DEVBOX_INSTANCE_TYPE=c8g.2xlarge DEVBOX_NAME=gitmirrorcache-arm-buildbox \
  DEVBOX_KEY_NAME=gitmirrorcache-arm-buildbox scripts/aws/devbox.sh
# prints INSTANCE_ID, PUBLIC_HOST, SSH_USER=ec2-user, PRIVATE_KEY_PATH

ssh -i $KEY ec2-user@$HOST 'sudo dnf install -y docker git && sudo systemctl start docker'
git archive --format=tar HEAD | gzip | ssh -i $KEY ec2-user@$HOST 'mkdir -p ~/src && tar -xzf - -C ~/src'
aws ecr get-login-password --region us-west-2 | ssh -i $KEY ec2-user@$HOST "sudo docker login --username AWS --password-stdin <ECR_URI>"
ssh -i $KEY ec2-user@$HOST "cd ~/src && sudo DOCKER_BUILDKIT=1 docker build --platform linux/arm64 -t <ECR_URI>:<tag> -f Dockerfile . && sudo docker push <ECR_URI>:<tag>"

aws ec2 terminate-instances --instance-ids <INSTANCE_ID> --region us-west-2
```

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
