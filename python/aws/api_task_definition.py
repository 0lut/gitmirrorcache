import json
import os
import sys

env = [
    {"name": "AWS_DEFAULT_REGION", "value": os.environ["AWS_REGION"]},
    {"name": "AWS_REGION", "value": os.environ["AWS_REGION"]},
    {"name": "GIT_CACHE_ALLOWED_UPSTREAM_HOSTS", "value": os.environ.get("ALLOWED_UPSTREAM_HOSTS", "github.com")},
    {"name": "GIT_CACHE_BIND_ADDR", "value": "0.0.0.0:8080"},
    {"name": "GIT_CACHE_DISK_MIN_FREE_BYTES", "value": os.environ["GIT_CACHE_DISK_MIN_FREE_BYTES"]},
    {"name": "GIT_CACHE_DISK_QUOTA_BYTES", "value": os.environ["GIT_CACHE_DISK_QUOTA_BYTES"]},
    {"name": "GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "value": os.environ.get("GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "10")},
    {"name": "GIT_CACHE_COMPACTION_INLINE", "value": os.environ.get("GIT_CACHE_COMPACTION_INLINE", "false")},
    {"name": "GIT_CACHE_GIT_TIMEOUT_SECONDS", "value": os.environ.get("GIT_CACHE_GIT_TIMEOUT_SECONDS", "3600")},
    {"name": "GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES", "value": os.environ.get("GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES", "8")},
    {"name": "GIT_CACHE_MAX_GIT_OUTPUT_BYTES", "value": os.environ.get("GIT_CACHE_MAX_GIT_OUTPUT_BYTES", "8589934592")},
    {"name": "GIT_CACHE_OBJECT_STORE_KIND", "value": "s3"},
    {"name": "GIT_CACHE_RATE_LIMIT_PER_MINUTE", "value": os.environ.get("GIT_CACHE_RATE_LIMIT_PER_MINUTE", "120")},
    {"name": "GIT_CACHE_ROOT", "value": "/cache"},
    {"name": "GIT_CACHE_S3_BUCKET", "value": os.environ["S3_BUCKET"]},
    {"name": "GIT_CACHE_S3_PREFIX", "value": os.environ["S3_PREFIX"]},
    {"name": "RUST_LOG", "value": os.environ.get("RUST_LOG", "info")},
]
if os.environ.get("S3_ENDPOINT"):
    env.append({"name": "GIT_CACHE_S3_ENDPOINT", "value": os.environ["S3_ENDPOINT"]})
secrets = []
if os.environ.get("GITHUB_TOKEN_SECRET_ARN"):
    env.append({"name": "GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV", "value": "GITHUB_TOKEN"})
    secrets.append({"name": "GITHUB_TOKEN", "valueFrom": os.environ["GITHUB_TOKEN_SECRET_ARN"]})

container = {
    "name": os.environ["ECS_CONTAINER_NAME"],
    "image": os.environ["IMAGE_URI"],
    "essential": True,
    "user": os.environ.get("ECS_CONTAINER_USER", "0"),
    "stopTimeout": int(os.environ["ECS_CONTAINER_STOP_TIMEOUT_SECONDS"]),
    "portMappings": [{
        "containerPort": 8080,
        "hostPort": 8080,
        "protocol": "tcp",
    }],
    "environment": env,
    "mountPoints": [{
        "sourceVolume": os.environ["ECS_CACHE_VOLUME_NAME"],
        "containerPath": "/cache",
        "readOnly": False,
    }],
    "logConfiguration": {
        "logDriver": "awslogs",
        "options": {
            "awslogs-group": os.environ["ECS_LOG_GROUP"],
            "awslogs-region": os.environ["AWS_REGION"],
            "awslogs-stream-prefix": os.environ.get("ECS_LOG_STREAM_PREFIX", "api"),
        },
    },
}
if secrets:
    container["secrets"] = secrets

task = {
    "family": os.environ["ECS_TASK_FAMILY"],
    "taskRoleArn": os.environ["ECS_TASK_ROLE_ARN"],
    "executionRoleArn": os.environ["ECS_EXECUTION_ROLE_ARN"],
    "networkMode": "host",
    "requiresCompatibilities": ["EC2"],
    "cpu": os.environ["ECS_CPU"],
    "memory": os.environ["ECS_MEMORY"],
    "runtimePlatform": {
        "cpuArchitecture": os.environ["ECS_CPU_ARCHITECTURE"],
        "operatingSystemFamily": "LINUX",
    },
    "containerDefinitions": [container],
    "volumes": [{
        "name": os.environ["ECS_CACHE_VOLUME_NAME"],
        "host": {"sourcePath": "/cache"},
    }],
}
json.dump(task, open(sys.argv[1], "w"))
