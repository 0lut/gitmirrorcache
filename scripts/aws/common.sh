#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd -- "$SCRIPT_DIR/../.." && pwd)}"

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

timing_now() {
  date +%s
}

timing_record() {
  local name="$1"
  local seconds="$2"
  local status="${3:-0}"

  [[ -n "${DEPLOY_TIMING_FILE:-}" ]] || return 0
  mkdir -p "$(dirname -- "$DEPLOY_TIMING_FILE")" 2>/dev/null || true
  printf '%s\t%s\t%s\n' "$name" "$seconds" "$status" >>"$DEPLOY_TIMING_FILE"
}

timed() {
  local name="$1"
  shift

  local started finished seconds status
  started="$(timing_now)"
  printf '==> %s\n' "$name" >&2
  if "$@"; then
    status=0
  else
    status=$?
  fi
  finished="$(timing_now)"
  seconds=$((finished - started))
  timing_record "$name" "$seconds" "$status"

  if [[ "$status" -eq 0 ]]; then
    printf '<== %s completed in %ss\n' "$name" "$seconds" >&2
  else
    printf '<== %s failed after %ss (exit %s)\n' "$name" "$seconds" "$status" >&2
  fi
  return "$status"
}

timing_print_summary() {
  local file="${1:-${DEPLOY_TIMING_FILE:-}}"
  [[ -n "$file" && -s "$file" ]] || return 0

  printf 'Deployment timings:\n'
  awk -F '\t' '{
    status = ($3 == 0 ? "ok" : "exit " $3)
    printf "  %6ss  %-7s  %s\n", $2, status, $1
  }' "$file"
}

timing_write_github_summary() {
  local title="${1:-Deployment timings}"
  local file="${2:-${DEPLOY_TIMING_FILE:-}}"
  [[ -n "${GITHUB_STEP_SUMMARY:-}" && -n "$file" && -s "$file" ]] || return 0

  {
    printf '## %s\n\n' "$title"
    printf '| Phase | Seconds | Status |\n'
    printf '| --- | ---: | --- |\n'
    while IFS=$'\t' read -r name seconds status; do
      if [[ "$status" == "0" ]]; then
        status="ok"
      else
        status="exit $status"
      fi
      printf '| %s | %s | %s |\n' "$name" "$seconds" "$status"
    done <"$file"
    printf '\n'
  } >>"$GITHUB_STEP_SUMMARY"
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
