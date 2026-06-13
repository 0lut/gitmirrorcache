#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context
require_cmd python3

ECS_CLUSTER_NAME="${ECS_CLUSTER_NAME:-$NAME_PREFIX-ec2}"
ECS_TASK_FAMILY="${ECS_TASK_FAMILY:-$NAME_PREFIX-ec2-api}"
ECS_CONTAINER_NAME="${ECS_CONTAINER_NAME:-git-cache-api}"
GIT_CACHE_REPO="${1:-${GIT_CACHE_REPO:-}}"
GIT_CACHE_OBJECT="${2:-${GIT_CACHE_OBJECT:-}}"
GIT_CACHE_SKIP_SLOW="${GIT_CACHE_SKIP_SLOW:-}"

[[ -n "$GIT_CACHE_REPO" ]] || die "usage: $0 host/owner/repo [object-sha]"
[[ "$GIT_CACHE_REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "invalid repo key: $GIT_CACHE_REPO"
if [[ -n "$GIT_CACHE_OBJECT" ]]; then
  [[ "$GIT_CACHE_OBJECT" =~ ^[0-9a-f]{40}$ ]] || die "invalid object sha: $GIT_CACHE_OBJECT"
fi

if [[ -z "${ECS_INSTANCE_ID:-}" ]]; then
  container_instance_arn="$(aws_cli ecs list-container-instances \
    --cluster "$ECS_CLUSTER_NAME" \
    --status ACTIVE \
    --query 'containerInstanceArns[0]' \
    --output text)"
  [[ -n "$container_instance_arn" && "$container_instance_arn" != "None" ]] || die "no active ECS container instance found; set ECS_INSTANCE_ID"
  ECS_INSTANCE_ID="$(aws_cli ecs describe-container-instances \
    --cluster "$ECS_CLUSTER_NAME" \
    --container-instances "$container_instance_arn" \
    --query 'containerInstances[0].ec2InstanceId' \
    --output text)"
fi
[[ "$ECS_INSTANCE_ID" =~ ^i-[a-f0-9]+$ ]] || die "invalid ECS_INSTANCE_ID: $ECS_INSTANCE_ID"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

python3 "$REPO_ROOT/python/aws/ssm_command.py" "$SCRIPT_DIR/ssm/cache-repo-integrity.sh" \
  expected_family="$ECS_TASK_FAMILY" \
  expected_container="$ECS_CONTAINER_NAME" \
  repo="$GIT_CACHE_REPO" \
  object="$GIT_CACHE_OBJECT" \
  skip_slow="$GIT_CACHE_SKIP_SLOW" >"$tmpdir/ssm-parameters.json"

command_id="$(aws_cli ssm send-command \
  --instance-ids "$ECS_INSTANCE_ID" \
  --document-name AWS-RunShellScript \
  --parameters "file://$tmpdir/ssm-parameters.json" \
  --timeout-seconds "${SSM_TIMEOUT_SECONDS:-600}" \
  --query 'Command.CommandId' \
  --output text)"

printf 'SSM_COMMAND_ID=%s\n' "$command_id"
for _ in {1..300}; do
  status="$(aws_cli ssm get-command-invocation \
    --command-id "$command_id" \
    --instance-id "$ECS_INSTANCE_ID" \
    --query 'Status' \
    --output text 2>/dev/null || true)"
  case "$status" in
    Success|Cancelled|TimedOut|Failed|Cancelling)
      break
      ;;
  esac
  sleep 2
done

aws_cli ssm get-command-invocation \
  --command-id "$command_id" \
  --instance-id "$ECS_INSTANCE_ID" \
  --query '{Status:Status,Stdout:StandardOutputContent,Stderr:StandardErrorContent}' \
  --output json

[[ "$status" == "Success" ]] || die "SSM command did not succeed: $status"
