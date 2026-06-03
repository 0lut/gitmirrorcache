#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context
require_cmd python3

ECS_CLUSTER_NAME="${ECS_CLUSTER_NAME:-$NAME_PREFIX-ec2}"
ECS_SERVICE_NAME="${ECS_SERVICE_NAME:-$NAME_PREFIX-ec2-api}"
ECS_TASK_FAMILY="${ECS_TASK_FAMILY:-$NAME_PREFIX-ec2-api}"
ECS_CONTAINER_NAME="${ECS_CONTAINER_NAME:-git-cache-api}"
ECS_HOST_PORT="${ECS_HOST_PORT:-8080}"

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

if [[ -z "${ECS_STALE_CONTAINER_ID:-}" ]]; then
  cat >&2 <<EOF
ECS_STALE_CONTAINER_ID is required.

Inspect candidate containers with:
  AWS_REGION=$AWS_REGION ENVIRONMENT=$ENVIRONMENT NAME_PREFIX=$NAME_PREFIX ECS_INSTANCE_ID=$ECS_INSTANCE_ID \\
  scripts/aws/ecs-host-diagnostics.sh
EOF
  exit 2
fi
[[ "$ECS_STALE_CONTAINER_ID" =~ ^[a-fA-F0-9]{12,64}$ ]] || die "invalid ECS_STALE_CONTAINER_ID: $ECS_STALE_CONTAINER_ID"
[[ "${CONFIRM_STOP:-false}" == "true" ]] || die "refusing to stop container without CONFIRM_STOP=true"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

export ECS_STALE_CONTAINER_ID ECS_TASK_FAMILY ECS_CONTAINER_NAME ECS_HOST_PORT
python3 >"$tmpdir/ssm-parameters.json" <<'PY'
import json
import os
import shlex

container_id = shlex.quote(os.environ["ECS_STALE_CONTAINER_ID"])
expected_family = shlex.quote(os.environ["ECS_TASK_FAMILY"])
expected_container = shlex.quote(os.environ["ECS_CONTAINER_NAME"])
host_port = shlex.quote(os.environ["ECS_HOST_PORT"])

script = f"""set -euo pipefail
container_id={container_id}
expected_family={expected_family}
expected_container={expected_container}
host_port={host_port}

docker inspect "$container_id" >/dev/null
family="$(docker inspect --format '{{{{ index .Config.Labels "com.amazonaws.ecs.task-definition-family" }}}}' "$container_id")"
container_name="$(docker inspect --format '{{{{ index .Config.Labels "com.amazonaws.ecs.container-name" }}}}' "$container_id")"
running="$(docker inspect --format '{{{{ .State.Running }}}}' "$container_id")"
image="$(docker inspect --format '{{{{ .Config.Image }}}}' "$container_id")"

printf 'candidate container: %s\\n' "$container_id"
printf 'image: %s\\n' "$image"
printf 'ecs family: %s\\n' "$family"
printf 'ecs container: %s\\n' "$container_name"
printf 'running: %s\\n' "$running"
printf 'listeners on :%s before stop:\\n' "$host_port"
sudo ss -ltnp | grep ":$host_port" || true

if [ "$family" != "$expected_family" ]; then
  echo "refusing to stop: expected ECS task family $expected_family, got $family" >&2
  exit 10
fi
if [ "$container_name" != "$expected_container" ]; then
  echo "refusing to stop: expected ECS container $expected_container, got $container_name" >&2
  exit 11
fi
if [ "$running" != "true" ]; then
  echo "container is not running; nothing to stop"
  exit 0
fi

docker stop "$container_id"
printf 'listeners on :%s after stop:\\n' "$host_port"
sudo ss -ltnp | grep ":$host_port" || true
"""

json.dump({"commands": [script]}, open("/dev/stdout", "w"))
PY

command_id="$(aws_cli ssm send-command \
  --instance-ids "$ECS_INSTANCE_ID" \
  --document-name AWS-RunShellScript \
  --parameters "file://$tmpdir/ssm-parameters.json" \
  --query 'Command.CommandId' \
  --output text)"

printf 'SSM_COMMAND_ID=%s\n' "$command_id"
for _ in {1..60}; do
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
