import json
import os
import sys

policy = {
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": "ecs:RunTask",
            "Resource": os.environ["ECS_COMPACTION_TASK_DEFINITION_ARN"],
        },
        {
            "Effect": "Allow",
            "Action": "iam:PassRole",
            "Resource": [
                os.environ["ECS_TASK_EXECUTION_ROLE_ARN"],
                os.environ["ECS_TASK_ROLE_ARN"],
            ],
            "Condition": {
                "StringEquals": {
                    "iam:PassedToService": "ecs-tasks.amazonaws.com",
                },
            },
        },
    ],
}
json.dump(policy, open(sys.argv[1], "w"))
