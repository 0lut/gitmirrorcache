#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context
require_cmd ssh

DEVBOX_NAME="${DEVBOX_NAME:-$NAME_PREFIX-devbox}"
DEVBOX_SSH_USER="${DEVBOX_SSH_USER:-ec2-user}"
DEVBOX_KEY_NAME="${DEVBOX_KEY_NAME:-$NAME_PREFIX-devbox}"

instance_id="$(aws_cli ec2 describe-instances \
  --filters \
    Name=tag:Name,Values="$DEVBOX_NAME" \
    Name=tag:App,Values="$APP_NAME" \
    Name=tag:Environment,Values="$ENVIRONMENT" \
    Name=instance-state-name,Values=running \
  --query 'Reservations[].Instances[].InstanceId | [0]' \
  --output text)"
[[ "$instance_id" != "None" && -n "$instance_id" ]] || die "running devbox not found; run scripts/aws/devbox.sh first"

host="$(aws_cli ec2 describe-instances \
  --instance-ids "$instance_id" \
  --query 'Reservations[0].Instances[0].PublicDnsName' \
  --output text)"
[[ "$host" != "None" && -n "$host" ]] || die "devbox has no public DNS name"

if [[ -n "${DEVBOX_PRIVATE_KEY_PATH:-}" ]]; then
  key_path="$DEVBOX_PRIVATE_KEY_PATH"
elif [[ -f "$HOME/.ssh/$DEVBOX_KEY_NAME" ]]; then
  key_path="$HOME/.ssh/$DEVBOX_KEY_NAME"
elif [[ -f "$HOME/.ssh/id_ed25519" ]]; then
  key_path="$HOME/.ssh/id_ed25519"
elif [[ -f "$HOME/.ssh/id_rsa" ]]; then
  key_path="$HOME/.ssh/id_rsa"
else
  die "could not find private key; set DEVBOX_PRIVATE_KEY_PATH"
fi
[[ -f "$key_path" ]] || die "private key does not exist: $key_path"

exec ssh \
  -i "$key_path" \
  -o IdentitiesOnly=yes \
  -o StrictHostKeyChecking=accept-new \
  "$DEVBOX_SSH_USER@$host" \
  "$@"
