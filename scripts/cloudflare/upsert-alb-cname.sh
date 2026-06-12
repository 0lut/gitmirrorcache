#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
AWS_SCRIPT_DIR="$(cd -- "$SCRIPT_DIR/../aws" && pwd)"
source "$AWS_SCRIPT_DIR/common.sh"

APP_NAME="${APP_NAME:-gitmirrorcache}"
ENVIRONMENT="${ENVIRONMENT:-prod}"
NAME_PREFIX="${NAME_PREFIX:-gitmirrorcache-prod}"
export APP_NAME ENVIRONMENT NAME_PREFIX

init_aws_context
require_cmd curl
require_cmd python3

DOMAIN_NAME="${DOMAIN_NAME:-gitcache.sh}"
API_DOMAIN_NAME="${API_DOMAIN_NAME:-api.$DOMAIN_NAME}"
CF_ZONE_NAME="${CF_ZONE_NAME:-$DOMAIN_NAME}"
CF_RECORD_NAME="${CF_RECORD_NAME:-$API_DOMAIN_NAME}"
CF_PROXIED="${CF_PROXIED:-true}"
CF_TTL="${CF_TTL:-1}"
CF_API_BASE="${CF_API_BASE:-https://api.cloudflare.com/client/v4}"
ECS_ALB_NAME="${ECS_ALB_NAME:-$NAME_PREFIX-ec2-alb}"

case "$CF_PROXIED" in
  true | false) ;;
  *) die "CF_PROXIED must be true or false" ;;
esac
[[ "$CF_TTL" =~ ^[0-9]+$ ]] || die "CF_TTL must be an integer"
[[ -n "${CLOUDFLARE_API_TOKEN:-}" ]] || die "CLOUDFLARE_API_TOKEN is required"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

urlencode() {
  python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$1"
}

cf_api() {
  curl -fsS \
    -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
    -H "Content-Type: application/json" \
    "$@"
}

parse_zone_id() {
  python3 - "$1" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1]))
if not data.get("success"):
    raise SystemExit("cloudflare zone lookup failed: " + json.dumps(data.get("errors", [])))
results = data.get("result", [])
if len(results) != 1:
    raise SystemExit(f"expected exactly one Cloudflare zone, found {len(results)}")
print(results[0]["id"])
PY
}

parse_record() {
  python3 - "$1" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1]))
if not data.get("success"):
    raise SystemExit("cloudflare DNS record lookup failed: " + json.dumps(data.get("errors", [])))
results = data.get("result", [])
if len(results) > 1:
    raise SystemExit(f"expected zero or one DNS record with this name, found {len(results)}")
if results:
    record = results[0]
    print(record["id"], record["type"])
PY
}

write_payload() {
  python3 - "$tmpdir/dns-record.json" "$CF_RECORD_NAME" "$alb_dns_name" "$CF_PROXIED" "$CF_TTL" <<'PY'
import json
import sys

path, name, content, proxied, ttl = sys.argv[1:]
json.dump(
    {
        "type": "CNAME",
        "name": name,
        "content": content,
        "ttl": int(ttl),
        "proxied": proxied == "true",
        "comment": "gitmirrorcache production ALB API origin",
    },
    open(path, "w"),
)
PY
}

alb_dns_name="${ALB_DNS_NAME:-$(aws_cli elbv2 describe-load-balancers \
  --names "$ECS_ALB_NAME" \
  --query 'LoadBalancers[0].DNSName' \
  --output text)}"
[[ -n "$alb_dns_name" && "$alb_dns_name" != "None" ]] || die "could not resolve ALB DNS name for $ECS_ALB_NAME"
alb_dns_name="${alb_dns_name%.}"

zone_query="$(urlencode "$CF_ZONE_NAME")"
cf_api "$CF_API_BASE/zones?name=$zone_query&status=active" >"$tmpdir/zone.json"
zone_id="${CLOUDFLARE_ZONE_ID:-$(parse_zone_id "$tmpdir/zone.json")}"

record_query="$(urlencode "$CF_RECORD_NAME")"
cf_api "$CF_API_BASE/zones/$zone_id/dns_records?name=$record_query" >"$tmpdir/records.json"
record_info="$(parse_record "$tmpdir/records.json")"
record_id=""
record_type=""
if [[ -n "$record_info" ]]; then
  read -r record_id record_type <<<"$record_info"
fi

write_payload
if [[ -n "$record_id" ]]; then
  printf 'updating Cloudflare DNS record: %s (%s -> CNAME %s)\n' "$CF_RECORD_NAME" "$record_type" "$alb_dns_name"
  cf_api -X PUT "$CF_API_BASE/zones/$zone_id/dns_records/$record_id" --data "@$tmpdir/dns-record.json" >"$tmpdir/upsert.json"
else
  printf 'creating Cloudflare DNS record: %s CNAME %s\n' "$CF_RECORD_NAME" "$alb_dns_name"
  cf_api -X POST "$CF_API_BASE/zones/$zone_id/dns_records" --data "@$tmpdir/dns-record.json" >"$tmpdir/upsert.json"
fi

python3 - "$tmpdir/upsert.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1]))
if not data.get("success"):
    raise SystemExit("cloudflare DNS upsert failed: " + json.dumps(data.get("errors", [])))
result = data["result"]
print(f"CLOUDFLARE_RECORD_ID={result['id']}")
print(f"CLOUDFLARE_RECORD_NAME={result['name']}")
print(f"CLOUDFLARE_RECORD_TYPE={result['type']}")
print(f"CLOUDFLARE_RECORD_CONTENT={result['content']}")
print(f"CLOUDFLARE_RECORD_PROXIED={str(result.get('proxied', False)).lower()}")
PY
