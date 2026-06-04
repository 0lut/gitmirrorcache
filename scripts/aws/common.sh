#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

slug() {
  printf '%s' "$1" | tr '[:upper:]_' '[:lower:]-' | tr -cd 'a-z0-9.-'
}

aws_cli() {
  aws "${AWS_ARGS[@]}" "$@"
}

role_arn() {
  printf 'arn:%s:iam::%s:role/%s' "$AWS_PARTITION" "$AWS_ACCOUNT_ID" "$1"
}

init_aws_context() {
  require_cmd aws

  AWS_REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-us-west-2}}"
  AWS_DEFAULT_REGION="$AWS_REGION"
  export AWS_REGION AWS_DEFAULT_REGION

  AWS_ARGS=(--region "$AWS_REGION")
  if [[ -n "${AWS_PROFILE:-}" ]]; then
    AWS_ARGS+=(--profile "$AWS_PROFILE")
  fi

  AWS_ACCOUNT_ID="$(aws_cli sts get-caller-identity --query Account --output text)"
  [[ -n "$AWS_ACCOUNT_ID" && "$AWS_ACCOUNT_ID" != "None" ]] || die "could not resolve AWS account id"

  AWS_PARTITION="${AWS_PARTITION:-aws}"
  APP_NAME="${APP_NAME:-gitmirrorcache}"
  ENVIRONMENT="${ENVIRONMENT:-dev-arm}"
  NAME_PREFIX="${NAME_PREFIX:-gitmirrorcache-arm}"
  [[ -n "$NAME_PREFIX" ]] || die "NAME_PREFIX resolved to empty"

  S3_BUCKET="${S3_BUCKET:-$NAME_PREFIX-$AWS_ACCOUNT_ID-$AWS_REGION}"
  S3_PREFIX="${S3_PREFIX:-repos}"
  ECR_REPOSITORY="${ECR_REPOSITORY:-$NAME_PREFIX}"
  ECR_REPOSITORY_URI="${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com/${ECR_REPOSITORY}"

  [[ ${#S3_BUCKET} -le 63 ]] || die "S3 bucket name is longer than 63 characters: $S3_BUCKET"

  export AWS_ACCOUNT_ID AWS_PARTITION APP_NAME ENVIRONMENT NAME_PREFIX
  export S3_BUCKET S3_PREFIX ECR_REPOSITORY ECR_REPOSITORY_URI
}

alb_base_url_by_name() {
  local alb_name="$1"
  local dns_name
  dns_name="$(aws_cli elbv2 describe-load-balancers \
    --names "$alb_name" \
    --query 'LoadBalancers[0].DNSName' \
    --output text 2>/dev/null || true)"
  [[ -n "$dns_name" && "$dns_name" != "None" ]] || return 1
  printf 'http://%s\n' "$dns_name"
}
