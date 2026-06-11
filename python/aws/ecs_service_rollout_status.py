import json, sys
service = json.load(sys.stdin) or {}
deployments = service.get("deployments") or []
primary = next((d for d in deployments if d.get("status") == "PRIMARY"), {})
print(
    service.get("status", "None"),
    service.get("desiredCount", 0),
    service.get("runningCount", 0),
    service.get("pendingCount", 0),
    len(deployments),
    primary.get("rolloutState", "UNKNOWN"),
)
