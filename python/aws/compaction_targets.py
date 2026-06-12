import json
import os
import sys

target = {
    "Id": os.environ["ECS_COMPACTION_TARGET_ID"],
    "Arn": os.environ["ECS_CLUSTER_ARN"],
    "RoleArn": os.environ["ECS_COMPACTION_EVENTS_ROLE_ARN"],
    "EcsParameters": {
        "TaskDefinitionArn": os.environ["ECS_COMPACTION_TASK_DEFINITION_ARN"],
        "TaskCount": 1,
        "LaunchType": "EC2",
        "Group": os.environ.get("ECS_COMPACTION_TASK_GROUP", "git-cache-compaction"),
        "PlacementConstraints": [{
            "type": "memberOf",
            "expression": "attribute:ecs.instance-type == " + os.environ["ECS_EC2_INSTANCE_TYPE"],
        }],
    },
}
json.dump([target], open(sys.argv[1], "w"))
