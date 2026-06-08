#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

[[ "${CONFIRM_DOWNTIME:-false}" == "true" ]] || die "set CONFIRM_DOWNTIME=true to scale the service to zero and replace the ECS host"

init_aws_context

PINNED_AL2023_ARM64_ECS_AMI_ID="ami-0ac01d3c8b7a34f9d"

ECS_CLUSTER_NAME="${ECS_CLUSTER_NAME:-$NAME_PREFIX-ec2}"
ECS_SERVICE_NAME="${ECS_SERVICE_NAME:-$NAME_PREFIX-ec2-api}"
ECS_INSTANCE_NAME="${ECS_INSTANCE_NAME:-$NAME_PREFIX-ecs-cache}"
ECS_EBS_DEVICE_NAME="${ECS_EBS_DEVICE_NAME:-/dev/xvdf}"
CREATE_CACHE_SNAPSHOT="${CREATE_CACHE_SNAPSHOT:-false}"

if [[ -n "${ECS_EC2_AMI_PARAMETER:-}" ]]; then
  if [[ "$ECS_EC2_AMI_PARAMETER" == *"amazon-linux-2"* ]]; then
    [[ "${CONFIRM_AL2_ROLLBACK:-false}" == "true" ]] \
      || die "set CONFIRM_AL2_ROLLBACK=true to use an AL2 AMI parameter"
  else
    [[ "$ECS_EC2_AMI_PARAMETER" == *"amazon-linux-2023"* ]] \
      || die "ECS_EC2_AMI_PARAMETER must point at AL2023 unless CONFIRM_AL2_ROLLBACK=true is set for rollback"
  fi
  unset ECS_EC2_AMI_ID
else
  ECS_EC2_AMI_ID="${ECS_EC2_AMI_ID:-$PINNED_AL2023_ARM64_ECS_AMI_ID}"
  [[ "$ECS_EC2_AMI_ID" =~ ^ami-[a-f0-9]+$ ]] || die "invalid ECS_EC2_AMI_ID: $ECS_EC2_AMI_ID"
fi

current_desired_count="$(aws_cli ecs describe-services \
  --cluster "$ECS_CLUSTER_NAME" \
  --services "$ECS_SERVICE_NAME" \
  --query 'services[0].desiredCount' \
  --output text)"
[[ "$current_desired_count" =~ ^[0-9]+$ ]] || die "could not read desired count for $ECS_CLUSTER_NAME/$ECS_SERVICE_NAME"

ECS_DESIRED_COUNT="${ECS_DESIRED_COUNT:-$current_desired_count}"
[[ "$ECS_DESIRED_COUNT" =~ ^[1-9][0-9]*$ ]] || die "ECS_DESIRED_COUNT must be greater than zero for redeploy; got $ECS_DESIRED_COUNT"
export ECS_DESIRED_COUNT
if [[ -n "${ECS_EC2_AMI_PARAMETER:-}" ]]; then
  export ECS_EC2_AMI_PARAMETER
else
  export ECS_EC2_AMI_ID
fi

instance_id="$(aws_cli ec2 describe-instances \
  --filters "Name=tag:Name,Values=$ECS_INSTANCE_NAME" Name=instance-state-name,Values=pending,running,stopping,stopped \
  --query 'Reservations[].Instances[].InstanceId | [0]' \
  --output text)"

printf 'Scaling ECS service to zero: %s/%s\n' "$ECS_CLUSTER_NAME" "$ECS_SERVICE_NAME"
aws_cli ecs update-service \
  --cluster "$ECS_CLUSTER_NAME" \
  --service "$ECS_SERVICE_NAME" \
  --desired-count 0 >/dev/null
aws_cli ecs wait services-stable --cluster "$ECS_CLUSTER_NAME" --services "$ECS_SERVICE_NAME"

if [[ "$instance_id" == "None" || -z "$instance_id" ]]; then
  printf 'No existing ECS container instance named %s was found; continuing to deploy AL2023 host\n' "$ECS_INSTANCE_NAME"
else
  [[ "$instance_id" =~ ^i-[a-f0-9]+$ ]] || die "invalid EC2 instance id for $ECS_INSTANCE_NAME: $instance_id"

  cache_volume_id="$(aws_cli ec2 describe-instances \
    --instance-ids "$instance_id" \
    --query "Reservations[0].Instances[0].BlockDeviceMappings[?DeviceName=='$ECS_EBS_DEVICE_NAME'].Ebs.VolumeId | [0]" \
    --output text)"

  if [[ "$CREATE_CACHE_SNAPSHOT" == "true" && "$cache_volume_id" != "None" && -n "$cache_volume_id" ]]; then
    snapshot_id="$(aws_cli ec2 create-snapshot \
      --volume-id "$cache_volume_id" \
      --description "$NAME_PREFIX cache before AL2023 ECS host migration" \
      --query SnapshotId \
      --output text)"
    printf 'Created cache volume snapshot: %s from %s\n' "$snapshot_id" "$cache_volume_id"
  elif [[ "$cache_volume_id" != "None" && -n "$cache_volume_id" ]]; then
    printf 'Leaving old cache volume for manual cleanup/rollback: %s\n' "$cache_volume_id"
  fi

  printf 'Terminating existing ECS host: %s\n' "$instance_id"
  aws_cli ec2 terminate-instances --instance-ids "$instance_id" >/dev/null
  aws_cli ec2 wait instance-terminated --instance-ids "$instance_id"
fi

if [[ -n "${ECS_EC2_AMI_PARAMETER:-}" ]]; then
  printf 'Deploying replacement ECS host with AMI parameter: %s\n' "$ECS_EC2_AMI_PARAMETER"
else
  printf 'Deploying replacement ECS host with pinned AMI ID: %s\n' "$ECS_EC2_AMI_ID"
fi
"$SCRIPT_DIR/deploy-and-smoke.sh"
