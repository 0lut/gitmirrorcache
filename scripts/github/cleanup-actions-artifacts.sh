#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd -- "$SCRIPT_DIR/../.." && pwd)}"

usage() {
  cat <<'USAGE'
Usage: cleanup-actions-artifacts.sh [--repo OWNER/REPO] [--older-than-days DAYS] [--all] [--dry-run] [--include-active-runs]

Deletes GitHub Actions artifacts for a repository.

Options:
  --repo OWNER/REPO       Repository to clean. Defaults to GH_REPO or the current gh repo.
  --older-than-days DAYS  Delete artifacts created before this many days ago. Default: 1.
  --all                  Delete every artifact.
  --dry-run              Print what would be deleted without deleting it.
  --include-active-runs  Also delete artifacts from in-progress workflow runs.
USAGE
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

repo="${GH_REPO:-}"
older_than_days="${ARTIFACT_OLDER_THAN_DAYS:-${OLDER_THAN_DAYS:-1}}"
delete_all="${DELETE_ALL_ARTIFACTS:-${DELETE_ALL:-false}}"
dry_run="${DRY_RUN:-false}"
skip_active_runs="${SKIP_ACTIVE_RUNS:-true}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      [[ $# -ge 2 ]] || die "--repo requires OWNER/REPO"
      repo="$2"
      shift 2
      ;;
    --older-than-days)
      [[ $# -ge 2 ]] || die "--older-than-days requires DAYS"
      older_than_days="$2"
      shift 2
      ;;
    --all)
      delete_all="true"
      shift
      ;;
    --dry-run)
      dry_run="true"
      shift
      ;;
    --include-active-runs)
      skip_active_runs="false"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

command -v gh >/dev/null 2>&1 || die "gh is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

if [[ -z "$repo" ]]; then
  repo="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
fi

[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "invalid repository: $repo"
[[ "$older_than_days" =~ ^[0-9]+$ ]] || die "--older-than-days must be a non-negative integer"

case "$delete_all" in
  true|false) ;;
  *) die "DELETE_ALL_ARTIFACTS must be true or false" ;;
esac

case "$dry_run" in
  true|false) ;;
  *) die "DRY_RUN must be true or false" ;;
esac

case "$skip_active_runs" in
  true|false) ;;
  *) die "SKIP_ACTIVE_RUNS must be true or false" ;;
esac

artifact_list="$(mktemp)"
trap 'rm -f "$artifact_list"' EXIT

gh api --paginate "/repos/${repo}/actions/artifacts?per_page=100" \
  --jq '.artifacts[] | [.id, .name, .created_at, (.expires_at // "-"), .size_in_bytes, (.workflow_run.id // "-")] | @tsv' \
  >"$artifact_list"

if [[ ! -s "$artifact_list" ]]; then
  printf 'No GitHub Actions artifacts found for %s.\n' "$repo"
  exit 0
fi

is_older_than() {
  python3 "$REPO_ROOT/python/github/artifact_is_older_than.py" "$1" "$2"
}

run_status() {
  local run_id="$1"
  gh api "/repos/${repo}/actions/runs/${run_id}" --jq .status 2>/dev/null || true
}

total=0
selected=0
deleted=0
skipped=0
selected_bytes=0
deleted_bytes=0

while IFS=$'\t' read -r artifact_id artifact_name created_at expires_at size_in_bytes workflow_run_id; do
  total=$((total + 1))
  [[ "$artifact_id" =~ ^[0-9]+$ ]] || die "GitHub returned a non-numeric artifact id: $artifact_id"
  [[ "$size_in_bytes" =~ ^[0-9]+$ ]] || size_in_bytes=0

  should_delete="false"
  if [[ "$delete_all" == "true" ]]; then
    should_delete="true"
  elif is_older_than "$created_at" "$older_than_days"; then
    should_delete="true"
  fi

  if [[ "$should_delete" != "true" ]]; then
    skipped=$((skipped + 1))
    continue
  fi

  if [[ "$skip_active_runs" == "true" && "$workflow_run_id" != "-" ]]; then
    [[ "$workflow_run_id" =~ ^[0-9]+$ ]] || die "GitHub returned a non-numeric workflow run id: $workflow_run_id"
    status="$(run_status "$workflow_run_id")"
    if [[ "$status" != "completed" ]]; then
      skipped=$((skipped + 1))
      printf 'Skipping artifact %s (%s) because workflow run %s status is %s.\n' \
        "$artifact_id" "$artifact_name" "$workflow_run_id" "${status:-unknown}"
      continue
    fi
  fi

  selected=$((selected + 1))
  selected_bytes=$((selected_bytes + size_in_bytes))

  if [[ "$dry_run" == "true" ]]; then
    printf 'Would delete artifact %s (%s, %s bytes, created %s, expires %s)\n' \
      "$artifact_id" "$artifact_name" "$size_in_bytes" "$created_at" "$expires_at"
    continue
  fi

  printf 'Deleting artifact %s (%s, %s bytes, created %s, expires %s)\n' \
    "$artifact_id" "$artifact_name" "$size_in_bytes" "$created_at" "$expires_at"
  gh api --method DELETE "/repos/${repo}/actions/artifacts/${artifact_id}" >/dev/null
  deleted=$((deleted + 1))
  deleted_bytes=$((deleted_bytes + size_in_bytes))
done <"$artifact_list"

if [[ "$dry_run" == "true" ]]; then
  printf 'Dry run complete for %s: %d total, %d selected (%d bytes), %d skipped.\n' \
    "$repo" "$total" "$selected" "$selected_bytes" "$skipped"
else
  printf 'Cleanup complete for %s: %d total, %d deleted (%d bytes), %d skipped.\n' \
    "$repo" "$total" "$deleted" "$deleted_bytes" "$skipped"
fi
