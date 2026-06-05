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

init_aws_context
preview_configure_shared_infra "$caller_s3_bucket" "$caller_ecr_repository"
preview_assert_safe_defaults

ECR_PUSH_LATEST="${ECR_PUSH_LATEST:-false}"
ECS_EBS_DELETE_ON_TERMINATION="${ECS_EBS_DELETE_ON_TERMINATION:-true}"
ECS_COMPACTION_SCHEDULE_STATE="${ECS_COMPACTION_SCHEDULE_STATE:-DISABLED}"
ECS_LOG_RETENTION_DAYS="${ECS_LOG_RETENTION_DAYS:-3}"
export ECR_PUSH_LATEST ECS_EBS_DELETE_ON_TERMINATION ECS_COMPACTION_SCHEDULE_STATE ECS_LOG_RETENTION_DAYS

printf 'Deploying preview %s from %s\n' "$VERSION_ID" "$PREVIEW_REF"
printf 'NAME_PREFIX=%s\nS3_BUCKET=%s\nS3_PREFIX=%s\nECR_REPOSITORY=%s\n' \
  "$NAME_PREFIX" "$S3_BUCKET" "$S3_PREFIX" "$ECR_REPOSITORY"

if [[ "${SKIP_BOOTSTRAP:-false}" != "true" ]]; then
  "$SCRIPT_DIR/bootstrap.sh"
fi

"$SCRIPT_DIR/deploy-and-smoke.sh"

public_base_url="${PUBLIC_BASE_URL:-$(alb_base_url_by_name "$ECS_ALB_NAME")}"
manifest_key="${PREVIEW_MANIFEST_KEY:-$(preview_manifest_key)}"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

PREVIEW_PUBLIC_BASE_URL="$public_base_url" \
PREVIEW_MANIFEST_KEY="$manifest_key" \
python3 - "$tmpdir/preview-manifest.json" <<'PY'
import json
import os
import sys
from datetime import datetime, timezone

manifest = {
    "version_id": os.environ["VERSION_ID"],
    "ref": os.environ["PREVIEW_REF"],
    "commit": os.environ.get("PREVIEW_COMMIT") or None,
    "deployed_at": datetime.now(timezone.utc).isoformat(),
    "aws_region": os.environ["AWS_REGION"],
    "name_prefix": os.environ["NAME_PREFIX"],
    "environment": os.environ["ENVIRONMENT"],
    "public_base_url": os.environ["PREVIEW_PUBLIC_BASE_URL"],
    "health_url": os.environ["PREVIEW_PUBLIC_BASE_URL"].rstrip("/") + "/healthz",
    "s3_bucket": os.environ["S3_BUCKET"],
    "s3_prefix": os.environ["S3_PREFIX"],
    "ecr_repository": os.environ["ECR_REPOSITORY"],
    "image_tag": os.environ["IMAGE_TAG"],
    "ecs_cluster_name": os.environ["ECS_CLUSTER_NAME"],
    "ecs_service_name": os.environ["ECS_SERVICE_NAME"],
}
json.dump(manifest, open(sys.argv[1], "w"), indent=2)
PY

aws_cli s3 cp "$tmpdir/preview-manifest.json" "s3://$S3_BUCKET/$manifest_key" >/dev/null

cat <<EOF
Preview deployment complete.
VERSION_ID=$VERSION_ID
PUBLIC_BASE_URL=$public_base_url
HEALTH_URL=$public_base_url/healthz
S3_MANIFEST=s3://$S3_BUCKET/$manifest_key
DESTROY_COMMAND=VERSION_ID=$VERSION_ID scripts/aws/destroy-preview.sh
EOF
