#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context
require_cmd curl

if [[ -n "${PUBLIC_BASE_URL:-}" ]]; then
  base_url="${PUBLIC_BASE_URL%/}"
else
  alb_name="${ECS_ALB_NAME:-$NAME_PREFIX-ec2-alb}"
  base_url="$(alb_base_url_by_name "$alb_name")" || die "could not resolve ALB $alb_name; set PUBLIC_BASE_URL"
fi

curl -fsS "$base_url/healthz"
printf '\n'
