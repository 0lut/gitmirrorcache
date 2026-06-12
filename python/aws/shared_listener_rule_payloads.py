import json
import os
import sys

conditions_path, actions_path, transforms_path = sys.argv[1:]

conditions = [{
    "Field": "path-pattern",
    "PathPatternConfig": {"Values": [os.environ["ECS_ALB_RULE_PATH_PATTERN"]]},
}]
actions = [{
    "Type": "forward",
    "TargetGroupArn": os.environ["ECS_LISTENER_TARGET_GROUP_ARN"],
}]
transforms = [{
    "Type": "url-rewrite",
    "UrlRewriteConfig": {
        "Rewrites": [{
            "Regex": os.environ["ECS_ALB_RULE_REWRITE_REGEX"],
            "Replace": os.environ["ECS_ALB_RULE_REWRITE_REPLACE"],
        }],
    },
}]

json.dump(conditions, open(conditions_path, "w"))
json.dump(actions, open(actions_path, "w"))
json.dump(transforms, open(transforms_path, "w"))
