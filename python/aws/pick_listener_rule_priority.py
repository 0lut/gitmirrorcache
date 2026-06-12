import json
import os
import sys

seed = sys.argv[1]
rules = json.loads(os.environ["RULES_JSON"]).get("Rules", [])
used = set()
for rule in rules:
    priority = rule.get("Priority")
    if priority and priority != "default":
        used.add(int(priority))

try:
    base = int(seed[:8], 16)
except ValueError:
    base = 1000

candidate = 100 + (base % 49900)
for offset in range(49900):
    priority = 100 + ((candidate - 100 + offset) % 49900)
    if priority not in used:
        print(priority)
        raise SystemExit(0)

raise SystemExit("no available ALB listener rule priorities")
