import json
import os
import sys

partition = os.environ["AWS_PARTITION"]
bucket = os.environ["S3_BUCKET"]
prefix = os.environ.get("S3_RUNTIME_PREFIX", os.environ.get("S3_PREFIX", "")).strip("/")
bucket_arn = f"arn:{partition}:s3:::{bucket}"
object_arn = f"{bucket_arn}/{prefix}/*" if prefix else f"{bucket_arn}/*"
list_statement = {
    "Effect": "Allow",
    "Action": ["s3:GetBucketLocation", "s3:ListBucket", "s3:ListBucketMultipartUploads"],
    "Resource": bucket_arn,
}
if prefix:
    list_statement["Condition"] = {
        "StringLikeIfExists": {"s3:prefix": [prefix, f"{prefix}/*"]}
    }
policy = {
    "Version": "2012-10-17",
    "Statement": [
        list_statement,
        {
            "Effect": "Allow",
            "Action": [
                "s3:AbortMultipartUpload",
                "s3:DeleteObject",
                "s3:GetObject",
                "s3:ListMultipartUploadParts",
                "s3:PutObject",
            ],
            "Resource": object_arn,
        },
    ],
}
json.dump(policy, open(sys.argv[1], "w"))
