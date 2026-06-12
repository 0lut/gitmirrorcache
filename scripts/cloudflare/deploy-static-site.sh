#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
AWS_SCRIPT_DIR="$(cd -- "$SCRIPT_DIR/../aws" && pwd)"
source "$AWS_SCRIPT_DIR/common.sh"

require_cmd npx

WRANGLER_DRY_RUN="${WRANGLER_DRY_RUN:-false}"
case "$WRANGLER_DRY_RUN" in
  true | false) ;;
  *) die "WRANGLER_DRY_RUN must be true or false" ;;
esac

APP_NAME="${APP_NAME:-gitmirrorcache}"
ENVIRONMENT="${ENVIRONMENT:-prod}"
NAME_PREFIX="${NAME_PREFIX:-gitmirrorcache-prod}"
export APP_NAME ENVIRONMENT NAME_PREFIX

WRANGLER_CONFIG="${WRANGLER_CONFIG:-$REPO_ROOT/wrangler.jsonc}"
ECS_ALB_NAME="${ECS_ALB_NAME:-$NAME_PREFIX-ec2-alb}"

if [[ -z "${API_ORIGIN:-}" ]]; then
  init_aws_context
  alb_dns_name="$(aws_cli elbv2 describe-load-balancers \
    --names "$ECS_ALB_NAME" \
    --query 'LoadBalancers[0].DNSName' \
    --output text)"
  [[ -n "$alb_dns_name" && "$alb_dns_name" != "None" ]] || die "could not resolve ALB DNS name for $ECS_ALB_NAME"
  API_ORIGIN="http://${alb_dns_name%.}"
fi

printf 'Deploying static site with API_ORIGIN=%s\n' "$API_ORIGIN"
args=(deploy --config "$WRANGLER_CONFIG" --var "API_ORIGIN:$API_ORIGIN")
if [[ "$WRANGLER_DRY_RUN" == "true" ]]; then
  args+=(--dry-run)
fi

npx wrangler "${args[@]}"
