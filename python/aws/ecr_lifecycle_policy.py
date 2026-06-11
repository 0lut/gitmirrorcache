import json
import os
import sys

retain = int(os.environ.get("ECR_RETAIN_IMAGES", "30"))
json.dump({
    "rules": [{
        "rulePriority": 1,
        "description": f"Keep the last {retain} images",
        "selection": {
            "tagStatus": "any",
            "countType": "imageCountMoreThan",
            "countNumber": retain,
        },
        "action": {"type": "expire"},
    }]
}, open(sys.argv[1], "w"))
