#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

init_aws_context
require_cmd curl

DEVBOX_NAME="${DEVBOX_NAME:-$NAME_PREFIX-devbox}"
DEVBOX_INSTANCE_TYPE="${DEVBOX_INSTANCE_TYPE:-t3.micro}"
DEVBOX_KEY_NAME="${DEVBOX_KEY_NAME:-$NAME_PREFIX-devbox}"
DEVBOX_SSH_USER="${DEVBOX_SSH_USER:-ec2-user}"

key_path_from_public_key() {
  local public_key_path="$1"
  printf '%s' "${public_key_path%.pub}"
}

choose_public_key() {
  if [[ -n "${DEVBOX_PUBLIC_KEY_PATH:-}" ]]; then
    [[ -f "$DEVBOX_PUBLIC_KEY_PATH" ]] || die "DEVBOX_PUBLIC_KEY_PATH does not exist: $DEVBOX_PUBLIC_KEY_PATH"
    printf '%s\n' "$DEVBOX_PUBLIC_KEY_PATH"
    return
  fi

  local generated="$HOME/.ssh/$DEVBOX_KEY_NAME"
  if [[ -f "$generated.pub" ]]; then
    printf '%s\n' "$generated.pub"
    return
  fi
  if [[ -f "$HOME/.ssh/id_ed25519.pub" ]]; then
    printf '%s\n' "$HOME/.ssh/id_ed25519.pub"
    return
  fi
  if [[ -f "$HOME/.ssh/id_rsa.pub" ]]; then
    printf '%s\n' "$HOME/.ssh/id_rsa.pub"
    return
  fi

  require_cmd ssh-keygen
  mkdir -p "$HOME/.ssh"
  chmod 700 "$HOME/.ssh"
  ssh-keygen -t ed25519 -f "$generated" -N "" -C "$USER@$HOSTNAME $DEVBOX_NAME" >/dev/null
  printf '%s\n' "$generated.pub"
}

app_base_url() {
  if [[ -n "${PUBLIC_BASE_URL:-}" ]]; then
    printf '%s\n' "${PUBLIC_BASE_URL%/}"
    return
  fi

  local alb_name
  alb_name="${ECS_ALB_NAME:-$NAME_PREFIX-ec2-alb}"
  alb_base_url_by_name "$alb_name" || die "could not resolve ALB $alb_name; set PUBLIC_BASE_URL to override"
}

default_vpc_id() {
  aws_cli ec2 describe-vpcs \
    --filters Name=is-default,Values=true \
    --query 'Vpcs[0].VpcId' \
    --output text
}

default_subnet_id() {
  local vpc_id="$1"
  local subnet_id
  subnet_id="$(aws_cli ec2 describe-subnets \
    --filters Name=vpc-id,Values="$vpc_id" Name=default-for-az,Values=true \
    --query 'Subnets[0].SubnetId' \
    --output text)"
  if [[ "$subnet_id" == "None" || -z "$subnet_id" ]]; then
    subnet_id="$(aws_cli ec2 describe-subnets \
      --filters Name=vpc-id,Values="$vpc_id" Name=state,Values=available \
      --query 'Subnets[0].SubnetId' \
      --output text)"
  fi
  printf '%s\n' "$subnet_id"
}

ensure_key_pair() {
  local public_key_path="$1"
  if aws_cli ec2 describe-key-pairs --key-names "$DEVBOX_KEY_NAME" >/dev/null 2>&1; then
    printf 'using existing EC2 key pair: %s\n' "$DEVBOX_KEY_NAME"
    return
  fi

  printf 'importing EC2 key pair: %s from %s\n' "$DEVBOX_KEY_NAME" "$public_key_path"
  aws_cli ec2 import-key-pair \
    --key-name "$DEVBOX_KEY_NAME" \
    --public-key-material "fileb://$public_key_path" \
    --tag-specifications "ResourceType=key-pair,Tags=[{Key=Name,Value=$DEVBOX_KEY_NAME},{Key=App,Value=$APP_NAME},{Key=Environment,Value=$ENVIRONMENT},{Key=ManagedBy,Value=scripts/aws/devbox.sh}]" >/dev/null
}

ensure_security_group() {
  local vpc_id="$1"
  local group_name="${DEVBOX_SECURITY_GROUP_NAME:-$DEVBOX_NAME}"
  local group_id
  group_id="$(aws_cli ec2 describe-security-groups \
    --filters Name=vpc-id,Values="$vpc_id" Name=group-name,Values="$group_name" \
    --query 'SecurityGroups[0].GroupId' \
    --output text)"
  if [[ "$group_id" == "None" || -z "$group_id" ]]; then
    printf 'creating security group: %s\n' "$group_name" >&2
    group_id="$(aws_cli ec2 create-security-group \
      --vpc-id "$vpc_id" \
      --group-name "$group_name" \
      --description "SSH access for $DEVBOX_NAME" \
      --tag-specifications "ResourceType=security-group,Tags=[{Key=Name,Value=$group_name},{Key=App,Value=$APP_NAME},{Key=Environment,Value=$ENVIRONMENT},{Key=ManagedBy,Value=scripts/aws/devbox.sh}]" \
      --query GroupId \
      --output text)"
  else
    printf 'using existing security group: %s (%s)\n' "$group_name" "$group_id" >&2
  fi

  local cidr="${DEVBOX_SSH_CIDR:-}"
  if [[ -z "$cidr" ]]; then
    local ip
    ip="$(curl -fsS https://checkip.amazonaws.com | tr -d '[:space:]')"
    [[ -n "$ip" ]] || die "could not determine current public IP; set DEVBOX_SSH_CIDR"
    cidr="$ip/32"
  fi

  local err_file
  err_file="$(mktemp)"
  if aws_cli ec2 authorize-security-group-ingress \
    --group-id "$group_id" \
    --ip-permissions "IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=$cidr,Description=SSH from dev workstation}]" \
    2>"$err_file" >/dev/null; then
    printf 'authorized SSH from %s\n' "$cidr" >&2
  elif grep -q 'InvalidPermission.Duplicate' "$err_file"; then
    printf 'SSH ingress already authorized from %s\n' "$cidr" >&2
  else
    cat "$err_file" >&2
    rm -f "$err_file"
    return 1
  fi
  rm -f "$err_file"

  printf '%s\n' "$group_id"
}

existing_instance_id() {
  aws_cli ec2 describe-instances \
    --filters \
      Name=tag:Name,Values="$DEVBOX_NAME" \
      Name=tag:App,Values="$APP_NAME" \
      Name=tag:Environment,Values="$ENVIRONMENT" \
      Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId | [0]' \
    --output text
}

instance_public_host() {
  local instance_id="$1"
  aws_cli ec2 describe-instances \
    --instance-ids "$instance_id" \
    --query 'Reservations[0].Instances[0].PublicDnsName' \
    --output text
}

instance_state() {
  local instance_id="$1"
  aws_cli ec2 describe-instances \
    --instance-ids "$instance_id" \
    --query 'Reservations[0].Instances[0].State.Name' \
    --output text
}

write_user_data() {
  local output="$1"
  local base_url="$2"
  cat >"$output" <<EOF
#!/usr/bin/env bash
set -euo pipefail
dnf install -y git jq curl >/var/log/gitmirrorcache-devbox-bootstrap.log 2>&1 || true
cat >/etc/profile.d/gitmirrorcache.sh <<'PROFILE'
export GITMIRRORCACHE_URL="$base_url"
PROFILE
cat >/home/$DEVBOX_SSH_USER/test-gitmirrorcache.sh <<'TEST'
#!/usr/bin/env bash
set -euo pipefail
base="\${1:-\${GITMIRRORCACHE_URL:-$base_url}}"
printf 'Testing %s\n' "\$base"
curl -fsS "\$base/healthz"
printf '\n'
TEST
chown $DEVBOX_SSH_USER:$DEVBOX_SSH_USER /home/$DEVBOX_SSH_USER/test-gitmirrorcache.sh
chmod 0755 /home/$DEVBOX_SSH_USER/test-gitmirrorcache.sh
EOF
}

public_key_path="$(choose_public_key)"
private_key_path="$(key_path_from_public_key "$public_key_path")"
ensure_key_pair "$public_key_path"

vpc_id="${DEVBOX_VPC_ID:-$(default_vpc_id)}"
[[ "$vpc_id" != "None" && -n "$vpc_id" ]] || die "no default VPC found; set DEVBOX_VPC_ID"
subnet_id="${DEVBOX_SUBNET_ID:-$(default_subnet_id "$vpc_id")}"
[[ "$subnet_id" != "None" && -n "$subnet_id" ]] || die "no usable subnet found; set DEVBOX_SUBNET_ID"
security_group_id="$(ensure_security_group "$vpc_id")"
base_url="$(app_base_url)"

instance_id="$(existing_instance_id)"
if [[ "$instance_id" == "None" || -z "$instance_id" ]]; then
  ami_id="${DEVBOX_AMI_ID:-$(aws_cli ssm get-parameter --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 --query Parameter.Value --output text)}"
  user_data="$(mktemp)"
  write_user_data "$user_data" "$base_url"
  printf 'creating devbox instance: %s\n' "$DEVBOX_NAME"
  instance_id="$(aws_cli ec2 run-instances \
    --image-id "$ami_id" \
    --instance-type "$DEVBOX_INSTANCE_TYPE" \
    --key-name "$DEVBOX_KEY_NAME" \
    --network-interfaces "DeviceIndex=0,SubnetId=$subnet_id,Groups=[$security_group_id],AssociatePublicIpAddress=true" \
    --user-data "file://$user_data" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$DEVBOX_NAME},{Key=App,Value=$APP_NAME},{Key=Environment,Value=$ENVIRONMENT},{Key=ManagedBy,Value=scripts/aws/devbox.sh},{Key=Purpose,Value=testing gitmirrorcache ECS deployment}]" \
    --query 'Instances[0].InstanceId' \
    --output text)"
  rm -f "$user_data"
else
  state="$(instance_state "$instance_id")"
  printf 'using existing devbox instance: %s (%s)\n' "$instance_id" "$state"
  if [[ "$state" == "stopped" ]]; then
    aws_cli ec2 start-instances --instance-ids "$instance_id" >/dev/null
  fi
fi

aws_cli ec2 wait instance-running --instance-ids "$instance_id"
aws_cli ec2 wait instance-status-ok --instance-ids "$instance_id"
host="$(instance_public_host "$instance_id")"
[[ "$host" != "None" && -n "$host" ]] || die "instance has no public DNS name"

cat <<EOF
Devbox is ready.
INSTANCE_ID=$instance_id
PUBLIC_HOST=$host
SSH_USER=$DEVBOX_SSH_USER
KEY_NAME=$DEVBOX_KEY_NAME
PRIVATE_KEY_PATH=$private_key_path
APP_BASE_URL=$base_url

Connect with:
AWS_REGION=$AWS_REGION ENVIRONMENT=$ENVIRONMENT scripts/aws/ssh-devbox.sh
EOF
