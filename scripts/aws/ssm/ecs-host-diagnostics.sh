# shellcheck shell=sh disable=SC2154
# Remote AWS-RunShellScript fragment; python/aws/ssm_command.py prepends
# 'set -euo pipefail' and the shell-quoted variable assignments.

echo '--- docker containers ---'
docker ps -a --format '{{.ID}} {{.Image}} {{.Names}} {{.Status}} {{.Ports}}'
echo
echo "--- listeners on :$host_port ---"
sudo ss -ltnp | grep ":$host_port" || true
echo
echo '--- ecs agent recent logs ---'
sudo journalctl -u ecs --no-pager -n 80
