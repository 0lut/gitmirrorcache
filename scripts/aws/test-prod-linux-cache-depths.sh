#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

# Exercises the direct-Git shallow/blobless depth + deepen paths against a live
# deployment (prod by default) and inspects the hot cache afterwards. Covers the
# PR #123 depth matrix and the PR #126 stale-`shallow.lock` deepen regression:
# heavy proxy-off `git fetch --deepen=N` over an existing shallow boundary used
# to poison the repo with an orphaned `shallow.lock` and return persistent 503s.
#
# Heavy cold deepens of a huge repo (linux at depth 40/80) take minutes of
# cache-side work. Through the public Cloudflare front door that can exceed the
# ~100s origin timeout and surface as HTTP 524 (a CDN timeout, not a cache
# failure). To measure the cache itself, point BASE_URL at the ALB origin, e.g.
#   BASE_URL=http://<name-prefix>-ec2-alb-<id>.<region>.elb.amazonaws.com
#
# For a no-cache baseline, point the matrix straight at the upstream and turn
# off the cache-only steps (the proxy-on-miss header is meaningless to GitHub):
#   REMOTE_URL=https://github.com/torvalds/linux.git MODES=default \
#   RUN_METRICS=false AWS_INSPECT=false AWS_LOG_TAIL=false \
#   scripts/aws/test-prod-linux-cache-depths.sh
#
# Exit status is non-zero if any clone/deepen fails or any HTTP 5xx is seen.

BASE_URL="${BASE_URL:-https://gitcache.sh}"
GIT_REPO_PATH="${GIT_REPO_PATH:-github.com/torvalds/linux}"
# `|`-separated list of independent-clone depth orders. Order matters: it
# changes whether each clone hits an already-deep cache or forces a deepen.
DEPTH_ORDERS="${DEPTH_ORDERS:-${DEPTHS:-1 10 50|50 10 1|10 1 50|1 50 10}}"
# Ascending `git fetch --deepen=N` increments applied over one shallow clone.
# These are RELATIVE and accumulate, so `1 9 40` walks the boundary to ~depth
# 51 (the exact range that 503'd on 0.0.8). Keep the total modest: on a huge
# merge-heavy repo like linux, deepening past ~depth 50 fans out across
# subsystem-merge parents into hundreds of thousands of commits and minutes of
# cache-side work per step (proxy-off does that work in the cache, not upstream).
DEEPEN_STEPS="${DEEPEN_STEPS:-1 9 40}"
# Initial clone depth before the ascending deepen sequence.
DEEPEN_BASE_DEPTH="${DEEPEN_BASE_DEPTH:-1}"
MODES="${MODES:-default proxy-off}"
RUN_DEPTH_MATRIX="${RUN_DEPTH_MATRIX:-true}"
RUN_DEEPEN_MATRIX="${RUN_DEEPEN_MATRIX:-true}"
RUN_REPEATS="${RUN_REPEATS:-true}"
RUN_METRICS="${RUN_METRICS:-true}"
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

FAILURES=0

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
printf 'DEPTH_ORDERS=%s\n' "$DEPTH_ORDERS"
printf 'DEEPEN_STEPS=%s (base depth %s)\n' "$DEEPEN_STEPS" "$DEEPEN_BASE_DEPTH"
printf 'MODES=%s\n' "$MODES"

metrics() {
  [[ "$RUN_METRICS" == "true" ]] || return 0
  local label="$1"
  printf '%s\n' "$label"
  curl -fsS "$METRICS_URL" || printf '(metrics unavailable)\n'
  printf '\n'
}

# Common per-request git config for one mode. proxy-off forces the local
# read-through (cache-fill) path instead of the cold-miss upstream proxy.
mode_git_args() {
  local mode="$1"
  case "$mode" in
    proxy-off) printf '%s\n' '-c' 'http.extraHeader=git-cache-use-proxy-on-miss: false' ;;
    default) ;;
    *) printf 'invalid mode: %s\n' "$mode" >&2; exit 2 ;;
  esac
}

# Note a 5xx in captured stderr and bump the failure counter.
note_failure() {
  local label="$1" err="$2"
  FAILURES=$((FAILURES + 1))
  printf '\nSTDERR label=%s\n' "$label"
  tail -40 "$err" || true
  if grep -qiE 'HTTP 5[0-9][0-9]' "$err"; then
    printf 'NOTE label=%s saw HTTP 5xx (524=Cloudflare timeout on heavy deepen; 503=stale-lock regression)\n' "$label"
  fi
}

report_repo() {
  local dir="$1"
  printf ' head=%s shallow=%s commits=%s pack_size=%s\n' \
    "$(git -C "$dir" rev-parse --short=12 HEAD 2>/dev/null || echo '?')" \
    "$(git -C "$dir" rev-parse --is-shallow-repository 2>/dev/null || echo '?')" \
    "$(git -C "$dir" rev-list --count HEAD 2>/dev/null || echo '?')" \
    "$(git -C "$dir" count-objects -vH 2>/dev/null | awk -F': ' '/size-pack/ {print $2}')"
}

clone_one() {
  local label="$1" mode="$2" depth="$3"
  local dir="$WORKDIR/$label"
  local err="$WORKDIR/$label.stderr"
  local started finished exit_code
  local mode_args=()
  while IFS= read -r a; do mode_args+=("$a"); done < <(mode_git_args "$mode")

  rm -rf "$dir"
  started="$(date +%s)"
  set +e
  git -c protocol.version=2 ${mode_args[@]+"${mode_args[@]}"} \
    clone --quiet --single-branch --no-tags --filter=blob:none \
    --no-checkout --depth "$depth" "$REMOTE_URL" "$dir" 2>"$err"
  exit_code=$?
  set -e
  finished="$(date +%s)"

  printf 'CLONE label=%s mode=%s depth=%s exit=%s elapsed_s=%s' \
    "$label" "$mode" "$depth" "$exit_code" "$((finished - started))"
  if [[ "$exit_code" -eq 0 ]]; then
    report_repo "$dir"
  else
    note_failure "$label" "$err"
  fi
}

# Deepen an existing shallow clone in place by N commits.
deepen_one() {
  local dir="$1" mode="$2" n="$3" label="$4"
  local err="$WORKDIR/$label.stderr"
  local started finished exit_code
  local mode_args=()
  while IFS= read -r a; do mode_args+=("$a"); done < <(mode_git_args "$mode")

  started="$(date +%s)"
  set +e
  git -C "$dir" -c protocol.version=2 ${mode_args[@]+"${mode_args[@]}"} \
    fetch --quiet --filter=blob:none --deepen "$n" origin 2>"$err"
  exit_code=$?
  set -e
  finished="$(date +%s)"

  printf '  DEEPEN label=%s mode=%s by=%s exit=%s elapsed_s=%s' \
    "$label" "$mode" "$n" "$exit_code" "$((finished - started))"
  if [[ "$exit_code" -eq 0 ]]; then
    report_repo "$dir"
  else
    note_failure "$label" "$err"
  fi
}

run_depth_matrix() {
  [[ "$RUN_DEPTH_MATRIX" == "true" ]] || return 0
  local phase="$1"
  local order depth mode mode_label order_label
  local orders=()
  # Split the `|`-separated order list; inner loops keep the default IFS so the
  # space-separated depths within each order split correctly.
  IFS='|' read -r -a orders <<< "$DEPTH_ORDERS"
  for order in "${orders[@]}"; do
    order_label="${order// /_}"
    printf -- '-- depth order [%s] --\n' "$order"
    for depth in $order; do
      for mode in $MODES; do
        mode_label="${mode//-/_}"
        clone_one "${phase}_${order_label}_${mode_label}_d${depth}" "$mode" "$depth"
      done
    done
  done
}

run_deepen_matrix() {
  [[ "$RUN_DEEPEN_MATRIX" == "true" ]] || return 0
  local phase="$1"
  local mode mode_label dir label n max=0
  for n in $DEEPEN_STEPS; do [[ "$n" -gt "$max" ]] && max="$n"; done
  for mode in $MODES; do
    mode_label="${mode//-/_}"

    # Ascending: one shallow clone, then deepen by each step in turn. This is
    # the path that re-poisoned prod on 0.0.8 (repeated boundary rewrites).
    label="${phase}_${mode_label}_asc"
    dir="$WORKDIR/$label"
    printf -- '-- deepen ascending [%s] mode=%s base=%s --\n' "$DEEPEN_STEPS" "$mode" "$DEEPEN_BASE_DEPTH"
    clone_one "$label" "$mode" "$DEEPEN_BASE_DEPTH"
    if [[ -d "$dir" ]]; then
      for n in $DEEPEN_STEPS; do deepen_one "$dir" "$mode" "$n" "${label}_by${n}"; done
    fi

    # Jump: fresh depth-1 clone straight to the largest deepen in one request.
    label="${phase}_${mode_label}_jump${max}"
    dir="$WORKDIR/$label"
    printf -- '-- deepen jump-to-%s mode=%s --\n' "$max" "$mode"
    clone_one "$label" "$mode" 1
    if [[ -d "$dir" ]]; then
      deepen_one "$dir" "$mode" "$max" "${label}_by${max}"
    fi
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
# Lingering repo-global lock files are the stale-`shallow.lock` regression
# signal (PR #126): if any of these persist with no git child holding them,
# the repo is poisoned and boundary-rewriting deepens will 503.
echo STALE_LOCKS
found_lock=0
for lk in shallow.lock objects/info/commit-graph.lock packed-refs.lock; do
  if [ -e "$repo_dir/$lk" ]; then
    found_lock=1
    ls -l "$repo_dir/$lk"
  fi
done
[ "$found_lock" -eq 0 ] && echo NO_STALE_LOCKS
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
printf '\n=== DEPTH MATRIX (independent clones) ===\n'
run_depth_matrix initial
if [[ "$RUN_REPEATS" == "true" ]]; then
  run_depth_matrix repeat
fi
printf '\n=== DEEPEN MATRIX (shallow clone then fetch --deepen) ===\n'
run_deepen_matrix deepen
metrics METRICS_AFTER
inspect_hot_cache
tail_api_logs
printf 'WORKDIR=%s\n' "$WORKDIR"

printf '\n=== SUMMARY failures=%s ===\n' "$FAILURES"
if [[ "$FAILURES" -ne 0 ]]; then
  printf 'FAILED: %s clone/deepen operation(s) returned non-zero\n' "$FAILURES"
  exit 1
fi
printf 'OK: all clone/deepen operations succeeded\n'
