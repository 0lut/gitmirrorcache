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
