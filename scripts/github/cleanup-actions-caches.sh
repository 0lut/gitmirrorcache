#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd -- "$SCRIPT_DIR/../.." && pwd)}"

usage() {
  cat <<'USAGE'
Usage: cleanup-actions-caches.sh [--repo OWNER/REPO] [--older-than-days DAYS] [--all] [--dry-run]

Deletes GitHub Actions caches for a repository.

Options:
  --repo OWNER/REPO       Repository to clean. Defaults to GH_REPO or the current gh repo.
  --older-than-days DAYS  Delete caches last accessed before this many days ago. Default: 7.
  --all                  Delete every Actions cache.
  --dry-run              Print what would be deleted without deleting it.
USAGE
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

repo="${GH_REPO:-}"
older_than_days="${OLDER_THAN_DAYS:-7}"
delete_all="${DELETE_ALL:-false}"
dry_run="${DRY_RUN:-false}"

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
  *) die "DELETE_ALL must be true or false" ;;
esac

case "$dry_run" in
  true|false) ;;
  *) die "DRY_RUN must be true or false" ;;
esac

cache_list="$(mktemp)"
trap 'rm -f "$cache_list"' EXIT

gh api --paginate "/repos/${repo}/actions/caches?per_page=100" \
  --jq '.actions_caches[] | [.id, .key, .last_accessed_at, .size_in_bytes] | @tsv' \
  >"$cache_list"

if [[ ! -s "$cache_list" ]]; then
  printf 'No GitHub Actions caches found for %s.\n' "$repo"
  exit 0
fi

total=0
selected=0
deleted=0
skipped=0

while IFS=$'\t' read -r cache_id cache_key last_accessed_at size_in_bytes; do
  total=$((total + 1))
  [[ "$cache_id" =~ ^[0-9]+$ ]] || die "GitHub returned a non-numeric cache id: $cache_id"

  should_delete="false"
  if [[ "$delete_all" == "true" ]]; then
    should_delete="true"
  elif python3 "$REPO_ROOT/python/github/cache_is_older_than.py" "$last_accessed_at" "$older_than_days"
  then
    should_delete="true"
  fi

  if [[ "$should_delete" != "true" ]]; then
    skipped=$((skipped + 1))
    continue
  fi

  selected=$((selected + 1))
  if [[ "$dry_run" == "true" ]]; then
    printf 'Would delete cache %s (%s, %s bytes, last accessed %s)\n' \
      "$cache_id" "$cache_key" "$size_in_bytes" "$last_accessed_at"
    continue
  fi

  printf 'Deleting cache %s (%s, %s bytes, last accessed %s)\n' \
    "$cache_id" "$cache_key" "$size_in_bytes" "$last_accessed_at"
  gh api --method DELETE "/repos/${repo}/actions/caches/${cache_id}" >/dev/null
  deleted=$((deleted + 1))
done <"$cache_list"

if [[ "$dry_run" == "true" ]]; then
  printf 'Dry run complete for %s: %d total, %d selected, %d skipped.\n' \
    "$repo" "$total" "$selected" "$skipped"
else
  printf 'Cleanup complete for %s: %d total, %d deleted, %d skipped.\n' \
    "$repo" "$total" "$deleted" "$skipped"
fi
