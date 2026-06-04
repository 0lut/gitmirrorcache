#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context
require_cmd docker
require_cmd python3

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

ECS_CLUSTER_NAME="${ECS_CLUSTER_NAME:-$NAME_PREFIX-ec2}"
ECS_SERVICE_NAME="${ECS_SERVICE_NAME:-$NAME_PREFIX-ec2-api}"
ECS_TASK_FAMILY="${ECS_TASK_FAMILY:-$NAME_PREFIX-ec2-api}"
ECS_CONTAINER_NAME="${ECS_CONTAINER_NAME:-git-cache-api}"
ECS_COMPACTION_TASK_FAMILY="${ECS_COMPACTION_TASK_FAMILY:-$NAME_PREFIX-ec2-compaction}"
ECS_COMPACTION_CONTAINER_NAME="${ECS_COMPACTION_CONTAINER_NAME:-git-cache-compaction}"
ECS_COMPACTION_EVENTS_ROLE_NAME="${ECS_COMPACTION_EVENTS_ROLE_NAME:-$NAME_PREFIX-ecs-compaction-events}"
ECS_COMPACTION_RULE_NAME="${ECS_COMPACTION_RULE_NAME:-$NAME_PREFIX-compact-hourly}"
ECS_COMPACTION_TARGET_ID="${ECS_COMPACTION_TARGET_ID:-compact-all}"
ECS_COMPACTION_SCHEDULE_EXPRESSION="${ECS_COMPACTION_SCHEDULE_EXPRESSION:-rate(1 hour)}"
ECS_COMPACTION_SCHEDULE_STATE="${ECS_COMPACTION_SCHEDULE_STATE:-ENABLED}"
ECS_COMPACTION_LOG_STREAM_PREFIX="${ECS_COMPACTION_LOG_STREAM_PREFIX:-compaction}"
ECS_COMPACTION_LOCK_PATH="${ECS_COMPACTION_LOCK_PATH:-/cache/git-cache-compaction.lock}"
ECS_COMPACTION_MEMORY_RESERVATION="${ECS_COMPACTION_MEMORY_RESERVATION:-4096}"
ECS_CACHE_VOLUME_NAME="${ECS_CACHE_VOLUME_NAME:-cache}"
ECS_ALB_NAME="${ECS_ALB_NAME:-$NAME_PREFIX-ec2-alb}"
ECS_TARGET_GROUP_NAME="${ECS_TARGET_GROUP_NAME:-$NAME_PREFIX-ec2-host-api}"
ECS_ALB_SG_NAME="${ECS_ALB_SG_NAME:-$NAME_PREFIX-ec2-alb}"
ECS_TASK_SG_NAME="${ECS_TASK_SG_NAME:-$NAME_PREFIX-ec2-task}"
ECS_EXECUTION_ROLE_NAME="${ECS_EXECUTION_ROLE_NAME:-$NAME_PREFIX-ecs-ec2-execution}"
ECS_TASK_ROLE_NAME="${ECS_TASK_ROLE_NAME:-$NAME_PREFIX-ecs-ec2-task}"
ECS_INSTANCE_ROLE_NAME="${ECS_INSTANCE_ROLE_NAME:-$NAME_PREFIX-ecs-container-instance}"
ECS_INSTANCE_PROFILE_NAME="${ECS_INSTANCE_PROFILE_NAME:-$ECS_INSTANCE_ROLE_NAME}"
ECS_INSTANCE_NAME="${ECS_INSTANCE_NAME:-$NAME_PREFIX-ecs-cache}"
ECS_LOG_GROUP="${ECS_LOG_GROUP:-/ecs/$NAME_PREFIX/ec2-api}"

ECS_CPU="${ECS_CPU:-8192}"
ECS_MEMORY="${ECS_MEMORY:-24576}"
ECS_DESIRED_COUNT="${ECS_DESIRED_COUNT:-1}"
ECS_EC2_INSTANCE_TYPE="${ECS_EC2_INSTANCE_TYPE:-m8g.2xlarge}"
ECS_CPU_ARCHITECTURE="${ECS_CPU_ARCHITECTURE:-ARM64}"
ECS_EBS_SIZE_GIB="${ECS_EBS_SIZE_GIB:-128}"
ECS_EBS_VOLUME_TYPE="${ECS_EBS_VOLUME_TYPE:-gp3}"
ECS_EBS_IOPS="${ECS_EBS_IOPS:-8000}"
ECS_EBS_THROUGHPUT="${ECS_EBS_THROUGHPUT:-500}"
ECS_EBS_DEVICE_NAME="${ECS_EBS_DEVICE_NAME:-/dev/xvdf}"
ECS_SKIP_DOCKER_BUILD="${ECS_SKIP_DOCKER_BUILD:-false}"
DOCKER_PLATFORM="${DOCKER_PLATFORM:-linux/arm64}"
IMAGE_TAG="${IMAGE_TAG:-$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || date -u +%Y%m%d%H%M%S)}"
IMAGE_URI="${IMAGE_URI:-${ECR_REPOSITORY_URI}:${IMAGE_TAG}}"
LATEST_URI="${ECR_REPOSITORY_URI}:latest"

runtime_s3_prefix() {
  local prefix="$1"
  while [[ "$prefix" == /* ]]; do
    prefix="${prefix#/}"
  done
  while [[ "$prefix" == */ ]]; do
    prefix="${prefix%/}"
  done
  if [[ -z "$prefix" ]]; then
    printf 'v2\n'
    return
  fi
  local component="${prefix##*/}"
  if [[ "$component" == "v2" || "$component" == *-v2 ]]; then
    printf '%s\n' "$prefix"
  elif [[ "$prefix" == */* ]]; then
    printf '%s/%s-v2\n' "${prefix%/*}" "$component"
  else
    printf '%s-v2\n' "$prefix"
  fi
}

S3_RUNTIME_PREFIX="${S3_RUNTIME_PREFIX:-$(runtime_s3_prefix "$S3_PREFIX")}"

GIT_CACHE_DISK_MIN_FREE_BYTES="${GIT_CACHE_DISK_MIN_FREE_BYTES:-10737418240}"
GIT_CACHE_DISK_QUOTA_BYTES="${GIT_CACHE_DISK_QUOTA_BYTES:-$((ECS_EBS_SIZE_GIB * 1024 * 1024 * 1024))}"
if ((GIT_CACHE_DISK_QUOTA_BYTES < 0)); then
  GIT_CACHE_DISK_QUOTA_BYTES=0
fi

export ECS_CLUSTER_NAME ECS_SERVICE_NAME ECS_TASK_FAMILY ECS_CONTAINER_NAME ECS_CACHE_VOLUME_NAME
export ECS_COMPACTION_TASK_FAMILY ECS_COMPACTION_CONTAINER_NAME ECS_COMPACTION_EVENTS_ROLE_NAME
export ECS_COMPACTION_RULE_NAME ECS_COMPACTION_TARGET_ID ECS_COMPACTION_SCHEDULE_EXPRESSION
export ECS_COMPACTION_SCHEDULE_STATE ECS_COMPACTION_LOG_STREAM_PREFIX ECS_COMPACTION_LOCK_PATH
export ECS_COMPACTION_MEMORY_RESERVATION
export ECS_ALB_NAME ECS_TARGET_GROUP_NAME ECS_ALB_SG_NAME ECS_TASK_SG_NAME ECS_LOG_GROUP
export ECS_EXECUTION_ROLE_NAME ECS_TASK_ROLE_NAME ECS_INSTANCE_ROLE_NAME ECS_INSTANCE_PROFILE_NAME
export ECS_INSTANCE_NAME ECS_CPU ECS_MEMORY ECS_DESIRED_COUNT ECS_EC2_INSTANCE_TYPE ECS_CPU_ARCHITECTURE
export ECS_EBS_SIZE_GIB ECS_EBS_VOLUME_TYPE ECS_EBS_IOPS ECS_EBS_THROUGHPUT ECS_EBS_DEVICE_NAME
export IMAGE_URI S3_RUNTIME_PREFIX GIT_CACHE_DISK_MIN_FREE_BYTES GIT_CACHE_DISK_QUOTA_BYTES

ensure_role() {
  local role_name="$1"
  local trust_file="$2"
  local description="$3"

  if aws_cli iam get-role --role-name "$role_name" >/dev/null 2>&1; then
    printf 'using existing IAM role: %s\n' "$role_name"
    aws_cli iam update-assume-role-policy \
      --role-name "$role_name" \
      --policy-document "file://$trust_file" >/dev/null
  else
    printf 'creating IAM role: %s\n' "$role_name"
    aws_cli iam create-role \
      --role-name "$role_name" \
      --assume-role-policy-document "file://$trust_file" \
      --description "$description" >/dev/null
  fi
}

role_arn_by_name() {
  aws_cli iam get-role --role-name "$1" --query Role.Arn --output text
}

ensure_instance_profile() {
  if aws_cli iam get-instance-profile --instance-profile-name "$ECS_INSTANCE_PROFILE_NAME" >/dev/null 2>&1; then
    printf 'using existing instance profile: %s\n' "$ECS_INSTANCE_PROFILE_NAME"
  else
    printf 'creating instance profile: %s\n' "$ECS_INSTANCE_PROFILE_NAME"
    aws_cli iam create-instance-profile --instance-profile-name "$ECS_INSTANCE_PROFILE_NAME" >/dev/null
  fi

  if ! aws_cli iam get-instance-profile \
    --instance-profile-name "$ECS_INSTANCE_PROFILE_NAME" \
    --query "InstanceProfile.Roles[?RoleName=='$ECS_INSTANCE_ROLE_NAME'].RoleName | [0]" \
    --output text | grep -qv '^None$'; then
    aws_cli iam add-role-to-instance-profile \
      --instance-profile-name "$ECS_INSTANCE_PROFILE_NAME" \
      --role-name "$ECS_INSTANCE_ROLE_NAME" >/dev/null
  fi
}

ensure_ecs_roles() {
  cat >"$tmpdir/ecs-tasks-trust.json" <<'JSON'
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ecs-tasks.amazonaws.com"},"Action":"sts:AssumeRole"}]}
JSON
  cat >"$tmpdir/ec2-trust.json" <<'JSON'
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}
JSON

  ensure_role "$ECS_EXECUTION_ROLE_NAME" "$tmpdir/ecs-tasks-trust.json" "ECS task execution role for gitmirrorcache EC2 service"
  ensure_role "$ECS_TASK_ROLE_NAME" "$tmpdir/ecs-tasks-trust.json" "ECS task runtime role for gitmirrorcache EC2 service"
  ensure_role "$ECS_INSTANCE_ROLE_NAME" "$tmpdir/ec2-trust.json" "ECS container instance role for gitmirrorcache"

  aws_cli iam attach-role-policy \
    --role-name "$ECS_EXECUTION_ROLE_NAME" \
    --policy-arn "arn:$AWS_PARTITION:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy" >/dev/null
  aws_cli iam attach-role-policy \
    --role-name "$ECS_INSTANCE_ROLE_NAME" \
    --policy-arn "arn:$AWS_PARTITION:iam::aws:policy/service-role/AmazonEC2ContainerServiceforEC2Role" >/dev/null
  aws_cli iam attach-role-policy \
    --role-name "$ECS_INSTANCE_ROLE_NAME" \
    --policy-arn "arn:$AWS_PARTITION:iam::aws:policy/AmazonSSMManagedInstanceCore" >/dev/null

  python3 - "$tmpdir/ecs-s3-policy.json" <<'PY'
import json
import os
import sys

partition = os.environ["AWS_PARTITION"]
bucket = os.environ["S3_BUCKET"]
prefix = os.environ.get("S3_RUNTIME_PREFIX", os.environ.get("S3_PREFIX", "")).strip("/")
bucket_arn = f"arn:{partition}:s3:::{bucket}"
object_arn = f"{bucket_arn}/{prefix}/*" if prefix else f"{bucket_arn}/*"
list_statement = {
    "Effect": "Allow",
    "Action": ["s3:GetBucketLocation", "s3:ListBucket", "s3:ListBucketMultipartUploads"],
    "Resource": bucket_arn,
}
if prefix:
    list_statement["Condition"] = {"StringLike": {"s3:prefix": [prefix, f"{prefix}/*"]}}
policy = {
    "Version": "2012-10-17",
    "Statement": [
        list_statement,
        {
            "Effect": "Allow",
            "Action": [
                "s3:AbortMultipartUpload",
                "s3:DeleteObject",
                "s3:GetObject",
                "s3:ListMultipartUploadParts",
                "s3:PutObject",
            ],
            "Resource": object_arn,
        },
    ],
}
json.dump(policy, open(sys.argv[1], "w"))
PY
  aws_cli iam put-role-policy \
    --role-name "$ECS_TASK_ROLE_NAME" \
    --policy-name "${NAME_PREFIX}-s3-object-store" \
    --policy-document "file://$tmpdir/ecs-s3-policy.json" >/dev/null

  if [[ -n "${GITHUB_TOKEN_SECRET_ARN:-}" ]]; then
    python3 - "$tmpdir/ecs-secrets-policy.json" <<'PY'
import json
import os
import sys

policy = {
    "Version": "2012-10-17",
    "Statement": [{
        "Effect": "Allow",
        "Action": ["secretsmanager:GetSecretValue"],
        "Resource": os.environ["GITHUB_TOKEN_SECRET_ARN"],
    }],
}
json.dump(policy, open(sys.argv[1], "w"))
PY
    aws_cli iam put-role-policy \
      --role-name "$ECS_EXECUTION_ROLE_NAME" \
      --policy-name "${NAME_PREFIX}-github-token-secret" \
      --policy-document "file://$tmpdir/ecs-secrets-policy.json" >/dev/null
  fi

  ensure_instance_profile
}

default_vpc_id() {
  aws_cli ec2 describe-vpcs \
    --filters Name=is-default,Values=true \
    --query 'Vpcs[0].VpcId' \
    --output text
}

default_subnet_ids() {
  local vpc_id="$1"
  aws_cli ec2 describe-subnets \
    --filters "Name=vpc-id,Values=$vpc_id" Name=default-for-az,Values=true \
    --query 'Subnets[].SubnetId' \
    --output text
}

security_group_id_by_name() {
  local vpc_id="$1"
  local group_name="$2"
  aws_cli ec2 describe-security-groups \
    --filters "Name=vpc-id,Values=$vpc_id" "Name=group-name,Values=$group_name" \
    --query 'SecurityGroups[0].GroupId' \
    --output text
}

ensure_security_group() {
  local vpc_id="$1"
  local group_name="$2"
  local description="$3"
  local group_id
  group_id="$(security_group_id_by_name "$vpc_id" "$group_name")"
  if [[ "$group_id" == "None" || -z "$group_id" ]]; then
    group_id="$(aws_cli ec2 create-security-group \
      --vpc-id "$vpc_id" \
      --group-name "$group_name" \
      --description "$description" \
      --query GroupId \
      --output text)"
    printf 'created security group %s: %s\n' "$group_name" "$group_id" >&2
  else
    printf 'using existing security group %s: %s\n' "$group_name" "$group_id" >&2
  fi
  printf '%s\n' "$group_id"
}

ensure_sg_rule() {
  local direction="$1"
  local group_id="$2"
  shift 2
  if ! aws_cli ec2 "authorize-security-group-$direction" --group-id "$group_id" "$@" >/dev/null 2>&1; then
    true
  fi
}

ensure_cluster() {
  if [[ "$(aws_cli ecs describe-clusters --clusters "$ECS_CLUSTER_NAME" --query 'clusters[0].status' --output text 2>/dev/null)" != "ACTIVE" ]]; then
    printf 'creating ECS cluster: %s\n' "$ECS_CLUSTER_NAME"
    aws_cli ecs create-cluster --cluster-name "$ECS_CLUSTER_NAME" >/dev/null
  else
    printf 'using existing ECS cluster: %s\n' "$ECS_CLUSTER_NAME"
  fi
}

ensure_log_group() {
  if ! aws_cli logs describe-log-groups --log-group-name-prefix "$ECS_LOG_GROUP" \
    --query "logGroups[?logGroupName=='$ECS_LOG_GROUP'].logGroupName | [0]" \
    --output text | grep -qv '^None$'; then
    printf 'creating CloudWatch log group: %s\n' "$ECS_LOG_GROUP"
    aws_cli logs create-log-group --log-group-name "$ECS_LOG_GROUP" >/dev/null
  else
    printf 'using existing CloudWatch log group: %s\n' "$ECS_LOG_GROUP"
  fi
  aws_cli logs put-retention-policy \
    --log-group-name "$ECS_LOG_GROUP" \
    --retention-in-days "${ECS_LOG_RETENTION_DAYS:-14}" >/dev/null
}

load_balancer_arn_by_name() {
  aws_cli elbv2 describe-load-balancers \
    --names "$ECS_ALB_NAME" \
    --query 'LoadBalancers[0].LoadBalancerArn' \
    --output text 2>/dev/null || true
}

target_group_arn_by_name() {
  aws_cli elbv2 describe-target-groups \
    --names "$ECS_TARGET_GROUP_NAME" \
    --query 'TargetGroups[0].TargetGroupArn' \
    --output text 2>/dev/null || true
}

ensure_load_balancer() {
  local vpc_id="$1"
  local subnets_csv="$2"
  local alb_sg_id="$3"
  local tg_arn lb_arn

  tg_arn="$(target_group_arn_by_name)"
  if [[ "$tg_arn" == "None" || -z "$tg_arn" ]]; then
    printf 'creating target group: %s\n' "$ECS_TARGET_GROUP_NAME" >&2
    tg_arn="$(aws_cli elbv2 create-target-group \
      --name "$ECS_TARGET_GROUP_NAME" \
      --protocol HTTP \
      --port 8080 \
      --target-type instance \
      --vpc-id "$vpc_id" \
      --health-check-protocol HTTP \
      --health-check-path /healthz \
      --health-check-interval-seconds 30 \
      --health-check-timeout-seconds 5 \
      --healthy-threshold-count 2 \
      --unhealthy-threshold-count 5 \
      --matcher HttpCode=200 \
      --query 'TargetGroups[0].TargetGroupArn' \
      --output text)"
  else
    printf 'using existing target group: %s\n' "$ECS_TARGET_GROUP_NAME" >&2
  fi

  lb_arn="$(load_balancer_arn_by_name)"
  if [[ "$lb_arn" == "None" || -z "$lb_arn" ]]; then
    printf 'creating ALB: %s\n' "$ECS_ALB_NAME" >&2
    IFS=',' read -r -a subnets <<<"$subnets_csv"
    lb_arn="$(aws_cli elbv2 create-load-balancer \
      --name "$ECS_ALB_NAME" \
      --type application \
      --scheme internet-facing \
      --security-groups "$alb_sg_id" \
      --subnets "${subnets[@]}" \
      --query 'LoadBalancers[0].LoadBalancerArn' \
      --output text)"
    aws_cli elbv2 wait load-balancer-available --load-balancer-arns "$lb_arn"
  else
    printf 'using existing ALB: %s\n' "$ECS_ALB_NAME" >&2
  fi

  aws_cli elbv2 modify-load-balancer-attributes \
    --load-balancer-arn "$lb_arn" \
    --attributes Key=idle_timeout.timeout_seconds,Value="${ECS_ALB_IDLE_TIMEOUT_SECONDS:-3600}" >/dev/null

  listener_arn="$(aws_cli elbv2 describe-listeners \
    --load-balancer-arn "$lb_arn" \
    --query 'Listeners[?Port==`80`].ListenerArn | [0]' \
    --output text)"
  if [[ "$listener_arn" == "None" || -z "$listener_arn" ]]; then
    printf 'creating HTTP listener on ALB\n' >&2
    aws_cli elbv2 create-listener \
      --load-balancer-arn "$lb_arn" \
      --protocol HTTP \
      --port 80 \
      --default-actions Type=forward,TargetGroupArn="$tg_arn" >/dev/null
  else
    aws_cli elbv2 modify-listener \
      --listener-arn "$listener_arn" \
      --default-actions Type=forward,TargetGroupArn="$tg_arn" >/dev/null
  fi

  printf '%s\n%s\n' "$lb_arn" "$tg_arn"
}

alb_dns_name() {
  aws_cli elbv2 describe-load-balancers \
    --names "$ECS_ALB_NAME" \
    --query 'LoadBalancers[0].DNSName' \
    --output text
}

cluster_arn_by_name() {
  aws_cli ecs describe-clusters \
    --clusters "$ECS_CLUSTER_NAME" \
    --query 'clusters[0].clusterArn' \
    --output text
}

build_and_push_image() {
  aws_cli ecr describe-repositories --repository-names "$ECR_REPOSITORY" >/dev/null 2>&1 || die "ECR repository not found; run scripts/aws/bootstrap.sh first"

  if [[ "$ECS_SKIP_DOCKER_BUILD" == "true" ]]; then
    printf 'skipping Docker build; using image: %s\n' "$IMAGE_URI"
    return
  fi

  aws_cli ecr get-login-password | docker login --username AWS --password-stdin "${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"
  docker build --platform "$DOCKER_PLATFORM" -t "$IMAGE_URI" -t "$LATEST_URI" -f "$REPO_ROOT/Dockerfile" "$REPO_ROOT"
  docker push "$IMAGE_URI"
  docker push "$LATEST_URI"
}

ecs_optimized_ami_id() {
  local ami_parameter
  ami_parameter="${ECS_EC2_AMI_PARAMETER:-}"
  if [[ -z "$ami_parameter" ]]; then
    [[ "$ECS_CPU_ARCHITECTURE" == "ARM64" ]] || die "ECS_EC2_AMI_PARAMETER must be set for $ECS_CPU_ARCHITECTURE"
    ami_parameter="/aws/service/ecs/optimized-ami/amazon-linux-2/arm64/recommended/image_id"
  fi

  aws_cli ssm get-parameter \
    --name "$ami_parameter" \
    --query Parameter.Value \
    --output text
}

write_instance_user_data() {
  cat >"$tmpdir/user-data.sh" <<EOF
#!/bin/bash
set -euxo pipefail

systemctl stop ecs || true
mkdir -p /etc/ecs
cat >/etc/ecs/ecs.config <<'ECSCONF'
ECS_CLUSTER=$ECS_CLUSTER_NAME
ECS_IMAGE_PULL_BEHAVIOR=prefer-cached
ECS_ENABLE_TASK_IAM_ROLE=true
ECS_ENABLE_TASK_IAM_ROLE_NETWORK_HOST=true
ECSCONF

device=""
for candidate in /dev/nvme1n1 /dev/xvdf /dev/sdf $ECS_EBS_DEVICE_NAME; do
  if [ -b "\$candidate" ]; then
    device="\$candidate"
    break
  fi
done
for _ in \$(seq 1 120); do
  if [ -n "\$device" ] && [ -b "\$device" ]; then
    break
  fi
  for candidate in /dev/nvme1n1 /dev/xvdf /dev/sdf $ECS_EBS_DEVICE_NAME; do
    if [ -b "\$candidate" ]; then
      device="\$candidate"
      break
    fi
  done
  sleep 1
done
if [ -z "\$device" ]; then
  echo "cache EBS device not found" >&2
  exit 1
fi

if ! blkid "\$device"; then
  mkfs -t ext4 -F "\$device"
fi

mkdir -p /cache
uuid="\$(blkid -s UUID -o value "\$device")"
if ! grep -q "\$uuid" /etc/fstab; then
  echo "UUID=\$uuid /cache ext4 defaults,nofail 0 2" >> /etc/fstab
fi
mount /cache
chmod 0755 /cache
systemctl enable ecs
systemctl start --no-block ecs
EOF
}

instance_id_by_name() {
  aws_cli ec2 describe-instances \
    --filters "Name=tag:Name,Values=$ECS_INSTANCE_NAME" Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId | [0]' \
    --output text
}

ensure_container_instance() {
  local subnet_id="$1"
  local instance_sg_id="$2"
  local instance_id state ami_id

  instance_id="$(instance_id_by_name)"
  if [[ "$instance_id" == "None" || -z "$instance_id" ]]; then
    ami_id="$(ecs_optimized_ami_id)"
    write_instance_user_data
    cat >"$tmpdir/block-device-mappings.json" <<EOF
[
  {
    "DeviceName": "$ECS_EBS_DEVICE_NAME",
    "Ebs": {
      "VolumeSize": $ECS_EBS_SIZE_GIB,
      "VolumeType": "$ECS_EBS_VOLUME_TYPE",
      "Iops": $ECS_EBS_IOPS,
      "Throughput": $ECS_EBS_THROUGHPUT,
      "Encrypted": true,
      "DeleteOnTermination": false
    }
  }
]
EOF
    printf 'launching ECS container instance: %s\n' "$ECS_INSTANCE_NAME" >&2
    instance_id="$(aws_cli ec2 run-instances \
      --image-id "$ami_id" \
      --instance-type "$ECS_EC2_INSTANCE_TYPE" \
      --iam-instance-profile "Name=$ECS_INSTANCE_PROFILE_NAME" \
      --subnet-id "$subnet_id" \
      --security-group-ids "$instance_sg_id" \
      --block-device-mappings "file://$tmpdir/block-device-mappings.json" \
      --user-data "file://$tmpdir/user-data.sh" \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$ECS_INSTANCE_NAME},{Key=App,Value=$APP_NAME},{Key=Environment,Value=$ENVIRONMENT}]" "ResourceType=volume,Tags=[{Key=Name,Value=$ECS_INSTANCE_NAME-cache},{Key=App,Value=$APP_NAME},{Key=Environment,Value=$ENVIRONMENT}]" \
      --query 'Instances[0].InstanceId' \
      --output text)"
  else
    state="$(aws_cli ec2 describe-instances --instance-ids "$instance_id" --query 'Reservations[0].Instances[0].State.Name' --output text)"
    printf 'using existing ECS container instance: %s (%s)\n' "$instance_id" "$state" >&2
    if [[ "$state" == "stopped" ]]; then
      aws_cli ec2 start-instances --instance-ids "$instance_id" >/dev/null
    fi
  fi

  aws_cli ec2 wait instance-running --instance-ids "$instance_id"
  printf 'waiting for ECS container instance registration: %s\n' "$instance_id" >&2
  for attempt in $(seq 1 80); do
    container_instance_arn="$(aws_cli ecs list-container-instances \
      --cluster "$ECS_CLUSTER_NAME" \
      --filter "ec2InstanceId == $instance_id" \
      --query 'containerInstanceArns[0]' \
      --output text)"
    if [[ "$container_instance_arn" != "None" && -n "$container_instance_arn" ]]; then
      status="$(aws_cli ecs describe-container-instances \
        --cluster "$ECS_CLUSTER_NAME" \
        --container-instances "$container_instance_arn" \
        --query 'containerInstances[0].status' \
        --output text)"
      if [[ "$status" == "ACTIVE" ]]; then
        printf '%s\n' "$instance_id"
        return 0
      fi
    fi
    printf 'waiting for ECS registration (%s/80)\n' "$attempt" >&2
    sleep 10
  done

  die "timed out waiting for ECS container instance registration for $instance_id"
}

cache_volume_id_for_instance() {
  local instance_id="$1"
  aws_cli ec2 describe-instances \
    --instance-ids "$instance_id" \
    --query "Reservations[0].Instances[0].BlockDeviceMappings[?DeviceName=='$ECS_EBS_DEVICE_NAME'].Ebs.VolumeId | [0]" \
    --output text
}

ensure_cache_volume_performance() {
  local instance_id="$1"
  local volume_id volume_type iops throughput

  volume_id="$(cache_volume_id_for_instance "$instance_id")"
  if [[ "$volume_id" == "None" || -z "$volume_id" ]]; then
    printf 'cache EBS volume not found for %s; skipping volume performance update\n' "$instance_id" >&2
    return 0
  fi

  read -r volume_type iops throughput < <(aws_cli ec2 describe-volumes \
    --volume-ids "$volume_id" \
    --query 'Volumes[0].[VolumeType,Iops,Throughput]' \
    --output text)

  if [[ "$volume_type" == "$ECS_EBS_VOLUME_TYPE" && "$iops" == "$ECS_EBS_IOPS" && "$throughput" == "$ECS_EBS_THROUGHPUT" ]]; then
    printf 'cache EBS volume already tuned: %s type=%s iops=%s throughput=%s\n' \
      "$volume_id" "$volume_type" "$iops" "$throughput" >&2
    printf '%s\n' "$volume_id"
    return 0
  fi

  printf 'modifying cache EBS volume %s: type=%s iops=%s throughput=%s\n' \
    "$volume_id" "$ECS_EBS_VOLUME_TYPE" "$ECS_EBS_IOPS" "$ECS_EBS_THROUGHPUT" >&2
  aws_cli ec2 modify-volume \
    --volume-id "$volume_id" \
    --volume-type "$ECS_EBS_VOLUME_TYPE" \
    --iops "$ECS_EBS_IOPS" \
    --throughput "$ECS_EBS_THROUGHPUT" >/dev/null
  printf '%s\n' "$volume_id"
}

register_task_definition() {
  local execution_role_arn="$1"
  local task_role_arn="$2"
  local public_base_url="$3"

  PUBLIC_BASE_URL_VALUE="$public_base_url" python3 - "$tmpdir/task-definition.json" <<'PY'
import json
import os
import sys

env = [
    {"name": "AWS_DEFAULT_REGION", "value": os.environ["AWS_REGION"]},
    {"name": "AWS_REGION", "value": os.environ["AWS_REGION"]},
    {"name": "GIT_CACHE_ALLOWED_UPSTREAM_HOSTS", "value": os.environ.get("ALLOWED_UPSTREAM_HOSTS", "github.com")},
    {"name": "GIT_CACHE_BIND_ADDR", "value": "0.0.0.0:8080"},
    {"name": "GIT_CACHE_DISK_MIN_FREE_BYTES", "value": os.environ["GIT_CACHE_DISK_MIN_FREE_BYTES"]},
    {"name": "GIT_CACHE_DISK_QUOTA_BYTES", "value": os.environ["GIT_CACHE_DISK_QUOTA_BYTES"]},
    {"name": "GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "value": os.environ.get("GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "10")},
    {"name": "GIT_CACHE_COMPACTION_INLINE", "value": os.environ.get("GIT_CACHE_COMPACTION_INLINE", "false")},
    {"name": "GIT_CACHE_GIT_REMOTE_ENABLED", "value": os.environ.get("GIT_REMOTE_ENABLED", "true")},
    {"name": "GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH", "value": os.environ.get("GIT_REMOTE_COMMIT_READ_THROUGH", "true")},
    {"name": "GIT_CACHE_GIT_TIMEOUT_SECONDS", "value": os.environ.get("GIT_CACHE_GIT_TIMEOUT_SECONDS", "3600")},
    {"name": "GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES", "value": os.environ.get("GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES", "8")},
    {"name": "GIT_CACHE_MAX_CONCURRENT_GENERATION_VERIFICATIONS", "value": os.environ.get("GIT_CACHE_MAX_CONCURRENT_GENERATION_VERIFICATIONS", "1")},
    {"name": "GIT_CACHE_MAX_GIT_OUTPUT_BYTES", "value": os.environ.get("GIT_CACHE_MAX_GIT_OUTPUT_BYTES", "1073741824")},
    {"name": "GIT_CACHE_OBJECT_STORE_KIND", "value": "s3"},
    {"name": "GIT_CACHE_PUBLIC_BASE_URL", "value": os.environ["PUBLIC_BASE_URL_VALUE"]},
    {"name": "GIT_CACHE_RATE_LIMIT_PER_MINUTE", "value": os.environ.get("GIT_CACHE_RATE_LIMIT_PER_MINUTE", "120")},
    {"name": "GIT_CACHE_ROOT", "value": "/cache"},
    {"name": "GIT_CACHE_S3_BUCKET", "value": os.environ["S3_BUCKET"]},
    {"name": "GIT_CACHE_S3_PREFIX", "value": os.environ["S3_PREFIX"]},
    {"name": "RUST_LOG", "value": os.environ.get("RUST_LOG", "info")},
]
if os.environ.get("S3_ENDPOINT"):
    env.append({"name": "GIT_CACHE_S3_ENDPOINT", "value": os.environ["S3_ENDPOINT"]})
secrets = []
if os.environ.get("GITHUB_TOKEN_SECRET_ARN"):
    env.append({"name": "GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV", "value": "GITHUB_TOKEN"})
    secrets.append({"name": "GITHUB_TOKEN", "valueFrom": os.environ["GITHUB_TOKEN_SECRET_ARN"]})

container = {
    "name": os.environ["ECS_CONTAINER_NAME"],
    "image": os.environ["IMAGE_URI"],
    "essential": True,
    "user": os.environ.get("ECS_CONTAINER_USER", "0"),
    "portMappings": [{
        "containerPort": 8080,
        "hostPort": 8080,
        "protocol": "tcp",
    }],
    "environment": env,
    "mountPoints": [{
        "sourceVolume": os.environ["ECS_CACHE_VOLUME_NAME"],
        "containerPath": "/cache",
        "readOnly": False,
    }],
    "logConfiguration": {
        "logDriver": "awslogs",
        "options": {
            "awslogs-group": os.environ["ECS_LOG_GROUP"],
            "awslogs-region": os.environ["AWS_REGION"],
            "awslogs-stream-prefix": os.environ.get("ECS_LOG_STREAM_PREFIX", "api"),
        },
    },
}
if secrets:
    container["secrets"] = secrets

task = {
    "family": os.environ["ECS_TASK_FAMILY"],
    "taskRoleArn": os.environ["ECS_TASK_ROLE_ARN"],
    "executionRoleArn": os.environ["ECS_EXECUTION_ROLE_ARN"],
    "networkMode": "host",
    "requiresCompatibilities": ["EC2"],
    "cpu": os.environ["ECS_CPU"],
    "memory": os.environ["ECS_MEMORY"],
    "runtimePlatform": {
        "cpuArchitecture": os.environ["ECS_CPU_ARCHITECTURE"],
        "operatingSystemFamily": "LINUX",
    },
    "containerDefinitions": [container],
    "volumes": [{
        "name": os.environ["ECS_CACHE_VOLUME_NAME"],
        "host": {"sourcePath": "/cache"},
    }],
}
json.dump(task, open(sys.argv[1], "w"))
PY

  ECS_TASK_EXECUTION_ROLE_ARN="$execution_role_arn" ECS_TASK_ROLE_ARN="$task_role_arn" \
    aws_cli ecs register-task-definition \
      --cli-input-json "file://$tmpdir/task-definition.json" \
    --query 'taskDefinition.taskDefinitionArn' \
    --output text
}

register_compaction_task_definition() {
  local execution_role_arn="$1"
  local task_role_arn="$2"
  local public_base_url="$3"

  PUBLIC_BASE_URL_VALUE="$public_base_url" python3 - "$tmpdir/compaction-task-definition.json" <<'PY'
import json
import os
import shlex
import sys

env = [
    {"name": "AWS_DEFAULT_REGION", "value": os.environ["AWS_REGION"]},
    {"name": "AWS_REGION", "value": os.environ["AWS_REGION"]},
    {"name": "GIT_CACHE_ALLOWED_UPSTREAM_HOSTS", "value": os.environ.get("ALLOWED_UPSTREAM_HOSTS", "github.com")},
    {"name": "GIT_CACHE_BIND_ADDR", "value": "0.0.0.0:8080"},
    {"name": "GIT_CACHE_DISK_MIN_FREE_BYTES", "value": os.environ["GIT_CACHE_DISK_MIN_FREE_BYTES"]},
    {"name": "GIT_CACHE_DISK_QUOTA_BYTES", "value": os.environ["GIT_CACHE_DISK_QUOTA_BYTES"]},
    {"name": "GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "value": os.environ.get("GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "10")},
    {"name": "GIT_CACHE_COMPACTION_INLINE", "value": os.environ.get("GIT_CACHE_COMPACTION_INLINE", "false")},
    {"name": "GIT_CACHE_GIT_REMOTE_ENABLED", "value": os.environ.get("GIT_REMOTE_ENABLED", "true")},
    {"name": "GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH", "value": os.environ.get("GIT_REMOTE_COMMIT_READ_THROUGH", "true")},
    {"name": "GIT_CACHE_GIT_TIMEOUT_SECONDS", "value": os.environ.get("GIT_CACHE_GIT_TIMEOUT_SECONDS", "3600")},
    {"name": "GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES", "value": os.environ.get("GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES", "8")},
    {"name": "GIT_CACHE_MAX_CONCURRENT_GENERATION_VERIFICATIONS", "value": os.environ.get("GIT_CACHE_MAX_CONCURRENT_GENERATION_VERIFICATIONS", "1")},
    {"name": "GIT_CACHE_MAX_GIT_OUTPUT_BYTES", "value": os.environ.get("GIT_CACHE_MAX_GIT_OUTPUT_BYTES", "1073741824")},
    {"name": "GIT_CACHE_OBJECT_STORE_KIND", "value": "s3"},
    {"name": "GIT_CACHE_PUBLIC_BASE_URL", "value": os.environ["PUBLIC_BASE_URL_VALUE"]},
    {"name": "GIT_CACHE_RATE_LIMIT_PER_MINUTE", "value": os.environ.get("GIT_CACHE_RATE_LIMIT_PER_MINUTE", "120")},
    {"name": "GIT_CACHE_ROOT", "value": "/cache"},
    {"name": "GIT_CACHE_S3_BUCKET", "value": os.environ["S3_BUCKET"]},
    {"name": "GIT_CACHE_S3_PREFIX", "value": os.environ["S3_PREFIX"]},
    {"name": "RUST_LOG", "value": os.environ.get("RUST_LOG", "info")},
]
if os.environ.get("S3_ENDPOINT"):
    env.append({"name": "GIT_CACHE_S3_ENDPOINT", "value": os.environ["S3_ENDPOINT"]})
secrets = []
if os.environ.get("GITHUB_TOKEN_SECRET_ARN"):
    env.append({"name": "GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV", "value": "GITHUB_TOKEN"})
    secrets.append({"name": "GITHUB_TOKEN", "valueFrom": os.environ["GITHUB_TOKEN_SECRET_ARN"]})

lock_path = shlex.quote(os.environ["ECS_COMPACTION_LOCK_PATH"])
script = (
    f"if /usr/bin/flock -n -E 75 {lock_path} /usr/local/bin/git-cache compact --all; then "
    "exit 0; "
    "else status=$?; "
    "if [ \"$status\" -eq 75 ]; then echo 'git-cache compaction already running; skipping'; exit 0; fi; "
    "exit \"$status\"; "
    "fi"
)

container = {
    "name": os.environ["ECS_COMPACTION_CONTAINER_NAME"],
    "image": os.environ["IMAGE_URI"],
    "essential": True,
    "user": os.environ.get("ECS_CONTAINER_USER", "0"),
    "memoryReservation": int(os.environ["ECS_COMPACTION_MEMORY_RESERVATION"]),
    "entryPoint": ["/bin/sh", "-c"],
    "command": [script],
    "environment": env,
    "mountPoints": [{
        "sourceVolume": os.environ["ECS_CACHE_VOLUME_NAME"],
        "containerPath": "/cache",
        "readOnly": False,
    }],
    "logConfiguration": {
        "logDriver": "awslogs",
        "options": {
            "awslogs-group": os.environ["ECS_LOG_GROUP"],
            "awslogs-region": os.environ["AWS_REGION"],
            "awslogs-stream-prefix": os.environ["ECS_COMPACTION_LOG_STREAM_PREFIX"],
        },
    },
}
if secrets:
    container["secrets"] = secrets

task = {
    "family": os.environ["ECS_COMPACTION_TASK_FAMILY"],
    "taskRoleArn": os.environ["ECS_TASK_ROLE_ARN"],
    "executionRoleArn": os.environ["ECS_EXECUTION_ROLE_ARN"],
    "networkMode": "host",
    "requiresCompatibilities": ["EC2"],
    "runtimePlatform": {
        "cpuArchitecture": os.environ["ECS_CPU_ARCHITECTURE"],
        "operatingSystemFamily": "LINUX",
    },
    "containerDefinitions": [container],
    "volumes": [{
        "name": os.environ["ECS_CACHE_VOLUME_NAME"],
        "host": {"sourcePath": "/cache"},
    }],
}
if os.environ.get("ECS_COMPACTION_CPU"):
    task["cpu"] = os.environ["ECS_COMPACTION_CPU"]
if os.environ.get("ECS_COMPACTION_MEMORY"):
    task["memory"] = os.environ["ECS_COMPACTION_MEMORY"]

json.dump(task, open(sys.argv[1], "w"))
PY

  ECS_TASK_EXECUTION_ROLE_ARN="$execution_role_arn" ECS_TASK_ROLE_ARN="$task_role_arn" \
    aws_cli ecs register-task-definition \
      --cli-input-json "file://$tmpdir/compaction-task-definition.json" \
      --query 'taskDefinition.taskDefinitionArn' \
      --output text
}

ensure_compaction_events_role() {
  local compaction_task_definition_arn="$1"
  local execution_role_arn="$2"
  local task_role_arn="$3"

  cat >"$tmpdir/events-trust.json" <<'JSON'
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"events.amazonaws.com"},"Action":"sts:AssumeRole"}]}
JSON

  ensure_role "$ECS_COMPACTION_EVENTS_ROLE_NAME" "$tmpdir/events-trust.json" "EventBridge role for hourly gitmirrorcache compaction" >&2

  ECS_COMPACTION_TASK_DEFINITION_ARN="$compaction_task_definition_arn" \
  ECS_TASK_EXECUTION_ROLE_ARN="$execution_role_arn" \
  ECS_TASK_ROLE_ARN="$task_role_arn" \
  python3 - "$tmpdir/compaction-events-policy.json" <<'PY'
import json
import os
import sys

policy = {
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": "ecs:RunTask",
            "Resource": os.environ["ECS_COMPACTION_TASK_DEFINITION_ARN"],
        },
        {
            "Effect": "Allow",
            "Action": "iam:PassRole",
            "Resource": [
                os.environ["ECS_TASK_EXECUTION_ROLE_ARN"],
                os.environ["ECS_TASK_ROLE_ARN"],
            ],
            "Condition": {
                "StringEquals": {
                    "iam:PassedToService": "ecs-tasks.amazonaws.com",
                },
            },
        },
    ],
}
json.dump(policy, open(sys.argv[1], "w"))
PY

  aws_cli iam put-role-policy \
    --role-name "$ECS_COMPACTION_EVENTS_ROLE_NAME" \
    --policy-name "${NAME_PREFIX}-compaction-run-task" \
    --policy-document "file://$tmpdir/compaction-events-policy.json" >/dev/null

  role_arn_by_name "$ECS_COMPACTION_EVENTS_ROLE_NAME"
}

ensure_compaction_schedule() {
  local compaction_task_definition_arn="$1"
  local compaction_events_role_arn="$2"
  local cluster_arn="$3"

  printf 'upserting hourly compaction rule: %s\n' "$ECS_COMPACTION_RULE_NAME"
  aws_cli events put-rule \
    --name "$ECS_COMPACTION_RULE_NAME" \
    --schedule-expression "$ECS_COMPACTION_SCHEDULE_EXPRESSION" \
    --state "$ECS_COMPACTION_SCHEDULE_STATE" \
    --description "Runs git-cache compact --all for $NAME_PREFIX" >/dev/null

  ECS_COMPACTION_TASK_DEFINITION_ARN="$compaction_task_definition_arn" \
  ECS_COMPACTION_EVENTS_ROLE_ARN="$compaction_events_role_arn" \
  ECS_CLUSTER_ARN="$cluster_arn" \
  python3 - "$tmpdir/compaction-targets.json" <<'PY'
import json
import os
import sys

target = {
    "Id": os.environ["ECS_COMPACTION_TARGET_ID"],
    "Arn": os.environ["ECS_CLUSTER_ARN"],
    "RoleArn": os.environ["ECS_COMPACTION_EVENTS_ROLE_ARN"],
    "EcsParameters": {
        "TaskDefinitionArn": os.environ["ECS_COMPACTION_TASK_DEFINITION_ARN"],
        "TaskCount": 1,
        "LaunchType": "EC2",
        "Group": os.environ.get("ECS_COMPACTION_TASK_GROUP", "git-cache-compaction"),
        "PlacementConstraints": [{
            "type": "memberOf",
            "expression": "attribute:ecs.instance-type == " + os.environ["ECS_EC2_INSTANCE_TYPE"],
        }],
    },
}
json.dump([target], open(sys.argv[1], "w"))
PY

  aws_cli events put-targets \
    --rule "$ECS_COMPACTION_RULE_NAME" \
    --targets "file://$tmpdir/compaction-targets.json" >/dev/null
}

write_service_inputs() {
  local task_definition_arn="$1"
  local task_sg_id="$2"
  local subnets_csv="$3"
  local target_group_arn="$4"

  TASK_DEFINITION_ARN="$task_definition_arn" TARGET_GROUP_ARN="$target_group_arn" python3 - "$tmpdir/create-service.json" "$tmpdir/update-service.json" <<'PY'
import json
import os
import sys

load_balancers = [{
    "containerName": os.environ["ECS_CONTAINER_NAME"],
    "containerPort": 8080,
    "targetGroupArn": os.environ["TARGET_GROUP_ARN"],
}]
base = {
    "cluster": os.environ["ECS_CLUSTER_NAME"],
    "serviceName": os.environ["ECS_SERVICE_NAME"],
    "taskDefinition": os.environ["TASK_DEFINITION_ARN"],
    "desiredCount": int(os.environ["ECS_DESIRED_COUNT"]),
    "launchType": "EC2",
    "loadBalancers": load_balancers,
    "healthCheckGracePeriodSeconds": int(os.environ.get("ECS_HEALTH_CHECK_GRACE_PERIOD_SECONDS", "300")),
    "deploymentConfiguration": {
        "minimumHealthyPercent": int(os.environ.get("ECS_MIN_HEALTHY_PERCENT", "0")),
        "maximumPercent": int(os.environ.get("ECS_MAX_PERCENT", "200")),
    },
    "placementConstraints": [{
        "type": "memberOf",
        "expression": "attribute:ecs.instance-type == " + os.environ["ECS_EC2_INSTANCE_TYPE"],
    }],
    "propagateTags": "SERVICE",
    "tags": [
        {"key": "App", "value": os.environ["APP_NAME"]},
        {"key": "Environment", "value": os.environ["ENVIRONMENT"]},
    ],
}
json.dump(base, open(sys.argv[1], "w"))

update = {
    "cluster": os.environ["ECS_CLUSTER_NAME"],
    "service": os.environ["ECS_SERVICE_NAME"],
    "taskDefinition": os.environ["TASK_DEFINITION_ARN"],
    "desiredCount": int(os.environ["ECS_DESIRED_COUNT"]),
    "forceNewDeployment": True,
    "loadBalancers": load_balancers,
}
json.dump(update, open(sys.argv[2], "w"))
PY
}

service_exists() {
  aws_cli ecs describe-services \
    --cluster "$ECS_CLUSTER_NAME" \
    --services "$ECS_SERVICE_NAME" \
    --query 'services[0].status' \
    --output text 2>/dev/null | grep -Eq '^(ACTIVE|DRAINING)$'
}

ensure_ecs_service() {
  if service_exists; then
    printf 'updating ECS service: %s\n' "$ECS_SERVICE_NAME"
    aws_cli ecs update-service \
      --cli-input-json "file://$tmpdir/update-service.json" >/dev/null
  else
    printf 'creating ECS service: %s\n' "$ECS_SERVICE_NAME"
    aws_cli ecs create-service \
      --cli-input-json "file://$tmpdir/create-service.json" >/dev/null
  fi
  aws_cli ecs wait services-stable --cluster "$ECS_CLUSTER_NAME" --services "$ECS_SERVICE_NAME"
}

ensure_ecs_roles
ensure_cluster
ensure_log_group

vpc_id="${ECS_VPC_ID:-$(default_vpc_id)}"
[[ "$vpc_id" != "None" && -n "$vpc_id" ]] || die "no default VPC found; set ECS_VPC_ID and ECS_SUBNET_IDS"
all_subnets_csv="${ECS_SUBNET_IDS:-$(default_subnet_ids "$vpc_id" | tr '\t' ',')}"
[[ -n "$all_subnets_csv" ]] || die "no default subnets found; set ECS_SUBNET_IDS"
instance_subnet_id="${ECS_EC2_SUBNET_ID:-$(printf '%s' "$all_subnets_csv" | cut -d, -f1)}"

alb_sg_id="$(ensure_security_group "$vpc_id" "$ECS_ALB_SG_NAME" "gitmirrorcache EC2 ECS ALB")"
task_sg_id="$(ensure_security_group "$vpc_id" "$ECS_TASK_SG_NAME" "gitmirrorcache EC2 ECS tasks")"
instance_sg_id="$task_sg_id"
ensure_sg_rule ingress "$alb_sg_id" --ip-permissions 'IpProtocol=tcp,FromPort=80,ToPort=80,IpRanges=[{CidrIp=0.0.0.0/0,Description="HTTP"}]'
ensure_sg_rule ingress "$task_sg_id" --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,UserIdGroupPairs=[{GroupId=$alb_sg_id,Description=\"ALB to API\"}]"

lb_output="$(ensure_load_balancer "$vpc_id" "$all_subnets_csv" "$alb_sg_id")"
load_balancer_arn="$(printf '%s\n' "$lb_output" | sed -n '1p')"
target_group_arn="$(printf '%s\n' "$lb_output" | sed -n '2p')"
public_base_url="${PUBLIC_BASE_URL:-http://$(alb_dns_name)}"

container_instance_id="$(ensure_container_instance "$instance_subnet_id" "$instance_sg_id")"
cache_volume_id="$(ensure_cache_volume_performance "$container_instance_id")"
build_and_push_image

execution_role_arn="$(role_arn_by_name "$ECS_EXECUTION_ROLE_NAME")"
task_role_arn="$(role_arn_by_name "$ECS_TASK_ROLE_NAME")"
export ECS_EXECUTION_ROLE_ARN="$execution_role_arn"
export ECS_TASK_ROLE_ARN="$task_role_arn"

task_definition_arn="$(register_task_definition "$execution_role_arn" "$task_role_arn" "$public_base_url")"
compaction_task_definition_arn="$(register_compaction_task_definition "$execution_role_arn" "$task_role_arn" "$public_base_url")"
write_service_inputs "$task_definition_arn" "$task_sg_id" "$all_subnets_csv" "$target_group_arn"
ensure_ecs_service
cluster_arn="$(cluster_arn_by_name)"
compaction_events_role_arn="$(ensure_compaction_events_role "$compaction_task_definition_arn" "$execution_role_arn" "$task_role_arn")"
ensure_compaction_schedule "$compaction_task_definition_arn" "$compaction_events_role_arn" "$cluster_arn"

cat <<EOF
ECS EC2/EBS deployment complete.
IMAGE_URI=$IMAGE_URI
ECS_CLUSTER_NAME=$ECS_CLUSTER_NAME
ECS_SERVICE_NAME=$ECS_SERVICE_NAME
ECS_TASK_DEFINITION_ARN=$task_definition_arn
ECS_COMPACTION_TASK_DEFINITION_ARN=$compaction_task_definition_arn
ECS_COMPACTION_RULE_NAME=$ECS_COMPACTION_RULE_NAME
ECS_COMPACTION_SCHEDULE_EXPRESSION=$ECS_COMPACTION_SCHEDULE_EXPRESSION
ECS_CONTAINER_INSTANCE_ID=$container_instance_id
ECS_LOAD_BALANCER_ARN=$load_balancer_arn
PUBLIC_BASE_URL=$public_base_url
HEALTH_URL=$public_base_url/healthz
ECS_CACHE_VOLUME_ID=$cache_volume_id
ECS_EBS_SIZE_GIB=$ECS_EBS_SIZE_GIB
ECS_EBS_IOPS=$ECS_EBS_IOPS
ECS_EBS_THROUGHPUT=$ECS_EBS_THROUGHPUT
GIT_CACHE_DISK_QUOTA_BYTES=$GIT_CACHE_DISK_QUOTA_BYTES
S3_RUNTIME_PREFIX=$S3_RUNTIME_PREFIX
EOF
