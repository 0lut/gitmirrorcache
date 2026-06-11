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

owns_timing_file=false
if [[ -z "${DEPLOY_TIMING_FILE:-}" ]]; then
  DEPLOY_TIMING_FILE="$tmpdir/ecs-ec2-ebs-timings.tsv"
  owns_timing_file=true
fi
export DEPLOY_TIMING_FILE
deploy_started_at="$(timing_now)"

ECS_CLUSTER_NAME="${ECS_CLUSTER_NAME:-$NAME_PREFIX-ec2}"
ECS_SERVICE_NAME="${ECS_SERVICE_NAME:-$NAME_PREFIX-ec2-api}"
ECS_TASK_FAMILY="${ECS_TASK_FAMILY:-$NAME_PREFIX-ec2-api}"
ECS_CONTAINER_NAME="${ECS_CONTAINER_NAME:-git-cache-api}"
ECS_COMPACTION_TASK_FAMILY="${ECS_COMPACTION_TASK_FAMILY:-$NAME_PREFIX-ec2-compaction}"
ECS_COMPACTION_CONTAINER_NAME="${ECS_COMPACTION_CONTAINER_NAME:-git-cache-compaction}"
ECS_COMPACTION_ENABLED="${ECS_COMPACTION_ENABLED:-true}"
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
ECS_PRECHECK_VCPU_QUOTA="${ECS_PRECHECK_VCPU_QUOTA:-false}"
ECS_CPU_ARCHITECTURE="${ECS_CPU_ARCHITECTURE:-ARM64}"
ECS_EBS_SIZE_GIB="${ECS_EBS_SIZE_GIB:-128}"
ECS_EBS_VOLUME_TYPE="${ECS_EBS_VOLUME_TYPE:-gp3}"
ECS_EBS_IOPS="${ECS_EBS_IOPS:-8000}"
ECS_EBS_THROUGHPUT="${ECS_EBS_THROUGHPUT:-500}"
ECS_EBS_DEVICE_NAME="${ECS_EBS_DEVICE_NAME:-/dev/xvdf}"
ECS_EBS_DELETE_ON_TERMINATION="${ECS_EBS_DELETE_ON_TERMINATION:-false}"
ECS_SKIP_DOCKER_BUILD="${ECS_SKIP_DOCKER_BUILD:-false}"
ECS_DOCKER_BUILD_NO_CACHE="${ECS_DOCKER_BUILD_NO_CACHE:-false}"
ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS="${ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS:-false}"
ECR_PUSH_LATEST="${ECR_PUSH_LATEST:-true}"
DOCKER_BUILDKIT="${DOCKER_BUILDKIT:-1}"
DOCKER_PLATFORM="${DOCKER_PLATFORM:-linux/arm64}"
ECS_SERVICE_STABLE_TIMEOUT_SECONDS="${ECS_SERVICE_STABLE_TIMEOUT_SECONDS:-900}"
ECS_SERVICE_STABLE_POLL_SECONDS="${ECS_SERVICE_STABLE_POLL_SECONDS:-10}"
ECS_SHARED_ALB="${ECS_SHARED_ALB:-false}"
ECS_PUBLIC_PATH_PREFIX="${ECS_PUBLIC_PATH_PREFIX:-}"
ECS_ALB_RULE_PATH_PATTERN="${ECS_ALB_RULE_PATH_PATTERN:-}"
ECS_ALB_RULE_REWRITE_REGEX="${ECS_ALB_RULE_REWRITE_REGEX:-}"
ECS_ALB_RULE_REWRITE_REPLACE="${ECS_ALB_RULE_REWRITE_REPLACE:-\$1}"
ECS_ALB_DEREGISTRATION_DELAY_SECONDS="${ECS_ALB_DEREGISTRATION_DELAY_SECONDS:-300}"
ECS_CONTAINER_STOP_TIMEOUT_SECONDS="${ECS_CONTAINER_STOP_TIMEOUT_SECONDS:-30}"
IMAGE_TAG="${IMAGE_TAG:-$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || date -u +%Y%m%d%H%M%S)}"
IMAGE_URI="${IMAGE_URI:-${ECR_REPOSITORY_URI}:${IMAGE_TAG}}"
LATEST_URI="${ECR_REPOSITORY_URI}:latest"
DEFAULT_AL2023_ARM64_ECS_AMI_ID="ami-0ac01d3c8b7a34f9d"

case "$ECS_EBS_DELETE_ON_TERMINATION" in
  true | false) ;;
  *) die "ECS_EBS_DELETE_ON_TERMINATION must be true or false" ;;
esac

case "$ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS" in
  true | false) ;;
  *) die "ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS must be true or false" ;;
esac

case "$ECS_DOCKER_BUILD_NO_CACHE" in
  true | false) ;;
  *) die "ECS_DOCKER_BUILD_NO_CACHE must be true or false" ;;
esac

case "$ECS_PRECHECK_VCPU_QUOTA" in
  true | false) ;;
  *) die "ECS_PRECHECK_VCPU_QUOTA must be true or false" ;;
esac

case "$ECS_COMPACTION_ENABLED" in
  true | false) ;;
  *) die "ECS_COMPACTION_ENABLED must be true or false" ;;
esac

case "$ECR_PUSH_LATEST" in
  true | false) ;;
  *) die "ECR_PUSH_LATEST must be true or false" ;;
esac

case "$ECS_SHARED_ALB" in
  true | false) ;;
  *) die "ECS_SHARED_ALB must be true or false" ;;
esac

if [[ -n "$ECS_PUBLIC_PATH_PREFIX" ]]; then
  [[ "$ECS_PUBLIC_PATH_PREFIX" == /* ]] || ECS_PUBLIC_PATH_PREFIX="/$ECS_PUBLIC_PATH_PREFIX"
  while [[ "$ECS_PUBLIC_PATH_PREFIX" == */ ]]; do
    ECS_PUBLIC_PATH_PREFIX="${ECS_PUBLIC_PATH_PREFIX%/}"
  done
fi

if [[ "$ECS_SHARED_ALB" == "true" ]]; then
  [[ -n "$ECS_PUBLIC_PATH_PREFIX" ]] || die "ECS_PUBLIC_PATH_PREFIX is required when ECS_SHARED_ALB=true"
  ECS_ALB_RULE_PATH_PATTERN="${ECS_ALB_RULE_PATH_PATTERN:-$ECS_PUBLIC_PATH_PREFIX/*}"
  ECS_ALB_RULE_REWRITE_REGEX="${ECS_ALB_RULE_REWRITE_REGEX:-^$ECS_PUBLIC_PATH_PREFIX(/.*)$}"
fi

# Single source of truth shared with OBJECT_STORE_SCHEMA_SUFFIX in
# crates/git-cache-domain/src/state.rs (via include_str!).
OBJECT_STORE_SCHEMA_SUFFIX="$(tr -d '[:space:]' < "$REPO_ROOT/crates/git-cache-domain/object-store-schema-suffix")"
if [[ -z "$OBJECT_STORE_SCHEMA_SUFFIX" ]]; then
  echo "object-store-schema-suffix file is empty" >&2
  exit 1
fi

runtime_s3_prefix() {
  local prefix="$1"
  while [[ "$prefix" == /* ]]; do
    prefix="${prefix#/}"
  done
  while [[ "$prefix" == */ ]]; do
    prefix="${prefix%/}"
  done
  if [[ -z "$prefix" ]]; then
    printf '%s\n' "$OBJECT_STORE_SCHEMA_SUFFIX"
    return
  fi
  local component="${prefix##*/}"
  if [[ "$component" == "$OBJECT_STORE_SCHEMA_SUFFIX" || "$component" == *-"$OBJECT_STORE_SCHEMA_SUFFIX" ]]; then
    printf '%s\n' "$prefix"
  elif [[ "$prefix" == */* ]]; then
    printf '%s/%s-%s\n' "${prefix%/*}" "$component" "$OBJECT_STORE_SCHEMA_SUFFIX"
  else
    printf '%s-%s\n' "$prefix" "$OBJECT_STORE_SCHEMA_SUFFIX"
  fi
}

S3_RUNTIME_PREFIX="${S3_RUNTIME_PREFIX:-$(runtime_s3_prefix "$S3_PREFIX")}"

GIT_CACHE_DISK_MIN_FREE_BYTES="${GIT_CACHE_DISK_MIN_FREE_BYTES:-10737418240}"
GIT_CACHE_DISK_QUOTA_BYTES="${GIT_CACHE_DISK_QUOTA_BYTES:-$((ECS_EBS_SIZE_GIB * 1024 * 1024 * 1024))}"
if ((GIT_CACHE_DISK_QUOTA_BYTES < 0)); then
  GIT_CACHE_DISK_QUOTA_BYTES=0
fi

export ECS_CLUSTER_NAME ECS_SERVICE_NAME ECS_TASK_FAMILY ECS_CONTAINER_NAME ECS_CACHE_VOLUME_NAME
export ECS_COMPACTION_TASK_FAMILY ECS_COMPACTION_CONTAINER_NAME ECS_COMPACTION_ENABLED ECS_COMPACTION_EVENTS_ROLE_NAME
export ECS_COMPACTION_RULE_NAME ECS_COMPACTION_TARGET_ID ECS_COMPACTION_SCHEDULE_EXPRESSION
export ECS_COMPACTION_SCHEDULE_STATE ECS_COMPACTION_LOG_STREAM_PREFIX ECS_COMPACTION_LOCK_PATH
export ECS_COMPACTION_MEMORY_RESERVATION
export ECS_ALB_NAME ECS_TARGET_GROUP_NAME ECS_ALB_SG_NAME ECS_TASK_SG_NAME ECS_LOG_GROUP
export ECS_EXECUTION_ROLE_NAME ECS_TASK_ROLE_NAME ECS_INSTANCE_ROLE_NAME ECS_INSTANCE_PROFILE_NAME
export ECS_INSTANCE_NAME ECS_CPU ECS_MEMORY ECS_DESIRED_COUNT ECS_EC2_INSTANCE_TYPE ECS_CPU_ARCHITECTURE
export ECS_PRECHECK_VCPU_QUOTA
export ECS_EBS_SIZE_GIB ECS_EBS_VOLUME_TYPE ECS_EBS_IOPS ECS_EBS_THROUGHPUT ECS_EBS_DEVICE_NAME
export ECS_EBS_DELETE_ON_TERMINATION
export IMAGE_URI S3_RUNTIME_PREFIX GIT_CACHE_DISK_MIN_FREE_BYTES GIT_CACHE_DISK_QUOTA_BYTES
export ECS_SERVICE_STABLE_TIMEOUT_SECONDS ECS_SERVICE_STABLE_POLL_SECONDS
export ECS_SHARED_ALB ECS_PUBLIC_PATH_PREFIX ECS_ALB_RULE_PATH_PATTERN
export ECS_ALB_RULE_REWRITE_REGEX ECS_ALB_RULE_REWRITE_REPLACE
export ECS_ALB_DEREGISTRATION_DELAY_SECONDS
export ECS_CONTAINER_STOP_TIMEOUT_SECONDS

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

  python3 "$REPO_ROOT/python/aws/ecs_s3_policy.py" "$tmpdir/ecs-s3-policy.json"
  aws_cli iam put-role-policy \
    --role-name "$ECS_TASK_ROLE_NAME" \
    --policy-name "${NAME_PREFIX}-s3-object-store" \
    --policy-document "file://$tmpdir/ecs-s3-policy.json" >/dev/null

  if [[ -n "${GITHUB_TOKEN_SECRET_ARN:-}" ]]; then
    python3 "$REPO_ROOT/python/aws/ecs_secrets_policy.py" "$tmpdir/ecs-secrets-policy.json"
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

preflight_ec2_vcpu_quota() {
  [[ "$ECS_PRECHECK_VCPU_QUOTA" == "true" ]] || return 0

  local existing_instance_id quota desired_vcpus running_types used_vcpus projected_vcpus
  existing_instance_id="$(instance_id_by_name)"
  if [[ "$existing_instance_id" != "None" && -n "$existing_instance_id" ]]; then
    printf 'skipping EC2 vCPU quota preflight; reusing instance: %s\n' "$existing_instance_id"
    return 0
  fi

  quota="$(aws_cli service-quotas get-service-quota \
    --service-code ec2 \
    --quota-code L-1216C47A \
    --query 'Quota.Value' \
    --output text 2>/dev/null || true)"
  if [[ -z "$quota" || "$quota" == "None" ]]; then
    printf 'warning: could not read EC2 vCPU quota; continuing without quota preflight\n' >&2
    return 0
  fi

  desired_vcpus="$(aws_cli ec2 describe-instance-types \
    --instance-types "$ECS_EC2_INSTANCE_TYPE" \
    --query 'InstanceTypes[0].VCpuInfo.DefaultVCpus' \
    --output text)"
  [[ "$desired_vcpus" =~ ^[0-9]+$ ]] || die "could not resolve vCPU count for $ECS_EC2_INSTANCE_TYPE"

  running_types="$(aws_cli ec2 describe-instances \
    --filters Name=instance-state-name,Values=pending,running \
    --query 'Reservations[].Instances[].InstanceType' \
    --output text)"
  used_vcpus=0
  if [[ -n "$running_types" ]]; then
    local unique_running_types type_vcpu_rows
    unique_running_types="$(printf '%s\n' $running_types | sort -u | tr '\n' ' ')"
    type_vcpu_rows="$(aws_cli ec2 describe-instance-types \
      --instance-types $unique_running_types \
      --query 'InstanceTypes[].[InstanceType,VCpuInfo.DefaultVCpus]' \
      --output text)"
    for instance_type in $running_types; do
      local type_vcpus
      type_vcpus="$(printf '%s\n' "$type_vcpu_rows" | awk -v type="$instance_type" '$1 == type { print $2; exit }')"
      [[ "$type_vcpus" =~ ^[0-9]+$ ]] || die "could not resolve vCPU count for $instance_type"
      used_vcpus=$((used_vcpus + type_vcpus))
    done
  fi

  projected_vcpus=$((used_vcpus + desired_vcpus))
  python3 "$REPO_ROOT/python/aws/check_vcpu_quota.py" "$used_vcpus" "$desired_vcpus" "$projected_vcpus" "$quota" "$ECS_EC2_INSTANCE_TYPE"
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

listener_rule_arn_by_path_pattern() {
  local listener_arn="$1"
  local path_pattern="$2"
  local rules_json
  rules_json="$(aws_cli elbv2 describe-rules \
    --listener-arn "$listener_arn" \
    --output json)"
  RULES_JSON="$rules_json" python3 "$REPO_ROOT/python/aws/find_listener_rule_arn.py" "$path_pattern"
}

next_listener_rule_priority() {
  local listener_arn="$1"
  local version_seed="${VERSION_ID:-${IMAGE_TAG:-1000}}"
  local rules_json
  rules_json="$(aws_cli elbv2 describe-rules \
    --listener-arn "$listener_arn" \
    --output json)"
  RULES_JSON="$rules_json" python3 "$REPO_ROOT/python/aws/pick_listener_rule_priority.py" "$version_seed"
}

write_shared_listener_rule_inputs() {
  local target_group_arn="$1"

  ECS_LISTENER_TARGET_GROUP_ARN="$target_group_arn" python3 "$REPO_ROOT/python/aws/shared_listener_rule_payloads.py" "$tmpdir/shared-listener-conditions.json" "$tmpdir/shared-listener-actions.json" "$tmpdir/shared-listener-transforms.json"
}

write_shared_listener_default_action() {
  python3 "$REPO_ROOT/python/aws/shared_listener_default_action.py" "$tmpdir/shared-listener-default-action.json"
}

ensure_shared_listener_rule() {
  local listener_arn="$1"
  local target_group_arn="$2"
  local rule_arn priority

  write_shared_listener_rule_inputs "$target_group_arn"
  rule_arn="$(listener_rule_arn_by_path_pattern "$listener_arn" "$ECS_ALB_RULE_PATH_PATTERN")"
  if [[ "$rule_arn" != "None" && -n "$rule_arn" ]]; then
    printf 'updating shared ALB listener rule for %s\n' "$ECS_ALB_RULE_PATH_PATTERN" >&2
    aws_cli elbv2 modify-rule \
      --rule-arn "$rule_arn" \
      --conditions "file://$tmpdir/shared-listener-conditions.json" \
      --actions "file://$tmpdir/shared-listener-actions.json" \
      --transforms "file://$tmpdir/shared-listener-transforms.json" >/dev/null
  else
    priority="$(next_listener_rule_priority "$listener_arn")"
    printf 'creating shared ALB listener rule: %s priority=%s\n' "$ECS_ALB_RULE_PATH_PATTERN" "$priority" >&2
    rule_arn="$(aws_cli elbv2 create-rule \
      --listener-arn "$listener_arn" \
      --priority "$priority" \
      --conditions "file://$tmpdir/shared-listener-conditions.json" \
      --actions "file://$tmpdir/shared-listener-actions.json" \
      --transforms "file://$tmpdir/shared-listener-transforms.json" \
      --query 'Rules[0].RuleArn' \
      --output text)"
  fi

  printf '%s\n' "$rule_arn"
}

ensure_load_balancer() {
  local vpc_id="$1"
  local subnets_csv="$2"
  local alb_sg_id="$3"
  local tg_arn lb_arn listener_arn rule_arn

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
      --health-check-interval-seconds "${ECS_ALB_HEALTH_CHECK_INTERVAL_SECONDS:-30}" \
      --health-check-timeout-seconds "${ECS_ALB_HEALTH_CHECK_TIMEOUT_SECONDS:-5}" \
      --healthy-threshold-count "${ECS_ALB_HEALTHY_THRESHOLD_COUNT:-2}" \
      --unhealthy-threshold-count "${ECS_ALB_UNHEALTHY_THRESHOLD_COUNT:-5}" \
      --matcher HttpCode=200 \
      --query 'TargetGroups[0].TargetGroupArn' \
      --output text)"
  else
    printf 'using existing target group: %s\n' "$ECS_TARGET_GROUP_NAME" >&2
  fi

  aws_cli elbv2 modify-target-group \
    --target-group-arn "$tg_arn" \
    --health-check-protocol HTTP \
    --health-check-path /healthz \
    --health-check-interval-seconds "${ECS_ALB_HEALTH_CHECK_INTERVAL_SECONDS:-30}" \
    --health-check-timeout-seconds "${ECS_ALB_HEALTH_CHECK_TIMEOUT_SECONDS:-5}" \
    --healthy-threshold-count "${ECS_ALB_HEALTHY_THRESHOLD_COUNT:-2}" \
    --unhealthy-threshold-count "${ECS_ALB_UNHEALTHY_THRESHOLD_COUNT:-5}" \
    --matcher HttpCode=200 >/dev/null

  aws_cli elbv2 modify-target-group-attributes \
    --target-group-arn "$tg_arn" \
    --attributes Key=deregistration_delay.timeout_seconds,Value="$ECS_ALB_DEREGISTRATION_DELAY_SECONDS" >/dev/null

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
    if [[ "$ECS_SHARED_ALB" == "true" ]]; then
      write_shared_listener_default_action
      listener_arn="$(aws_cli elbv2 create-listener \
        --load-balancer-arn "$lb_arn" \
        --protocol HTTP \
        --port 80 \
        --default-actions "file://$tmpdir/shared-listener-default-action.json" \
        --query 'Listeners[0].ListenerArn' \
        --output text)"
    else
      listener_arn="$(aws_cli elbv2 create-listener \
        --load-balancer-arn "$lb_arn" \
        --protocol HTTP \
        --port 80 \
        --default-actions Type=forward,TargetGroupArn="$tg_arn" \
        --query 'Listeners[0].ListenerArn' \
        --output text)"
    fi
  else
    if [[ "$ECS_SHARED_ALB" == "true" ]]; then
      write_shared_listener_default_action
      aws_cli elbv2 modify-listener \
        --listener-arn "$listener_arn" \
        --default-actions "file://$tmpdir/shared-listener-default-action.json" >/dev/null
    else
      aws_cli elbv2 modify-listener \
        --listener-arn "$listener_arn" \
        --default-actions Type=forward,TargetGroupArn="$tg_arn" >/dev/null
    fi
  fi

  rule_arn=""
  if [[ "$ECS_SHARED_ALB" == "true" ]]; then
    rule_arn="$(ensure_shared_listener_rule "$listener_arn" "$tg_arn")"
  fi

  printf '%s\n%s\n%s\n' "$lb_arn" "$tg_arn" "$rule_arn"
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

  if [[ "$ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS" == "true" ]] \
    && aws_cli ecr describe-images \
      --repository-name "$ECR_REPOSITORY" \
      --image-ids imageTag="$IMAGE_TAG" >/dev/null 2>&1; then
    printf 'skipping Docker build; image already exists: %s\n' "$IMAGE_URI"
    return
  fi

  aws_cli ecr get-login-password | docker login --username AWS --password-stdin "${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"
  local build_args=(--platform "$DOCKER_PLATFORM")
  local docker_tags=(-t "$IMAGE_URI")
  if [[ "$ECS_DOCKER_BUILD_NO_CACHE" == "true" ]]; then
    build_args+=(--no-cache)
  fi
  if [[ "$ECR_PUSH_LATEST" == "true" ]]; then
    docker_tags+=(-t "$LATEST_URI")
  fi
  DOCKER_BUILDKIT="$DOCKER_BUILDKIT" docker build "${build_args[@]}" "${docker_tags[@]}" -f "$REPO_ROOT/Dockerfile" "$REPO_ROOT"
  docker push "$IMAGE_URI"
  if [[ "$ECR_PUSH_LATEST" == "true" ]]; then
    docker push "$LATEST_URI"
  fi
}

ecs_optimized_ami_id() {
  local ami_id ami_parameter
  ami_id="${ECS_EC2_AMI_ID:-}"
  ami_parameter="${ECS_EC2_AMI_PARAMETER:-}"

  [[ -z "$ami_id" || -z "$ami_parameter" ]] || die "set only one of ECS_EC2_AMI_ID or ECS_EC2_AMI_PARAMETER"
  if [[ -n "$ami_id" ]]; then
    [[ "$ami_id" =~ ^ami-[a-f0-9]+$ ]] || die "invalid ECS_EC2_AMI_ID: $ami_id"
    printf '%s\n' "$ami_id"
    return
  fi

  if [[ -z "$ami_parameter" ]]; then
    [[ "$ECS_CPU_ARCHITECTURE" == "ARM64" ]] || die "ECS_EC2_AMI_ID or ECS_EC2_AMI_PARAMETER must be set for $ECS_CPU_ARCHITECTURE"
    [[ "$AWS_REGION" == "us-west-2" ]] || die "default pinned ECS_EC2_AMI_ID is for us-west-2; set ECS_EC2_AMI_ID or ECS_EC2_AMI_PARAMETER for $AWS_REGION"
    printf '%s\n' "$DEFAULT_AL2023_ARM64_ECS_AMI_ID"
    return
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
      "DeleteOnTermination": $ECS_EBS_DELETE_ON_TERMINATION
    }
  }
]
EOF
    printf 'launching ECS container instance: %s\n' "$ECS_INSTANCE_NAME" >&2
    if ! instance_id="$(aws_cli ec2 run-instances \
      --image-id "$ami_id" \
      --instance-type "$ECS_EC2_INSTANCE_TYPE" \
      --iam-instance-profile "Name=$ECS_INSTANCE_PROFILE_NAME" \
      --subnet-id "$subnet_id" \
      --security-group-ids "$instance_sg_id" \
      --block-device-mappings "file://$tmpdir/block-device-mappings.json" \
      --user-data "file://$tmpdir/user-data.sh" \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$ECS_INSTANCE_NAME},{Key=App,Value=$APP_NAME},{Key=Environment,Value=$ENVIRONMENT}]" "ResourceType=volume,Tags=[{Key=Name,Value=$ECS_INSTANCE_NAME-cache},{Key=App,Value=$APP_NAME},{Key=Environment,Value=$ENVIRONMENT}]" \
      --query 'Instances[0].InstanceId' \
      --output text)"; then
      die "failed to launch ECS container instance: $ECS_INSTANCE_NAME"
    fi
    [[ -n "$instance_id" && "$instance_id" != "None" ]] || die "EC2 launch did not return an instance id for $ECS_INSTANCE_NAME"
  else
    state="$(aws_cli ec2 describe-instances --instance-ids "$instance_id" --query 'Reservations[0].Instances[0].State.Name' --output text)"
    printf 'using existing ECS container instance: %s (%s)\n' "$instance_id" "$state" >&2
    if [[ "$state" == "stopped" ]]; then
      aws_cli ec2 start-instances --instance-ids "$instance_id" >/dev/null
    fi
  fi

  timed "wait EC2 instance running" aws_cli ec2 wait instance-running --instance-ids "$instance_id"
  printf 'waiting for ECS container instance registration: %s\n' "$instance_id" >&2
  local registration_started
  registration_started="$(timing_now)"
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
        timing_record "wait ECS container instance registration" "$(( $(timing_now) - registration_started ))" 0
        printf '%s\n' "$instance_id"
        return 0
      fi
    fi
    printf 'waiting for ECS registration (%s/80)\n' "$attempt" >&2
    sleep 10
  done

  timing_record "wait ECS container instance registration" "$(( $(timing_now) - registration_started ))" 1
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

  python3 "$REPO_ROOT/python/aws/api_task_definition.py" "$tmpdir/task-definition.json"

  ECS_TASK_EXECUTION_ROLE_ARN="$execution_role_arn" ECS_TASK_ROLE_ARN="$task_role_arn" \
    aws_cli ecs register-task-definition \
      --cli-input-json "file://$tmpdir/task-definition.json" \
    --query 'taskDefinition.taskDefinitionArn' \
    --output text
}

register_compaction_task_definition() {
  local execution_role_arn="$1"
  local task_role_arn="$2"

  python3 "$REPO_ROOT/python/aws/compaction_task_definition.py" "$tmpdir/compaction-task-definition.json"

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

  ECS_COMPACTION_TASK_DEFINITION_ARN="$compaction_task_definition_arn" ECS_TASK_EXECUTION_ROLE_ARN="$execution_role_arn" ECS_TASK_ROLE_ARN="$task_role_arn" python3 "$REPO_ROOT/python/aws/compaction_events_policy.py" "$tmpdir/compaction-events-policy.json"

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

  ECS_COMPACTION_TASK_DEFINITION_ARN="$compaction_task_definition_arn" ECS_COMPACTION_EVENTS_ROLE_ARN="$compaction_events_role_arn" ECS_CLUSTER_ARN="$cluster_arn" python3 "$REPO_ROOT/python/aws/compaction_targets.py" "$tmpdir/compaction-targets.json"

  aws_cli events put-targets \
    --rule "$ECS_COMPACTION_RULE_NAME" \
    --targets "file://$tmpdir/compaction-targets.json" >/dev/null
}

write_service_inputs() {
  local task_definition_arn="$1"
  local task_sg_id="$2"
  local subnets_csv="$3"
  local target_group_arn="$4"

  TASK_DEFINITION_ARN="$task_definition_arn" TARGET_GROUP_ARN="$target_group_arn" python3 "$REPO_ROOT/python/aws/ecs_service_payloads.py" "$tmpdir/create-service.json" "$tmpdir/update-service.json"
}

service_exists() {
  aws_cli ecs describe-services \
    --cluster "$ECS_CLUSTER_NAME" \
    --services "$ECS_SERVICE_NAME" \
    --query 'services[0].status' \
    --output text 2>/dev/null | grep -Eq '^(ACTIVE|DRAINING)$'
}

wait_for_ecs_service_stable() {
  local started now elapsed status desired running pending deployment_count primary_rollout
  started="$(timing_now)"

  while true; do
    read -r status desired running pending deployment_count primary_rollout < <(
      aws_cli ecs describe-services \
        --cluster "$ECS_CLUSTER_NAME" \
        --services "$ECS_SERVICE_NAME" \
        --query 'services[0]' \
        --output json |
      python3 "$REPO_ROOT/python/aws/ecs_service_rollout_status.py"
    )

    now="$(timing_now)"
    elapsed=$((now - started))
    if [[ "$status" == "ACTIVE" &&
      "$desired" == "$running" &&
      "$pending" == "0" &&
      "$deployment_count" == "1" &&
      ( "$primary_rollout" == "COMPLETED" || "$primary_rollout" == "UNKNOWN" ) ]]; then
      printf 'ECS service stable after %ss: desired=%s running=%s rollout=%s\n' \
        "$elapsed" "$desired" "$running" "$primary_rollout" >&2
      return 0
    fi

    if (( elapsed >= ECS_SERVICE_STABLE_TIMEOUT_SECONDS )); then
      die "timed out waiting for ECS service stability after ${elapsed}s: status=$status desired=$desired running=$running pending=$pending deployments=$deployment_count rollout=$primary_rollout"
    fi

    printf 'waiting for ECS service stable (%ss/%ss): status=%s desired=%s running=%s pending=%s deployments=%s rollout=%s\n' \
      "$elapsed" "$ECS_SERVICE_STABLE_TIMEOUT_SECONDS" "$status" "$desired" "$running" "$pending" "$deployment_count" "$primary_rollout" >&2
    sleep "$ECS_SERVICE_STABLE_POLL_SECONDS"
  done
}

ensure_ecs_service() {
  if service_exists; then
    printf 'updating ECS service: %s\n' "$ECS_SERVICE_NAME"
    timed "update ECS service" aws_cli ecs update-service \
      --cli-input-json "file://$tmpdir/update-service.json" >/dev/null
  else
    printf 'creating ECS service: %s\n' "$ECS_SERVICE_NAME"
    timed "create ECS service" aws_cli ecs create-service \
      --cli-input-json "file://$tmpdir/create-service.json" >/dev/null
  fi
  timed "wait ECS service stable" wait_for_ecs_service_stable
}

timed "preflight EC2 vCPU quota" preflight_ec2_vcpu_quota
timed "ensure ECS IAM roles" ensure_ecs_roles
timed "ensure ECS cluster" ensure_cluster
timed "ensure CloudWatch log group" ensure_log_group

vpc_id="${ECS_VPC_ID:-$(timed "resolve default VPC" default_vpc_id)}"
[[ "$vpc_id" != "None" && -n "$vpc_id" ]] || die "no default VPC found; set ECS_VPC_ID and ECS_SUBNET_IDS"
all_subnets_csv="${ECS_SUBNET_IDS:-$(timed "resolve default subnets" default_subnet_ids "$vpc_id" | tr '\t' ',')}"
[[ -n "$all_subnets_csv" ]] || die "no default subnets found; set ECS_SUBNET_IDS"
instance_subnet_id="${ECS_EC2_SUBNET_ID:-$(printf '%s' "$all_subnets_csv" | cut -d, -f1)}"

alb_sg_id="$(timed "ensure ALB security group" ensure_security_group "$vpc_id" "$ECS_ALB_SG_NAME" "gitmirrorcache EC2 ECS ALB")"
task_sg_id="$(timed "ensure task security group" ensure_security_group "$vpc_id" "$ECS_TASK_SG_NAME" "gitmirrorcache EC2 ECS tasks")"
instance_sg_id="$task_sg_id"
timed "ensure ALB ingress rule" ensure_sg_rule ingress "$alb_sg_id" --ip-permissions 'IpProtocol=tcp,FromPort=80,ToPort=80,IpRanges=[{CidrIp=0.0.0.0/0,Description="HTTP"}]'
timed "ensure task ingress rule" ensure_sg_rule ingress "$task_sg_id" --ip-permissions "IpProtocol=tcp,FromPort=8080,ToPort=8080,UserIdGroupPairs=[{GroupId=$alb_sg_id,Description=\"ALB to API\"}]"

lb_output="$(timed "ensure load balancer" ensure_load_balancer "$vpc_id" "$all_subnets_csv" "$alb_sg_id")"
load_balancer_arn="$(printf '%s\n' "$lb_output" | sed -n '1p')"
target_group_arn="$(printf '%s\n' "$lb_output" | sed -n '2p')"
listener_rule_arn="$(printf '%s\n' "$lb_output" | sed -n '3p')"
public_base_url="${PUBLIC_BASE_URL:-$(public_base_url_by_alb_name "$ECS_ALB_NAME")}"

container_instance_id="$(timed "ensure EC2/ECS container instance" ensure_container_instance "$instance_subnet_id" "$instance_sg_id")"
cache_volume_id="$(timed "ensure EBS cache volume performance" ensure_cache_volume_performance "$container_instance_id")"
timed "build and push image" build_and_push_image

execution_role_arn="$(timed "resolve execution role ARN" role_arn_by_name "$ECS_EXECUTION_ROLE_NAME")"
task_role_arn="$(timed "resolve task role ARN" role_arn_by_name "$ECS_TASK_ROLE_NAME")"
export ECS_EXECUTION_ROLE_ARN="$execution_role_arn"
export ECS_TASK_ROLE_ARN="$task_role_arn"

task_definition_arn="$(timed "register API task definition" register_task_definition "$execution_role_arn" "$task_role_arn")"
compaction_task_definition_arn=""
if [[ "$ECS_COMPACTION_ENABLED" == "true" ]]; then
  compaction_task_definition_arn="$(timed "register compaction task definition" register_compaction_task_definition "$execution_role_arn" "$task_role_arn")"
fi

timed "write ECS service inputs" write_service_inputs "$task_definition_arn" "$task_sg_id" "$all_subnets_csv" "$target_group_arn"
ensure_ecs_service

if [[ "$ECS_COMPACTION_ENABLED" == "true" ]]; then
  cluster_arn="$(timed "resolve ECS cluster ARN" cluster_arn_by_name)"
  compaction_events_role_arn="$(timed "ensure compaction events role" ensure_compaction_events_role "$compaction_task_definition_arn" "$execution_role_arn" "$task_role_arn")"
  timed "ensure compaction schedule" ensure_compaction_schedule "$compaction_task_definition_arn" "$compaction_events_role_arn" "$cluster_arn"
else
  printf 'skipping compaction task and schedule because ECS_COMPACTION_ENABLED=false\n'
fi

timing_record "ecs/ec2 deployment total" "$(( $(timing_now) - deploy_started_at ))" 0

cat <<EOF
ECS EC2/EBS deployment complete.
IMAGE_URI=$IMAGE_URI
ECS_CLUSTER_NAME=$ECS_CLUSTER_NAME
ECS_SERVICE_NAME=$ECS_SERVICE_NAME
ECS_TASK_DEFINITION_ARN=$task_definition_arn
ECS_COMPACTION_ENABLED=$ECS_COMPACTION_ENABLED
ECS_COMPACTION_TASK_DEFINITION_ARN=$compaction_task_definition_arn
ECS_COMPACTION_RULE_NAME=$ECS_COMPACTION_RULE_NAME
ECS_COMPACTION_SCHEDULE_EXPRESSION=$ECS_COMPACTION_SCHEDULE_EXPRESSION
ECS_CONTAINER_INSTANCE_ID=$container_instance_id
ECS_LOAD_BALANCER_ARN=$load_balancer_arn
ECS_LISTENER_RULE_ARN=$listener_rule_arn
PUBLIC_BASE_URL=$public_base_url
HEALTH_URL=$public_base_url/healthz
ECS_CACHE_VOLUME_ID=$cache_volume_id
ECS_EBS_SIZE_GIB=$ECS_EBS_SIZE_GIB
ECS_EBS_IOPS=$ECS_EBS_IOPS
ECS_EBS_THROUGHPUT=$ECS_EBS_THROUGHPUT
GIT_CACHE_DISK_QUOTA_BYTES=$GIT_CACHE_DISK_QUOTA_BYTES
S3_RUNTIME_PREFIX=$S3_RUNTIME_PREFIX
EOF

if [[ "$owns_timing_file" == "true" ]]; then
  timing_print_summary
  timing_write_github_summary "ECS EC2/EBS deployment timings"
fi
