import sys

used = int(sys.argv[1])
desired = int(sys.argv[2])
projected = int(sys.argv[3])
quota = float(sys.argv[4])
instance_type = sys.argv[5]

if projected > quota:
    raise SystemExit(
        "EC2 vCPU quota preflight failed: "
        f"running/pending={used}, requested {instance_type}={desired}, "
        f"projected={projected}, quota={quota:g}"
    )

print(
    "EC2 vCPU quota preflight passed: "
    f"running/pending={used}, requested {instance_type}={desired}, "
    f"projected={projected}, quota={quota:g}"
)
