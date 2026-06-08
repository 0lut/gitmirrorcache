#!/usr/bin/env bash

preview_resolve_version() {
  local requested_ref="${1:-${REF:-HEAD}}"

  if [[ -n "${VERSION_ID:-}" ]]; then
    [[ "$VERSION_ID" =~ ^[0-9a-f]{12,40}$ ]] || die "VERSION_ID must be a 12-40 character lowercase git commit prefix"
    VERSION_ID="${VERSION_ID:0:12}"
    PREVIEW_REF="${REF:-$requested_ref}"
    PREVIEW_COMMIT="${PREVIEW_COMMIT:-}"
    export VERSION_ID PREVIEW_REF PREVIEW_COMMIT
    return 0
  fi

  [[ -n "$requested_ref" ]] || die "preview ref must not be empty"
  [[ "$requested_ref" != -* ]] || die "preview ref must not start with '-'"

  local commit
  commit="$(git -C "$REPO_ROOT" rev-parse --verify --end-of-options "${requested_ref}^{commit}")" \
    || die "could not resolve git ref: $requested_ref"
  VERSION_ID="$(git -C "$REPO_ROOT" rev-parse --short=12 "$commit")" \
    || die "could not derive short commit for: $commit"

  [[ "$VERSION_ID" =~ ^[0-9a-f]{12}$ ]] || die "git returned invalid preview version: $VERSION_ID"
  PREVIEW_REF="$requested_ref"
  PREVIEW_COMMIT="$commit"
  export VERSION_ID PREVIEW_REF PREVIEW_COMMIT
}

preview_configure_identity_defaults() {
  NAME_PREFIX="${NAME_PREFIX:-gmc-p-$VERSION_ID}"
  ENVIRONMENT="${ENVIRONMENT:-preview-$VERSION_ID}"
  S3_PREFIX="${S3_PREFIX:-previews/$VERSION_ID/repos}"
  IMAGE_TAG="${IMAGE_TAG:-$VERSION_ID}"

  export NAME_PREFIX ENVIRONMENT S3_PREFIX IMAGE_TAG
}

preview_configure_shared_infra() {
  local caller_s3_bucket="$1"
  local caller_ecr_repository="$2"
  local shared_name_prefix="${PREVIEW_SHARED_NAME_PREFIX:-gitmirrorcache-arm}"

  if [[ -n "${PREVIEW_S3_BUCKET:-}" ]]; then
    S3_BUCKET="$PREVIEW_S3_BUCKET"
  elif [[ -n "$caller_s3_bucket" ]]; then
    S3_BUCKET="$caller_s3_bucket"
  else
    S3_BUCKET="$shared_name_prefix-$AWS_ACCOUNT_ID-$AWS_REGION"
  fi

  if [[ -n "${PREVIEW_ECR_REPOSITORY:-}" ]]; then
    ECR_REPOSITORY="$PREVIEW_ECR_REPOSITORY"
  elif [[ -n "$caller_ecr_repository" ]]; then
    ECR_REPOSITORY="$caller_ecr_repository"
  else
    ECR_REPOSITORY="$shared_name_prefix"
  fi

  ECR_REPOSITORY_URI="${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com/${ECR_REPOSITORY}"
  export S3_BUCKET ECR_REPOSITORY ECR_REPOSITORY_URI
}

preview_export_resource_defaults() {
  ECS_CLUSTER_NAME="${ECS_CLUSTER_NAME:-$NAME_PREFIX-ec2}"
  ECS_SERVICE_NAME="${ECS_SERVICE_NAME:-$NAME_PREFIX-ec2-api}"
  ECS_TASK_FAMILY="${ECS_TASK_FAMILY:-$NAME_PREFIX-ec2-api}"
  ECS_CONTAINER_NAME="${ECS_CONTAINER_NAME:-git-cache-api}"
  ECS_COMPACTION_TASK_FAMILY="${ECS_COMPACTION_TASK_FAMILY:-$NAME_PREFIX-ec2-compaction}"
  ECS_COMPACTION_EVENTS_ROLE_NAME="${ECS_COMPACTION_EVENTS_ROLE_NAME:-$NAME_PREFIX-ecs-compaction-events}"
  ECS_COMPACTION_RULE_NAME="${ECS_COMPACTION_RULE_NAME:-$NAME_PREFIX-compact-hourly}"
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

  export ECS_CLUSTER_NAME ECS_SERVICE_NAME ECS_TASK_FAMILY ECS_CONTAINER_NAME
  export ECS_COMPACTION_TASK_FAMILY ECS_COMPACTION_EVENTS_ROLE_NAME ECS_COMPACTION_RULE_NAME
  export ECS_ALB_NAME ECS_TARGET_GROUP_NAME ECS_ALB_SG_NAME ECS_TASK_SG_NAME
  export ECS_EXECUTION_ROLE_NAME ECS_TASK_ROLE_NAME ECS_INSTANCE_ROLE_NAME ECS_INSTANCE_PROFILE_NAME
  export ECS_INSTANCE_NAME ECS_LOG_GROUP
}

preview_configure_ingress_defaults() {
  PREVIEW_SHARED_ALB="${PREVIEW_SHARED_ALB:-true}"
  case "$PREVIEW_SHARED_ALB" in
    true | false) ;;
    *) die "PREVIEW_SHARED_ALB must be true or false" ;;
  esac

  PREVIEW_DEDICATED_ALB_NAME="${PREVIEW_DEDICATED_ALB_NAME:-$NAME_PREFIX-ec2-alb}"
  PREVIEW_DEDICATED_ALB_SG_NAME="${PREVIEW_DEDICATED_ALB_SG_NAME:-$NAME_PREFIX-ec2-alb}"

  if [[ "$PREVIEW_SHARED_ALB" == "true" ]]; then
    local shared_name_prefix="${PREVIEW_SHARED_NAME_PREFIX:-gitmirrorcache-arm}"
    local public_path_prefix

    ECS_SHARED_ALB="true"
    ECS_ALB_NAME="${PREVIEW_ALB_NAME:-$shared_name_prefix-preview-alb}"
    ECS_ALB_SG_NAME="${PREVIEW_ALB_SG_NAME:-$ECS_ALB_NAME}"

    public_path_prefix="${ECS_PUBLIC_PATH_PREFIX:-/v/$VERSION_ID}"
    [[ "$public_path_prefix" == /* ]] || public_path_prefix="/$public_path_prefix"
    while [[ "$public_path_prefix" == */ ]]; do
      public_path_prefix="${public_path_prefix%/}"
    done
    ECS_PUBLIC_PATH_PREFIX="$public_path_prefix"
    ECS_ALB_RULE_PATH_PATTERN="${ECS_ALB_RULE_PATH_PATTERN:-$ECS_PUBLIC_PATH_PREFIX/*}"
    ECS_ALB_RULE_REWRITE_REGEX="${ECS_ALB_RULE_REWRITE_REGEX:-^$ECS_PUBLIC_PATH_PREFIX(/.*)$}"
    ECS_ALB_RULE_REWRITE_REPLACE="${ECS_ALB_RULE_REWRITE_REPLACE:-\$1}"
  else
    ECS_SHARED_ALB="${ECS_SHARED_ALB:-false}"
  fi

  export PREVIEW_SHARED_ALB PREVIEW_DEDICATED_ALB_NAME PREVIEW_DEDICATED_ALB_SG_NAME
  export ECS_SHARED_ALB ECS_ALB_NAME ECS_ALB_SG_NAME ECS_PUBLIC_PATH_PREFIX
  export ECS_ALB_RULE_PATH_PATTERN ECS_ALB_RULE_REWRITE_REGEX ECS_ALB_RULE_REWRITE_REPLACE
}

preview_assert_safe_defaults() {
  if [[ "${ALLOW_CUSTOM_PREVIEW_NAMES:-false}" != "true" ]]; then
    [[ "$ENVIRONMENT" == preview-* ]] || die "refusing preview operation outside preview-* environment: $ENVIRONMENT"
    [[ "$S3_PREFIX" == previews/* ]] || die "refusing preview operation outside previews/ S3 prefix: $S3_PREFIX"
  fi

  if [[ "${ECS_SHARED_ALB:-false}" == "true" ]]; then
    [[ "${ECS_PUBLIC_PATH_PREFIX:-}" == /v/"$VERSION_ID" ]] || die "refusing shared preview ALB path outside /v/$VERSION_ID: ${ECS_PUBLIC_PATH_PREFIX:-}"
    [[ "${ECS_ALB_RULE_PATH_PATTERN:-}" == /v/"$VERSION_ID"/* ]] || die "refusing shared preview ALB rule outside /v/$VERSION_ID/*: ${ECS_ALB_RULE_PATH_PATTERN:-}"
  fi

  if [[ -z "${ECS_TARGET_GROUP_NAME_OVERRIDE_OK:-}" && ${#NAME_PREFIX} -gt 19 ]]; then
    die "NAME_PREFIX is too long for default target group names; use 19 characters or fewer"
  fi
}

preview_data_prefix() {
  printf 'previews/%s/' "$VERSION_ID"
}

preview_manifest_key() {
  printf 'deployments/previews/%s.json' "$VERSION_ID"
}
