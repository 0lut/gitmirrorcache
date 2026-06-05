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
ECS_HOST_PORT="${ECS_HOST_PORT:-8080}"

if [[ -n "$GIT_CACHE_REPO" ]]; then
  [[ "$GIT_CACHE_REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "invalid repo key: $GIT_CACHE_REPO"
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

export ECS_TASK_FAMILY ECS_CONTAINER_NAME GIT_CACHE_REPO ECS_HOST_PORT
python3 >"$tmpdir/ssm-parameters.json" <<'PY'
import json
import os
import shlex

expected_family = shlex.quote(os.environ["ECS_TASK_FAMILY"])
expected_container = shlex.quote(os.environ["ECS_CONTAINER_NAME"])
repo = shlex.quote(os.environ.get("GIT_CACHE_REPO", ""))
host_port = shlex.quote(os.environ["ECS_HOST_PORT"])

script = f"""set -euo pipefail
expected_family={expected_family}
expected_container={expected_container}
repo={repo}
host_port={host_port}

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
running="$(docker inspect --format '{{{{ .State.Running }}}}' "$container_id")"

printf 'container: %s\\n' "$container_id"
printf 'image: %s\\n' "$image"
printf 'ecs family: %s\\n' "$family"
printf 'ecs container: %s\\n' "$container_name"
printf 'running: %s\\n' "$running"

if [ "$family" != "$expected_family" ]; then
  echo "refusing diagnostics: expected ECS task family $expected_family, got $family" >&2
  exit 10
fi
if [ "$container_name" != "$expected_container" ]; then
  echo "refusing diagnostics: expected ECS container $expected_container, got $container_name" >&2
  exit 11
fi

echo
echo '--- host disk ---'
df -h /cache || true
df -i /cache || true
du -sh /cache /cache/* 2>/dev/null | sort -h || true
echo
echo '--- cache tree top levels ---'
find /cache -maxdepth 2 -mindepth 1 -type d -printf '%p\\n' 2>/dev/null | sort | head -120 || true
echo
echo '--- largest cache directories ---'
du -xh --max-depth=2 /cache 2>/dev/null | sort -h | tail -40 || true

echo
echo '--- docker stats ---'
docker stats --no-stream "$container_id" || true

echo
echo '--- listeners ---'
sudo ss -ltnp | grep ":$host_port" || true

echo
echo '--- established network involving git/cache processes ---'
sudo ss -tnp | grep -E 'git|cache|:443|:80' || true

echo
echo '--- host process tree for cache/git ---'
ps -eo pid,ppid,stat,etime,pcpu,pmem,args | grep -E 'git-cache|git |git$|upload-pack|index-pack|pack-objects|ls-remote|fetch' | grep -v grep || true

echo
echo '--- container process tree ---'
docker top "$container_id" -eo pid,ppid,stat,etime,pcpu,pmem,args || true

echo
echo '--- recent cache repo sizes ---'
find /cache/repos /cache/tmp -mindepth 3 -maxdepth 4 -type d -name '*.git' -printf '%T@ %p\\n' 2>/dev/null \\
  | sort -nr \\
  | head -20 \\
  | while read -r _ path; do du -sh "$path" 2>/dev/null || true; done

if [ -n "$repo" ]; then
  repo_dir="/cache/repos/${{repo}}.git"
  echo
  printf '%s\\n' "--- repo diagnostics: $repo ---"
  if [ -d "$repo_dir" ]; then
    du -sh "$repo_dir" || true
    find "$repo_dir" -maxdepth 2 -type f -printf '%p %s\\n' 2>/dev/null | sort | head -80 || true
    docker exec "$container_id" git -C "$repo_dir" status --short --branch || true
    docker exec "$container_id" git -C "$repo_dir" count-objects -vH || true
    docker exec "$container_id" git -C "$repo_dir" show-ref --heads | head -20 || true
    docker exec "$container_id" git -C "$repo_dir" for-each-ref --count=20 --sort=-committerdate --format='%(refname) %(objectname)' refs/heads refs/cache 2>/dev/null || true
  else
    printf 'repo dir not found: %s\\n' "$repo_dir"
  fi
fi
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
