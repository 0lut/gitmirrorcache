import json
import os
import sys

load_balancers = [{
    "containerName": os.environ["ECS_CONTAINER_NAME"],
    "containerPort": 8080,
    "targetGroupArn": os.environ["TARGET_GROUP_ARN"],
}]
base = {
    "cluster": os.environ["ECS_CLUSTER_NAME"],
    "serviceName": os.environ["ECS_SERVICE_NAME"],
    "taskDefinition": os.environ["TASK_DEFINITION_ARN"],
    "desiredCount": int(os.environ["ECS_DESIRED_COUNT"]),
    "launchType": "EC2",
    "loadBalancers": load_balancers,
    "healthCheckGracePeriodSeconds": int(os.environ.get("ECS_HEALTH_CHECK_GRACE_PERIOD_SECONDS", "300")),
    "deploymentConfiguration": {
        "minimumHealthyPercent": int(os.environ.get("ECS_MIN_HEALTHY_PERCENT", "0")),
        "maximumPercent": int(os.environ.get("ECS_MAX_PERCENT", "200")),
    },
    "placementConstraints": [{
        "type": "memberOf",
        "expression": "attribute:ecs.instance-type == " + os.environ["ECS_EC2_INSTANCE_TYPE"],
    }],
    "propagateTags": "SERVICE",
    "tags": [
        {"key": "App", "value": os.environ["APP_NAME"]},
        {"key": "Environment", "value": os.environ["ENVIRONMENT"]},
    ],
}
json.dump(base, open(sys.argv[1], "w"))

update = {
    "cluster": os.environ["ECS_CLUSTER_NAME"],
    "service": os.environ["ECS_SERVICE_NAME"],
    "taskDefinition": os.environ["TASK_DEFINITION_ARN"],
    "desiredCount": int(os.environ["ECS_DESIRED_COUNT"]),
    "forceNewDeployment": True,
    "loadBalancers": load_balancers,
    "healthCheckGracePeriodSeconds": base["healthCheckGracePeriodSeconds"],
    "deploymentConfiguration": base["deploymentConfiguration"],
}
json.dump(update, open(sys.argv[2], "w"))
