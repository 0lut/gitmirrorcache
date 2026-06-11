import json
import os
import sys

path_pattern = sys.argv[1]
try:
    rules = json.loads(os.environ.get("RULES_JSON") or "{}").get("Rules", [])
except json.JSONDecodeError:
    rules = []
for rule in rules:
    for condition in rule.get("Conditions", []):
        if condition.get("Field") != "path-pattern":
            continue
        values = condition.get("Values") or condition.get("PathPatternConfig", {}).get("Values", [])
        if path_pattern in values:
            print(rule["RuleArn"])
            raise SystemExit(0)
print("None")
