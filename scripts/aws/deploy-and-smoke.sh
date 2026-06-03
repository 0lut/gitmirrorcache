#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context

printf 'Deploying %s to %s/%s with NAME_PREFIX=%s\n' "$APP_NAME" "$AWS_ACCOUNT_ID" "$AWS_REGION" "$NAME_PREFIX"
"$SCRIPT_DIR/deploy-ecs-ec2-ebs.sh"

if [[ "${SKIP_SMOKE_TEST:-false}" == "true" ]]; then
  printf 'Skipping smoke test because SKIP_SMOKE_TEST=true\n'
else
  printf 'Running smoke test for NAME_PREFIX=%s in %s\n' "$NAME_PREFIX" "$AWS_REGION"
  "$SCRIPT_DIR/smoke-test.sh"
fi
