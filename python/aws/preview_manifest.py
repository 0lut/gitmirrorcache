import json
import os
import sys
from datetime import datetime, timezone

manifest = {
    "version_id": os.environ["VERSION_ID"],
    "ref": os.environ["PREVIEW_REF"],
    "commit": os.environ.get("PREVIEW_COMMIT") or None,
    "deployed_at": datetime.now(timezone.utc).isoformat(),
    "aws_region": os.environ["AWS_REGION"],
    "name_prefix": os.environ["NAME_PREFIX"],
    "environment": os.environ["ENVIRONMENT"],
    "public_base_url": os.environ["PREVIEW_PUBLIC_BASE_URL"],
    "health_url": os.environ["PREVIEW_PUBLIC_BASE_URL"].rstrip("/") + "/healthz",
    "shared_alb": os.environ.get("ECS_SHARED_ALB") == "true",
    "alb_name": os.environ["ECS_ALB_NAME"],
    "public_path_prefix": os.environ.get("ECS_PUBLIC_PATH_PREFIX") or None,
    "s3_bucket": os.environ["S3_BUCKET"],
    "s3_prefix": os.environ["S3_PREFIX"],
    "ecr_repository": os.environ["ECR_REPOSITORY"],
    "image_tag": os.environ["IMAGE_TAG"],
    "ecs_cluster_name": os.environ["ECS_CLUSTER_NAME"],
    "ecs_service_name": os.environ["ECS_SERVICE_NAME"],
}
json.dump(manifest, open(sys.argv[1], "w"), indent=2)
