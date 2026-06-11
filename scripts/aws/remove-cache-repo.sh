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

[[ -n "$GIT_CACHE_REPO" ]] || die "usage: $0 host/owner/repo"
[[ "$GIT_CACHE_REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "invalid repo key: $GIT_CACHE_REPO"

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

export ECS_TASK_FAMILY ECS_CONTAINER_NAME GIT_CACHE_REPO
python3 >"$tmpdir/ssm-parameters.json" <<'PY'
import json
import os
import shlex

expected_family = shlex.quote(os.environ["ECS_TASK_FAMILY"])
expected_container = shlex.quote(os.environ["ECS_CONTAINER_NAME"])
repo = shlex.quote(os.environ["GIT_CACHE_REPO"])

script = f"""set -euo pipefail
expected_family={expected_family}
expected_container={expected_container}
repo={repo}
repo_dir="/cache/repos/${{repo}}.git"

container_id="$(docker ps \
  --filter "label=com.amazonaws.ecs.task-definition-family=$expected_family" \
  --filter "label=com.amazonaws.ecs.container-name=$expected_container" \
  --format '{{{{.ID}}}}' | head -n1)"

if [ -z "$container_id" ]; then
  echo "no running ECS API container found for $expected_family/$expected_container" >&2
  docker ps -a --format '{{{{.ID}}}} {{{{.Image}}}} {{{{.Names}}}} {{{{.Status}}}}'
  exit 20
fi

family="$(docker inspect --format '{{{{ index .Config.Labels "com.amazonaws.ecs.task-definition-family" }}}}' "$container_id")"
container_name="$(docker inspect --format '{{{{ index .Config.Labels "com.amazonaws.ecs.container-name" }}}}' "$container_id")"
image="$(docker inspect --format '{{{{ .Config.Image }}}}' "$container_id")"

printf 'container: %s\\n' "$container_id"
printf 'image: %s\\n' "$image"
printf 'ecs family: %s\\n' "$family"
printf 'ecs container: %s\\n' "$container_name"
printf 'repo: %s\\n' "$repo"
printf 'repo_dir: %s\\n' "$repo_dir"

if [ "$family" != "$expected_family" ]; then
  echo "refusing removal: expected ECS task family $expected_family, got $family" >&2
  exit 10
fi
if [ "$container_name" != "$expected_container" ]; then
  echo "refusing removal: expected ECS container $expected_container, got $container_name" >&2
  exit 11
fi

if [ -d "$repo_dir" ]; then
  echo '--- before ---'
  du -sh "$repo_dir" || true
  rm -rf -- "$repo_dir"
  echo 'removed local hot-cache repo'
else
  echo 'local hot-cache repo was already absent'
fi

if [ -e "$repo_dir" ]; then
  echo "repo dir still exists after removal: $repo_dir" >&2
  exit 12
fi
echo 'local hot-cache repo absent'
"""

json.dump({"commands": [script]}, open("/dev/stdout", "w"))
PY

command_id="$(aws_cli ssm send-command \
  --instance-ids "$ECS_INSTANCE_ID" \
  --document-name AWS-RunShellScript \
  --parameters "file://$tmpdir/ssm-parameters.json" \
  --timeout-seconds "${SSM_TIMEOUT_SECONDS:-180}" \
  --query 'Command.CommandId' \
  --output text)"

printf 'SSM_COMMAND_ID=%s\n' "$command_id"
for _ in {1..90}; do
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
