# shellcheck shell=sh disable=SC2154
# Remote AWS-RunShellScript fragment; python/aws/ssm_command.py prepends
# 'set -euo pipefail' and the shell-quoted variable assignments.

docker inspect "$container_id" >/dev/null
family="$(docker inspect --format '{{ index .Config.Labels "com.amazonaws.ecs.task-definition-family" }}' "$container_id")"
container_name="$(docker inspect --format '{{ index .Config.Labels "com.amazonaws.ecs.container-name" }}' "$container_id")"
running="$(docker inspect --format '{{ .State.Running }}' "$container_id")"
image="$(docker inspect --format '{{ .Config.Image }}' "$container_id")"

printf 'candidate container: %s\n' "$container_id"
printf 'image: %s\n' "$image"
printf 'ecs family: %s\n' "$family"
printf 'ecs container: %s\n' "$container_name"
printf 'running: %s\n' "$running"
printf 'listeners on :%s before stop:\n' "$host_port"
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
printf 'listeners on :%s after stop:\n' "$host_port"
sudo ss -ltnp | grep ":$host_port" || true
