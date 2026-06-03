---
name: gitmirrorcache-deploy
description: Deploy gitmirrorcache to AWS ECS/EC2/EBS and recover stale ECS containers safely
triggers:
  - user
  - model
---

Use this skill when asked to deploy gitmirrorcache, check a deployment, or
recover a stuck ECS rollout.

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
