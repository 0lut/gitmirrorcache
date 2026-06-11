#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context
require_cmd python3

BOOTSTRAP_FAST_EXISTING="${BOOTSTRAP_FAST_EXISTING:-false}"
case "$BOOTSTRAP_FAST_EXISTING" in
  true | false) ;;
  *) die "BOOTSTRAP_FAST_EXISTING must be true or false" ;;
esac

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

ensure_bucket() {
  if aws_cli s3api head-bucket --bucket "$S3_BUCKET" >/dev/null 2>&1; then
    printf 'using existing S3 bucket: %s\n' "$S3_BUCKET"
    if [[ "$BOOTSTRAP_FAST_EXISTING" == "true" ]]; then
      return 0
    fi
  else
    printf 'creating S3 bucket: %s\n' "$S3_BUCKET"
    if [[ "$AWS_REGION" == "us-east-1" ]]; then
      aws_cli s3api create-bucket --bucket "$S3_BUCKET" >/dev/null
    else
      aws_cli s3api create-bucket \
        --bucket "$S3_BUCKET" \
        --create-bucket-configuration "LocationConstraint=$AWS_REGION" >/dev/null
    fi
  fi

  aws_cli s3api put-public-access-block \
    --bucket "$S3_BUCKET" \
    --public-access-block-configuration BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true >/dev/null

  aws_cli s3api put-bucket-encryption \
    --bucket "$S3_BUCKET" \
    --server-side-encryption-configuration '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"},"BucketKeyEnabled":true}]}' >/dev/null

  if [[ "${S3_ENABLE_VERSIONING:-true}" == "true" ]]; then
    aws_cli s3api put-bucket-versioning \
      --bucket "$S3_BUCKET" \
      --versioning-configuration Status=Enabled >/dev/null
  fi

  cat >"$tmpdir/s3-lifecycle.json" <<'JSON'
{"Rules":[{"ID":"abort-incomplete-multipart-uploads","Status":"Enabled","Filter":{"Prefix":""},"AbortIncompleteMultipartUpload":{"DaysAfterInitiation":7}}]}
JSON
  aws_cli s3api put-bucket-lifecycle-configuration \
    --bucket "$S3_BUCKET" \
    --lifecycle-configuration "file://$tmpdir/s3-lifecycle.json" >/dev/null
}

ensure_ecr_repository() {
  if aws_cli ecr describe-repositories --repository-names "$ECR_REPOSITORY" >/dev/null 2>&1; then
    printf 'using existing ECR repository: %s\n' "$ECR_REPOSITORY"
    if [[ "$BOOTSTRAP_FAST_EXISTING" == "true" ]]; then
      return 0
    fi
  else
    printf 'creating ECR repository: %s\n' "$ECR_REPOSITORY"
    aws_cli ecr create-repository \
      --repository-name "$ECR_REPOSITORY" \
      --image-scanning-configuration scanOnPush=true \
      --encryption-configuration encryptionType=AES256 >/dev/null
  fi

  aws_cli ecr put-image-scanning-configuration \
    --repository-name "$ECR_REPOSITORY" \
    --image-scanning-configuration scanOnPush=true >/dev/null

  python3 "$REPO_ROOT/python/aws/ecr_lifecycle_policy.py" "$tmpdir/ecr-lifecycle.json"
  aws_cli ecr put-lifecycle-policy \
    --repository-name "$ECR_REPOSITORY" \
    --lifecycle-policy-text "file://$tmpdir/ecr-lifecycle.json" >/dev/null
}

ensure_bucket
ensure_ecr_repository

cat <<EOF
AWS bootstrap complete.
S3_BUCKET=$S3_BUCKET
S3_PREFIX=$S3_PREFIX
ECR_REPOSITORY_URI=$ECR_REPOSITORY_URI
EOF
