# shellcheck shell=sh disable=SC2154
# Remote AWS-RunShellScript fragment; python/aws/ssm_command.py prepends
# 'set -euo pipefail' and the shell-quoted variable assignments.

repo_dir="/cache/repos/${repo}.git"

container_id="$(docker ps   --filter "label=com.amazonaws.ecs.task-definition-family=$expected_family"   --filter "label=com.amazonaws.ecs.container-name=$expected_container"   --format '{{.ID}}' | head -n1)"

if [ -z "$container_id" ]; then
  echo "no running ECS API container found for $expected_family/$expected_container" >&2
  docker ps -a --format '{{.ID}} {{.Image}} {{.Names}} {{.Status}}'
  exit 20
fi

family="$(docker inspect --format '{{ index .Config.Labels "com.amazonaws.ecs.task-definition-family" }}' "$container_id")"
container_name="$(docker inspect --format '{{ index .Config.Labels "com.amazonaws.ecs.container-name" }}' "$container_id")"
image="$(docker inspect --format '{{ .Config.Image }}' "$container_id")"

printf 'container: %s\n' "$container_id"
printf 'image: %s\n' "$image"
printf 'ecs family: %s\n' "$family"
printf 'ecs container: %s\n' "$container_name"
printf 'repo: %s\n' "$repo"

if [ "$family" != "$expected_family" ]; then
  echo "refusing diagnostics: expected ECS task family $expected_family, got $family" >&2
  exit 10
fi
if [ "$container_name" != "$expected_container" ]; then
  echo "refusing diagnostics: expected ECS container $expected_container, got $container_name" >&2
  exit 11
fi
if [ ! -d "$repo_dir" ]; then
  printf 'repo dir not found: %s\n' "$repo_dir" >&2
  exit 12
fi

echo
echo '--- repo basics ---'
docker exec "$container_id" git --version
docker exec "$container_id" git -C "$repo_dir" rev-parse --is-bare-repository
docker exec "$container_id" git -C "$repo_dir" rev-parse --is-shallow-repository || true
docker exec "$container_id" git -C "$repo_dir" count-objects -vH
docker exec "$container_id" /bin/sh -c "test -f '$repo_dir/shallow' && echo shallow-file-present || echo no-shallow-file"
docker exec "$container_id" /bin/sh -c "test -f '$repo_dir/git-cache-partial-hydration' && echo partial-hydration-marker-present || echo no-partial-hydration-marker"
docker exec "$container_id" /bin/sh -c "ls -lh '$repo_dir'/objects/pack"
docker exec "$container_id" /bin/sh -c "git -C '$repo_dir' config --local --list | grep -E 'uploadpack|transfer|promisor|partialclone|hideRefs|allow' || true"

echo
echo '--- selected refs ---'
docker exec "$container_id" git -C "$repo_dir" for-each-ref \
  --count=120 \
  --format='%(refname) %(objectname)' \
  refs/heads/main refs/cache/upstream/heads/main refs/git-cache-served/commits refs/cache/commits

if [ -n "$object" ]; then
  echo
  printf '%s\n' "--- object: $object ---"
  docker exec "$container_id" /bin/sh -c "GIT_NO_LAZY_FETCH=1 git -C '$repo_dir' cat-file -t '$object' && GIT_NO_LAZY_FETCH=1 git -C '$repo_dir' cat-file -s '$object'"
  docker exec "$container_id" /bin/sh -c "GIT_NO_LAZY_FETCH=1 git -C '$repo_dir' cat-file -e '${object}^{commit}' && echo commit-exists-no-lazy"
  docker exec "$container_id" /bin/sh -c "git -C '$repo_dir' merge-base --is-ancestor '$object' refs/heads/main && echo object-is-ancestor-of-main || echo object-is-not-ancestor-of-main"
  docker exec "$container_id" git -C "$repo_dir" for-each-ref --contains "$object" --count=80 --format='%(refname) %(objectname)' || true
fi

echo
echo '--- main ancestry sample ---'
docker exec "$container_id" git -C "$repo_dir" rev-list --parents --max-count=12 refs/heads/main

echo
echo '--- missing objects from reachable refs ---'
if [ -n "$skip_slow" ]; then
  echo 'skipped'
else
  docker exec "$container_id" /bin/sh -c "GIT_NO_LAZY_FETCH=1 git -C '$repo_dir' rev-list --objects --missing=print --all | awk 'BEGIN { missing=0 } substr(\$0, 1, 1) == \"?\" { missing++; if (missing <= 20) print } END { print \"missing_count=\" missing }'"

  echo
  echo '--- fsck connectivity ---'
  docker exec "$container_id" /bin/sh -c "GIT_NO_LAZY_FETCH=1 git -C '$repo_dir' fsck --connectivity-only --no-dangling"
fi
