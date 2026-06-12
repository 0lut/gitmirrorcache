#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

ghcr_repository_from_origin() {
  local origin slug
  origin="$(git -C "$REPO_ROOT" remote get-url origin 2>/dev/null || true)"
  case "$origin" in
    git@github.com:*)
      slug="${origin#git@github.com:}"
      ;;
    ssh://git@github.com/*)
      slug="${origin#ssh://git@github.com/}"
      ;;
    https://github.com/*)
      slug="${origin#https://github.com/}"
      ;;
    http://github.com/*)
      slug="${origin#http://github.com/}"
      ;;
    *)
      slug="0lut/gitmirrorcache"
      ;;
  esac
  slug="${slug%.git}"
  slug="$(printf '%s' "$slug" | tr '[:upper:]' '[:lower:]')"
  printf 'ghcr.io/%s\n' "$slug"
}

APP_NAME="${APP_NAME:-gitmirrorcache}"
ENVIRONMENT="${ENVIRONMENT:-prod}"
NAME_PREFIX="${NAME_PREFIX:-gitmirrorcache-prod}"
S3_PREFIX="${S3_PREFIX:-repos}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
GHCR_IMAGE_REPOSITORY="${GHCR_IMAGE_REPOSITORY:-$(ghcr_repository_from_origin)}"
IMAGE_URI="${IMAGE_URI:-$GHCR_IMAGE_REPOSITORY:$IMAGE_TAG}"

ECS_EC2_INSTANCE_TYPE="${ECS_EC2_INSTANCE_TYPE:-m9gd.2xlarge}"
ECS_CACHE_VOLUME_KIND="${ECS_CACHE_VOLUME_KIND:-instance-store}"
ECS_SKIP_DOCKER_BUILD="${ECS_SKIP_DOCKER_BUILD:-true}"
ECR_PUSH_LATEST="${ECR_PUSH_LATEST:-false}"
BOOTSTRAP_SKIP_ECR="${BOOTSTRAP_SKIP_ECR:-true}"
ECS_EBS_DELETE_ON_TERMINATION="${ECS_EBS_DELETE_ON_TERMINATION:-false}"
ECS_COMPACTION_ENABLED="${ECS_COMPACTION_ENABLED:-true}"
ECS_LOG_RETENTION_DAYS="${ECS_LOG_RETENTION_DAYS:-30}"
DOMAIN_NAME="${DOMAIN_NAME:-gitcache.sh}"

export APP_NAME ENVIRONMENT NAME_PREFIX S3_PREFIX IMAGE_TAG GHCR_IMAGE_REPOSITORY IMAGE_URI
export ECS_EC2_INSTANCE_TYPE ECS_CACHE_VOLUME_KIND
export ECS_SKIP_DOCKER_BUILD ECR_PUSH_LATEST BOOTSTRAP_SKIP_ECR
export ECS_EBS_DELETE_ON_TERMINATION ECS_COMPACTION_ENABLED ECS_LOG_RETENTION_DAYS
export DOMAIN_NAME

cat <<EOF
Deploying production target.
ENVIRONMENT=$ENVIRONMENT
NAME_PREFIX=$NAME_PREFIX
IMAGE_URI=$IMAGE_URI
ECS_EC2_INSTANCE_TYPE=$ECS_EC2_INSTANCE_TYPE
ECS_CACHE_VOLUME_KIND=$ECS_CACHE_VOLUME_KIND
DOMAIN_NAME=$DOMAIN_NAME
EOF

if [[ "${SKIP_BOOTSTRAP:-false}" != "true" ]]; then
  "$SCRIPT_DIR/bootstrap.sh"
fi

"$SCRIPT_DIR/deploy-and-smoke.sh"
