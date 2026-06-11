#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"
source "$SCRIPT_DIR/preview-common.sh"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

DEPLOY_TIMING_FILE="${DEPLOY_TIMING_FILE:-$tmpdir/preview-timings.tsv}"
export DEPLOY_TIMING_FILE
preview_started_at="$(timing_now)"

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

ECR_PUSH_LATEST="${ECR_PUSH_LATEST:-false}"
ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS="${ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS:-true}"
ECS_EC2_INSTANCE_TYPE="${ECS_EC2_INSTANCE_TYPE:-m8g.2xlarge}"
ECS_PRECHECK_VCPU_QUOTA="${ECS_PRECHECK_VCPU_QUOTA:-true}"
ECS_EBS_DELETE_ON_TERMINATION="${ECS_EBS_DELETE_ON_TERMINATION:-true}"
ECS_COMPACTION_ENABLED="${ECS_COMPACTION_ENABLED:-false}"
ECS_LOG_RETENTION_DAYS="${ECS_LOG_RETENTION_DAYS:-3}"
ECS_ALB_HEALTH_CHECK_INTERVAL_SECONDS="${ECS_ALB_HEALTH_CHECK_INTERVAL_SECONDS:-5}"
ECS_ALB_HEALTH_CHECK_TIMEOUT_SECONDS="${ECS_ALB_HEALTH_CHECK_TIMEOUT_SECONDS:-2}"
ECS_ALB_HEALTHY_THRESHOLD_COUNT="${ECS_ALB_HEALTHY_THRESHOLD_COUNT:-2}"
ECS_ALB_UNHEALTHY_THRESHOLD_COUNT="${ECS_ALB_UNHEALTHY_THRESHOLD_COUNT:-3}"
ECS_HEALTH_CHECK_GRACE_PERIOD_SECONDS="${ECS_HEALTH_CHECK_GRACE_PERIOD_SECONDS:-15}"
ECS_SERVICE_STABLE_POLL_SECONDS="${ECS_SERVICE_STABLE_POLL_SECONDS:-5}"
BOOTSTRAP_FAST_EXISTING="${BOOTSTRAP_FAST_EXISTING:-true}"
export ECR_PUSH_LATEST ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS ECS_EC2_INSTANCE_TYPE
export ECS_PRECHECK_VCPU_QUOTA ECS_EBS_DELETE_ON_TERMINATION
export ECS_COMPACTION_ENABLED ECS_LOG_RETENTION_DAYS
export ECS_ALB_HEALTH_CHECK_INTERVAL_SECONDS ECS_ALB_HEALTH_CHECK_TIMEOUT_SECONDS
export ECS_ALB_HEALTHY_THRESHOLD_COUNT ECS_ALB_UNHEALTHY_THRESHOLD_COUNT
export ECS_HEALTH_CHECK_GRACE_PERIOD_SECONDS ECS_SERVICE_STABLE_POLL_SECONDS
export BOOTSTRAP_FAST_EXISTING

printf 'Deploying preview %s from %s\n' "$VERSION_ID" "$PREVIEW_REF"
printf 'NAME_PREFIX=%s\nS3_BUCKET=%s\nS3_PREFIX=%s\nECR_REPOSITORY=%s\nECS_SHARED_ALB=%s\nECS_ALB_NAME=%s\nECS_PUBLIC_PATH_PREFIX=%s\n' \
  "$NAME_PREFIX" "$S3_BUCKET" "$S3_PREFIX" "$ECR_REPOSITORY" \
  "${ECS_SHARED_ALB:-false}" "$ECS_ALB_NAME" "${ECS_PUBLIC_PATH_PREFIX:-}"

if [[ "${SKIP_BOOTSTRAP:-false}" != "true" ]]; then
  timed "preview bootstrap" "$SCRIPT_DIR/bootstrap.sh"
fi

timed "preview deploy and smoke" "$SCRIPT_DIR/deploy-and-smoke.sh"

public_base_url="${PUBLIC_BASE_URL:-$(public_base_url_by_alb_name "$ECS_ALB_NAME")}"
manifest_key="${PREVIEW_MANIFEST_KEY:-$(preview_manifest_key)}"

write_preview_manifest() {
  PREVIEW_PUBLIC_BASE_URL="$public_base_url" PREVIEW_MANIFEST_KEY="$manifest_key" python3 "$REPO_ROOT/python/aws/preview_manifest.py" "$tmpdir/preview-manifest.json"

  aws_cli s3 cp "$tmpdir/preview-manifest.json" "s3://$S3_BUCKET/$manifest_key" >/dev/null
}

timed "write preview manifest" write_preview_manifest
timing_record "preview deployment total" "$(( $(timing_now) - preview_started_at ))" 0

cat <<EOF
Preview deployment complete.
VERSION_ID=$VERSION_ID
PUBLIC_BASE_URL=$public_base_url
HEALTH_URL=$public_base_url/healthz
S3_MANIFEST=s3://$S3_BUCKET/$manifest_key
DESTROY_COMMAND=VERSION_ID=$VERSION_ID scripts/aws/destroy-preview.sh
EOF

timing_print_summary

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    printf '## Preview deployment complete\n\n'
    printf '- Version: `%s`\n' "$VERSION_ID"
    printf '- Ref: `%s`\n' "$PREVIEW_REF"
    if [[ -n "${PREVIEW_COMMIT:-}" ]]; then
      printf '- Commit: `%s`\n' "$PREVIEW_COMMIT"
    fi
    printf '- URL: %s\n' "$public_base_url"
    printf '- Health: %s/healthz\n' "$public_base_url"
    printf '- Manifest: `s3://%s/%s`\n\n' "$S3_BUCKET" "$manifest_key"
    printf 'To tear this down, run `VERSION_ID=%s scripts/aws/destroy-preview.sh`.\n\n' "$VERSION_ID"
  } >>"$GITHUB_STEP_SUMMARY"
  timing_write_github_summary "Preview deployment timings"
fi
