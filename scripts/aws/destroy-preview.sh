#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"
source "$SCRIPT_DIR/preview-common.sh"

requested_ref="${1:-${REF:-HEAD}}"
caller_s3_bucket="${S3_BUCKET:-}"
caller_ecr_repository="${ECR_REPOSITORY:-}"

preview_resolve_version "$requested_ref"
preview_configure_identity_defaults
preview_export_resource_defaults
preview_configure_ingress_defaults

init_aws_context
preview_configure_shared_infra "$caller_s3_bucket" "$caller_ecr_repository"
preview_assert_safe_defaults

IMAGE_TAG="${IMAGE_TAG:-$VERSION_ID}"
DELETE_IMAGE="${DELETE_IMAGE:-true}"
DELETE_DATA="${DELETE_DATA:-false}"
export IMAGE_TAG DELETE_IMAGE DELETE_DATA

printf 'Destroying preview %s\n' "$VERSION_ID"
printf 'NAME_PREFIX=%s\nS3_BUCKET=%s\nS3_PREFIX=%s\nDELETE_DATA=%s\nECS_SHARED_ALB=%s\nECS_ALB_NAME=%s\nECS_PUBLIC_PATH_PREFIX=%s\n' \
  "$NAME_PREFIX" "$S3_BUCKET" "$S3_PREFIX" "$DELETE_DATA" \
  "${ECS_SHARED_ALB:-false}" "$ECS_ALB_NAME" "${ECS_PUBLIC_PATH_PREFIX:-}"

delete_event_rule() {
  local targets
  targets="$(aws_cli events list-targets-by-rule \
    --rule "$ECS_COMPACTION_RULE_NAME" \
    --query 'Targets[].Id' \
    --output text 2>/dev/null || true)"
  if [[ -n "$targets" ]]; then
    aws_cli events remove-targets --rule "$ECS_COMPACTION_RULE_NAME" --ids $targets >/dev/null || true
  fi
  aws_cli events delete-rule --name "$ECS_COMPACTION_RULE_NAME" >/dev/null 2>&1 || true
}

cluster_exists() {
  local status
  status="$(aws_cli ecs describe-clusters \
    --clusters "$ECS_CLUSTER_NAME" \
    --query 'clusters[0].status' \
    --output text 2>/dev/null || true)"
  [[ "$status" == "ACTIVE" ]]
}

delete_ecs_service() {
  cluster_exists || return 0
  local status
  status="$(aws_cli ecs describe-services \
    --cluster "$ECS_CLUSTER_NAME" \
    --services "$ECS_SERVICE_NAME" \
    --query 'services[0].status' \
    --output text 2>/dev/null || true)"
  if [[ "$status" == "ACTIVE" || "$status" == "DRAINING" ]]; then
    aws_cli ecs update-service \
      --cluster "$ECS_CLUSTER_NAME" \
      --service "$ECS_SERVICE_NAME" \
      --desired-count 0 >/dev/null 2>&1 || true
    aws_cli ecs delete-service \
      --cluster "$ECS_CLUSTER_NAME" \
      --service "$ECS_SERVICE_NAME" \
      --force >/dev/null 2>&1 || true
    aws_cli ecs wait services-inactive \
      --cluster "$ECS_CLUSTER_NAME" \
      --services "$ECS_SERVICE_NAME" >/dev/null 2>&1 || true
  fi
}

deregister_task_family() {
  local family="$1"
  local task_defs
  task_defs="$(aws_cli ecs list-task-definitions \
    --family-prefix "$family" \
    --status ACTIVE \
    --query 'taskDefinitionArns[]' \
    --output text 2>/dev/null || true)"
  for task_def in $task_defs; do
    aws_cli ecs deregister-task-definition --task-definition "$task_def" >/dev/null 2>&1 || true
  done
}

load_balancer_arn_by_destroy_name() {
  local lb_name="$1"
  aws_cli elbv2 describe-load-balancers \
    --names "$lb_name" \
    --query 'LoadBalancers[0].LoadBalancerArn' \
    --output text 2>/dev/null || true
}

target_group_arn_by_destroy_name() {
  local tg_name="$1"
  aws_cli elbv2 describe-target-groups \
    --names "$tg_name" \
    --query 'TargetGroups[0].TargetGroupArn' \
    --output text 2>/dev/null || true
}

listener_arn_by_destroy_lb_arn() {
  local lb_arn="$1"
  aws_cli elbv2 describe-listeners \
    --load-balancer-arn "$lb_arn" \
    --query 'Listeners[?Port==`80`].ListenerArn | [0]' \
    --output text 2>/dev/null || true
}

listener_rule_arn_by_destroy_path_pattern() {
  local listener_arn="$1"
  local path_pattern="$2"
  local rules_json
  rules_json="$(aws_cli elbv2 describe-rules \
    --listener-arn "$listener_arn" \
    --output json 2>/dev/null || true)"
  RULES_JSON="$rules_json" python3 - "$path_pattern" <<'PY'
import json
import os
import sys

path_pattern = sys.argv[1]
try:
    rules = json.loads(os.environ.get("RULES_JSON") or "{}").get("Rules", [])
except json.JSONDecodeError:
    rules = []
for rule in rules:
    for condition in rule.get("Conditions", []):
        if condition.get("Field") != "path-pattern":
            continue
        values = condition.get("Values") or condition.get("PathPatternConfig", {}).get("Values", [])
        if path_pattern in values:
            print(rule["RuleArn"])
            raise SystemExit(0)
print("None")
PY
}

delete_shared_listener_rule() {
  [[ "${ECS_SHARED_ALB:-false}" == "true" ]] || return 0
  local lb_arn listener_arn rule_arn
  lb_arn="$(load_balancer_arn_by_destroy_name "$ECS_ALB_NAME")"
  [[ -n "$lb_arn" && "$lb_arn" != "None" ]] || return 0
  listener_arn="$(listener_arn_by_destroy_lb_arn "$lb_arn")"
  [[ -n "$listener_arn" && "$listener_arn" != "None" ]] || return 0
  rule_arn="$(listener_rule_arn_by_destroy_path_pattern "$listener_arn" "$ECS_ALB_RULE_PATH_PATTERN")"
  if [[ -n "$rule_arn" && "$rule_arn" != "None" ]]; then
    aws_cli elbv2 delete-rule --rule-arn "$rule_arn" >/dev/null 2>&1 || true
  fi
}

delete_load_balancer_by_name() {
  local lb_name="$1"
  local lb_arn
  lb_arn="$(load_balancer_arn_by_destroy_name "$lb_name")"
  if [[ -n "$lb_arn" && "$lb_arn" != "None" ]]; then
    aws_cli elbv2 delete-load-balancer --load-balancer-arn "$lb_arn" >/dev/null 2>&1 || true
    aws_cli elbv2 wait load-balancers-deleted --load-balancer-arns "$lb_arn" >/dev/null 2>&1 || true
  fi
}

delete_target_group_by_name() {
  local tg_name="$1"
  local tg_arn
  tg_arn="$(target_group_arn_by_destroy_name "$tg_name")"
  if [[ -n "$tg_arn" && "$tg_arn" != "None" ]]; then
    for _ in $(seq 1 12); do
      if aws_cli elbv2 delete-target-group --target-group-arn "$tg_arn" >/dev/null 2>&1; then
        break
      fi
      sleep 10
    done
  fi
}

delete_load_balancer() {
  if [[ "${ECS_SHARED_ALB:-false}" == "true" ]]; then
    delete_shared_listener_rule
    delete_load_balancer_by_name "$PREVIEW_DEDICATED_ALB_NAME"
  else
    delete_load_balancer_by_name "$ECS_ALB_NAME"
  fi

  delete_target_group_by_name "$ECS_TARGET_GROUP_NAME"
}

terminate_instances_and_volumes() {
  local instance_ids volume_ids
  instance_ids="$(aws_cli ec2 describe-instances \
    --filters "Name=tag:Name,Values=$ECS_INSTANCE_NAME" Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId' \
    --output text 2>/dev/null || true)"
  if [[ -n "$instance_ids" ]]; then
    aws_cli ec2 terminate-instances --instance-ids $instance_ids >/dev/null 2>&1 || true
    aws_cli ec2 wait instance-terminated --instance-ids $instance_ids >/dev/null 2>&1 || true
  fi

  volume_ids="$(aws_cli ec2 describe-volumes \
    --filters "Name=tag:Name,Values=$ECS_INSTANCE_NAME-cache" Name=status,Values=available \
    --query 'Volumes[].VolumeId' \
    --output text 2>/dev/null || true)"
  for volume_id in $volume_ids; do
    aws_cli ec2 delete-volume --volume-id "$volume_id" >/dev/null 2>&1 || true
  done
}

delete_cluster() {
  cluster_exists || return 0
  for _ in $(seq 1 12); do
    if aws_cli ecs delete-cluster --cluster "$ECS_CLUSTER_NAME" >/dev/null 2>&1; then
      break
    fi
    sleep 10
  done
}

default_vpc_id_for_destroy() {
  aws_cli ec2 describe-vpcs \
    --filters Name=is-default,Values=true \
    --query 'Vpcs[0].VpcId' \
    --output text 2>/dev/null || true
}

security_group_id_by_destroy_name() {
  local vpc_id="$1"
  local group_name="$2"
  aws_cli ec2 describe-security-groups \
    --filters "Name=vpc-id,Values=$vpc_id" "Name=group-name,Values=$group_name" \
    --query 'SecurityGroups[0].GroupId' \
    --output text 2>/dev/null || true
}

delete_security_group_by_name() {
  local vpc_id="$1"
  local group_name="$2"
  local group_id
  group_id="$(security_group_id_by_destroy_name "$vpc_id" "$group_name")"
  [[ -n "$group_id" && "$group_id" != "None" ]] || return 0
  for _ in $(seq 1 12); do
    if aws_cli ec2 delete-security-group --group-id "$group_id" >/dev/null 2>&1; then
      break
    fi
    sleep 10
  done
}

delete_security_groups() {
  local vpc_id
  vpc_id="${ECS_VPC_ID:-$(default_vpc_id_for_destroy)}"
  [[ -n "$vpc_id" && "$vpc_id" != "None" ]] || return 0
  delete_security_group_by_name "$vpc_id" "$ECS_TASK_SG_NAME"
  if [[ "${ECS_SHARED_ALB:-false}" == "true" ]]; then
    delete_security_group_by_name "$vpc_id" "$PREVIEW_DEDICATED_ALB_SG_NAME"
  else
    delete_security_group_by_name "$vpc_id" "$ECS_ALB_SG_NAME"
  fi
}

delete_role_by_name() {
  local role_name="$1"
  local attached inline
  attached="$(aws_cli iam list-attached-role-policies \
    --role-name "$role_name" \
    --query 'AttachedPolicies[].PolicyArn' \
    --output text 2>/dev/null || true)"
  for policy_arn in $attached; do
    aws_cli iam detach-role-policy --role-name "$role_name" --policy-arn "$policy_arn" >/dev/null 2>&1 || true
  done

  inline="$(aws_cli iam list-role-policies \
    --role-name "$role_name" \
    --query 'PolicyNames[]' \
    --output text 2>/dev/null || true)"
  for policy_name in $inline; do
    aws_cli iam delete-role-policy --role-name "$role_name" --policy-name "$policy_name" >/dev/null 2>&1 || true
  done

  aws_cli iam delete-role --role-name "$role_name" >/dev/null 2>&1 || true
}

delete_iam() {
  local profile_roles
  profile_roles="$(aws_cli iam get-instance-profile \
    --instance-profile-name "$ECS_INSTANCE_PROFILE_NAME" \
    --query 'InstanceProfile.Roles[].RoleName' \
    --output text 2>/dev/null || true)"
  for role_name in $profile_roles; do
    aws_cli iam remove-role-from-instance-profile \
      --instance-profile-name "$ECS_INSTANCE_PROFILE_NAME" \
      --role-name "$role_name" >/dev/null 2>&1 || true
  done
  aws_cli iam delete-instance-profile --instance-profile-name "$ECS_INSTANCE_PROFILE_NAME" >/dev/null 2>&1 || true

  delete_role_by_name "$ECS_COMPACTION_EVENTS_ROLE_NAME"
  delete_role_by_name "$ECS_EXECUTION_ROLE_NAME"
  delete_role_by_name "$ECS_TASK_ROLE_NAME"
  delete_role_by_name "$ECS_INSTANCE_ROLE_NAME"
}

delete_image() {
  [[ "$DELETE_IMAGE" == "true" ]] || return 0
  aws_cli ecr batch-delete-image \
    --repository-name "$ECR_REPOSITORY" \
    --image-ids imageTag="$IMAGE_TAG" >/dev/null 2>&1 || true
}

delete_s3_objects() {
  local manifest_key data_prefix
  manifest_key="${PREVIEW_MANIFEST_KEY:-$(preview_manifest_key)}"
  aws_cli s3 rm "s3://$S3_BUCKET/$manifest_key" >/dev/null 2>&1 || true

  [[ "$DELETE_DATA" == "true" ]] || return 0
  data_prefix="${PREVIEW_DATA_PREFIX:-$(preview_data_prefix)}"
  [[ "$data_prefix" == previews/"$VERSION_ID"/* ]] || die "refusing to delete unexpected data prefix: $data_prefix"
  aws_cli s3 rm "s3://$S3_BUCKET/$data_prefix" --recursive >/dev/null 2>&1 || true
}

delete_event_rule
delete_ecs_service
delete_load_balancer
terminate_instances_and_volumes
deregister_task_family "$ECS_TASK_FAMILY"
deregister_task_family "$ECS_COMPACTION_TASK_FAMILY"
delete_cluster
aws_cli logs delete-log-group --log-group-name "$ECS_LOG_GROUP" >/dev/null 2>&1 || true
delete_security_groups
delete_iam
delete_image
delete_s3_objects

cat <<EOF
Preview destroy complete.
VERSION_ID=$VERSION_ID
NAME_PREFIX=$NAME_PREFIX
DELETE_DATA=$DELETE_DATA
EOF

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    printf '## Preview destroy complete\n\n'
    printf '- Version: `%s`\n' "$VERSION_ID"
    printf '- Name prefix: `%s`\n' "$NAME_PREFIX"
    printf '- Shared ALB: `%s`\n' "${ECS_SHARED_ALB:-false}"
    printf '- Deleted durable preview data: `%s`\n' "$DELETE_DATA"
  } >>"$GITHUB_STEP_SUMMARY"
fi
