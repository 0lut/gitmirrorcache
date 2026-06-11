import json
import os
import sys

policy = {
    "Version": "2012-10-17",
    "Statement": [{
        "Effect": "Allow",
        "Action": ["secretsmanager:GetSecretValue"],
        "Resource": os.environ["GITHUB_TOKEN_SECRET_ARN"],
    }],
}
json.dump(policy, open(sys.argv[1], "w"))
