import json
import os
import shlex

host_port = shlex.quote(os.environ["ECS_HOST_PORT"])
script = f"""set -euo pipefail
host_port={host_port}

echo '--- docker containers ---'
docker ps -a --format '{{{{.ID}}}} {{{{.Image}}}} {{{{.Names}}}} {{{{.Status}}}} {{{{.Ports}}}}'
echo
echo "--- listeners on :$host_port ---"
sudo ss -ltnp | grep ":$host_port" || true
echo
echo '--- ecs agent recent logs ---'
sudo journalctl -u ecs --no-pager -n 80
"""

json.dump({"commands": [script]}, open("/dev/stdout", "w"))
