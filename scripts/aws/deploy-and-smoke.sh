#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context

timing_tmpdir=""
owns_timing_file=false
if [[ -z "${DEPLOY_TIMING_FILE:-}" ]]; then
  timing_tmpdir="$(mktemp -d)"
  DEPLOY_TIMING_FILE="$timing_tmpdir/deploy-and-smoke-timings.tsv"
  owns_timing_file=true
  cleanup_timing() {
    rm -rf "$timing_tmpdir"
  }
  trap cleanup_timing EXIT
fi
export DEPLOY_TIMING_FILE
deploy_and_smoke_started_at="$(timing_now)"

printf 'Deploying %s to %s/%s with NAME_PREFIX=%s\n' "$APP_NAME" "$AWS_ACCOUNT_ID" "$AWS_REGION" "$NAME_PREFIX"
timed "deploy ECS/EC2 stack" "$SCRIPT_DIR/deploy-ecs-ec2-ebs.sh"

if [[ "${SKIP_SMOKE_TEST:-false}" == "true" ]]; then
  printf 'Skipping smoke test because SKIP_SMOKE_TEST=true\n'
else
  printf 'Running smoke test for NAME_PREFIX=%s in %s\n' "$NAME_PREFIX" "$AWS_REGION"
  timed "smoke test" "$SCRIPT_DIR/smoke-test.sh"
fi

timing_record "deploy and smoke total" "$(( $(timing_now) - deploy_and_smoke_started_at ))" 0

if [[ "$owns_timing_file" == "true" ]]; then
  timing_print_summary
  timing_write_github_summary "Deploy and smoke timings"
fi
