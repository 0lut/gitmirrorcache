#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

BASE_URL="${BASE_URL:-https://gitcache.sh}"
GIT_REPO_PATH="${GIT_REPO_PATH:-github.com/torvalds/linux}"
DEPTHS="${DEPTHS:-1 10 50}"
RUN_REPEATS="${RUN_REPEATS:-true}"
AWS_INSPECT="${AWS_INSPECT:-true}"
AWS_LOG_TAIL="${AWS_LOG_TAIL:-true}"
KEEP_WORKDIR="${KEEP_WORKDIR:-false}"
WORKDIR="${WORKDIR:-$(mktemp -d /tmp/gitcache-linux-depths.XXXXXX)}"
AWS_REGION="${AWS_REGION:-us-west-2}"
ENVIRONMENT="${ENVIRONMENT:-prod}"
NAME_PREFIX="${NAME_PREFIX:-gitmirrorcache-prod}"
ECS_CLUSTER_NAME="${ECS_CLUSTER_NAME:-$NAME_PREFIX-ec2}"
ECS_TASK_FAMILY="${ECS_TASK_FAMILY:-$NAME_PREFIX-ec2-api}"
ECS_CONTAINER_NAME="${ECS_CONTAINER_NAME:-git-cache-api}"
ECS_LOG_GROUP="${ECS_LOG_GROUP:-/ecs/$NAME_PREFIX/ec2-api}"
METRICS_URL="${METRICS_URL:-${BASE_URL%/}/metrics}"
REMOTE_URL="${REMOTE_URL:-${BASE_URL%/}/git/${GIT_REPO_PATH}.git}"

cleanup() {
  if [[ "$KEEP_WORKDIR" != "true" ]]; then
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT

require_cmd git
require_cmd curl

printf 'WORKDIR=%s\n' "$WORKDIR"
printf 'REMOTE_URL=%s\n' "$REMOTE_URL"
printf 'DEPTHS=%s\n' "$DEPTHS"

metrics() {
  local label="$1"
  printf '%s\n' "$label"
  curl -fsS "$METRICS_URL"
  printf '\n'
}

clone_one() {
  local label="$1"
  local mode="$2"
  local depth="$3"
  local dir="$WORKDIR/$label"
  local err="$WORKDIR/$label.stderr"
  local started finished elapsed exit_code

  rm -rf "$dir"
  started="$(date +%s)"
  set +e
  if [[ "$mode" == "proxy-off" ]]; then
    git \
      -c protocol.version=2 \
      -c 'http.extraHeader=git-cache-use-proxy-on-miss: false' \
      clone --quiet --single-branch --no-tags --filter=blob:none \
      --no-checkout --depth "$depth" "$REMOTE_URL" "$dir" 2>"$err"
  else
    git \
      -c protocol.version=2 \
      clone --quiet --single-branch --no-tags --filter=blob:none \
      --no-checkout --depth "$depth" "$REMOTE_URL" "$dir" 2>"$err"
  fi
  exit_code=$?
  set -e
  finished="$(date +%s)"
  elapsed=$((finished - started))

  printf 'RESULT label=%s mode=%s depth=%s exit=%s elapsed_s=%s' \
    "$label" "$mode" "$depth" "$exit_code" "$elapsed"
  if [[ "$exit_code" -eq 0 ]]; then
    printf ' head=%s shallow=%s commits=%s pack_size=%s\n' \
      "$(git -C "$dir" rev-parse --short=12 HEAD)" \
      "$(git -C "$dir" rev-parse --is-shallow-repository)" \
      "$(git -C "$dir" rev-list --count HEAD)" \
      "$(git -C "$dir" count-objects -vH | awk -F': ' '/size-pack/ {print $2}')"
  else
    printf '\nSTDERR label=%s\n' "$label"
    tail -80 "$err" || true
  fi
}

run_matrix() {
  local phase="$1"
  local depth
  for depth in $DEPTHS; do
    clone_one "${phase}_default_depth${depth}" default "$depth"
    clone_one "${phase}_proxy_off_depth${depth}" proxy-off "$depth"
  done
}

resolve_instance_id() {
  if [[ -n "${ECS_INSTANCE_ID:-}" ]]; then
    printf '%s\n' "$ECS_INSTANCE_ID"
    return
  fi
  local container_instance_arn
  container_instance_arn="$(aws_cli ecs list-container-instances \
    --cluster "$ECS_CLUSTER_NAME" \
    --status ACTIVE \
    --query 'containerInstanceArns[0]' \
    --output text)"
  [[ -n "$container_instance_arn" && "$container_instance_arn" != "None" ]] \
    || die "no active ECS container instance found; set ECS_INSTANCE_ID"
  aws_cli ecs describe-container-instances \
    --cluster "$ECS_CLUSTER_NAME" \
    --container-instances "$container_instance_arn" \
    --query 'containerInstances[0].ec2InstanceId' \
    --output text
}

send_ssm_script() {
  local script_file="$1"
  local timeout_seconds="${2:-600}"
  local instance_id command_id status tmp
  instance_id="$(resolve_instance_id)"
  [[ "$instance_id" =~ ^i-[a-f0-9]+$ ]] || die "invalid ECS instance id: $instance_id"
  tmp="$(mktemp)"
  python3 "$REPO_ROOT/python/aws/ssm_command.py" "$script_file" \
    expected_family="$ECS_TASK_FAMILY" \
    expected_container="$ECS_CONTAINER_NAME" \
    repo="$GIT_REPO_PATH" >"$tmp"
  command_id="$(aws_cli ssm send-command \
    --instance-ids "$instance_id" \
    --document-name AWS-RunShellScript \
    --parameters "file://$tmp" \
    --timeout-seconds "$timeout_seconds" \
    --query 'Command.CommandId' \
    --output text)"
  rm -f "$tmp"
  printf 'SSM_COMMAND_ID=%s\n' "$command_id"
  for _ in $(seq 1 "$((timeout_seconds / 2))"); do
    status="$(aws_cli ssm get-command-invocation \
      --command-id "$command_id" \
      --instance-id "$instance_id" \
      --query 'Status' \
      --output text 2>/dev/null || true)"
    case "$status" in
      Success|Cancelled|TimedOut|Failed|Cancelling) break ;;
    esac
    sleep 2
  done
  aws_cli ssm get-command-invocation \
    --command-id "$command_id" \
    --instance-id "$instance_id" \
    --query '{Status:Status,Stdout:StandardOutputContent,Stderr:StandardErrorContent}' \
    --output json
}

inspect_hot_cache() {
  [[ "$AWS_INSPECT" == "true" ]] || return 0
  require_cmd aws
  require_cmd python3
  init_aws_context

  local script_file inner_script_b64
  inner_script_b64="$(
    cat <<'INNER' | base64 | tr -d '\n'
set -eu
repo_dir="/cache/repos/${GIT_CACHE_REPO}.git"
echo "REPO=$repo_dir"
if [ ! -d "$repo_dir" ]; then
  echo "repo dir not found"
  exit 0
fi
du -sh "$repo_dir"
git --git-dir="$repo_dir" rev-parse --is-bare-repository
echo "IS_SHALLOW=$(git --git-dir="$repo_dir" rev-parse --is-shallow-repository)"
echo "HEAD=$(git --git-dir="$repo_dir" rev-parse --short=12 HEAD 2>/dev/null || true)"
echo "UPSTREAM_MASTER=$(git --git-dir="$repo_dir" rev-parse --short=12 refs/cache/upstream/heads/master 2>/dev/null || true)"
git --git-dir="$repo_dir" count-objects -vH || true
echo PACKS
find "$repo_dir/objects/pack" -maxdepth 1 -type f \( -name "*.pack" -o -name "*.idx" -o -name "*.promisor" \) \
  -printf "%TY-%Tm-%Td %TH:%TM %s %f\n" | sort | tail -40 || true
if [ -f "$repo_dir/git-cache-partial-hydration" ]; then
  echo PARTIAL_MARKER
  cat "$repo_dir/git-cache-partial-hydration"
else
  echo NO_PARTIAL_MARKER
fi
if [ -f "$repo_dir/shallow" ]; then
  echo "SHALLOW_LINES=$(wc -l < "$repo_dir/shallow")"
  tail -5 "$repo_dir/shallow"
else
  echo NO_SHALLOW_FILE
fi
echo SHOW_REF_TAIL
git --git-dir="$repo_dir" show-ref | tail -30 || true
INNER
  )"
  script_file="$(mktemp)"
  {
    printf "inner_script_b64='%s'\n" "$inner_script_b64"
    cat <<'SCRIPT'
container_id="$(docker ps \
  --filter "label=com.amazonaws.ecs.task-definition-family=$expected_family" \
  --filter "label=com.amazonaws.ecs.container-name=$expected_container" \
  --format '{{.ID}}' | head -n1)"
if [ -z "$container_id" ]; then
  echo "no running ECS API container found" >&2
  docker ps -a --format '{{.ID}} {{.Image}} {{.Names}} {{.Status}}'
  exit 20
fi
repo_dir="/cache/repos/${repo}.git"
echo "CONTAINER=$container_id"
printf '%s' "$inner_script_b64" | base64 -d | docker exec -i -e GIT_CACHE_REPO="$repo" "$container_id" sh
SCRIPT
  } >"$script_file"
  send_ssm_script "$script_file" 600
  rm -f "$script_file"
}

tail_api_logs() {
  [[ "$AWS_LOG_TAIL" == "true" ]] || return 0
  require_cmd aws
  init_aws_context
  printf 'CLOUDWATCH_TAIL log_group=%s\n' "$ECS_LOG_GROUP"
  aws_cli logs tail "$ECS_LOG_GROUP" --since 20m --format short || true
}

metrics METRICS_BEFORE
run_matrix initial
if [[ "$RUN_REPEATS" == "true" ]]; then
  run_matrix repeat
fi
metrics METRICS_AFTER
inspect_hot_cache
tail_api_logs
printf 'WORKDIR=%s\n' "$WORKDIR"
