#!/usr/bin/env bash
# Build and push the arm64 image without a buildbox: cross-compile the Rust
# binaries natively on the host, then assemble a runtime-only image with
# docker buildx (QEMU is only used for the apt-get layer).
#
# Usage:
#   AWS_REGION=us-west-2 scripts/aws/build-image-cross.sh
#
# Variables:
#   IMAGE_TAG        image tag (default: git short HEAD)
#   PUSH             push to ECR (default: true; set false for --load only)
#   ECR_PUSH_LATEST  also tag/push :latest (default: false)
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

require_cmd docker
require_cmd cargo
require_cmd rustup
require_cmd git

CROSS_TARGET="aarch64-unknown-linux-gnu"
CROSS_LINKER="${CROSS_LINKER:-aarch64-linux-gnu-gcc}"
DOCKER_PLATFORM="${DOCKER_PLATFORM:-linux/arm64}"
PUSH="${PUSH:-true}"
ECR_PUSH_LATEST="${ECR_PUSH_LATEST:-false}"

command -v "$CROSS_LINKER" >/dev/null 2>&1 \
  || die "missing $CROSS_LINKER; install with: sudo apt-get install -y gcc-aarch64-linux-gnu"

rustup target list --installed | grep -qx "$CROSS_TARGET" \
  || rustup target add "$CROSS_TARGET"

if [[ ! -e /proc/sys/fs/binfmt_misc/qemu-aarch64 ]]; then
  printf 'registering qemu-aarch64 binfmt handler\n'
  docker run --privileged --rm tonistiigi/binfmt --install arm64 >/dev/null
fi

init_aws_context

IMAGE_TAG="${IMAGE_TAG:-$(git -C "$REPO_ROOT" rev-parse --short HEAD)}"
IMAGE_URI="${ECR_REPOSITORY_URI}:${IMAGE_TAG}"
LATEST_URI="${ECR_REPOSITORY_URI}:latest"

cross_compile() {
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$CROSS_LINKER" \
  CC_aarch64_unknown_linux_gnu="$CROSS_LINKER" \
    cargo build --release --features s3 \
    -p git-cache-api -p git-cache-cli \
    --target "$CROSS_TARGET" \
    --manifest-path "$REPO_ROOT/Cargo.toml"
}

STAGE_DIR=""
cleanup_stage_dir() {
  if [[ -n "$STAGE_DIR" ]]; then
    rm -rf "$STAGE_DIR"
  fi
}
trap cleanup_stage_dir EXIT

build_image() {
  local ctx
  ctx="$(mktemp -d)"
  STAGE_DIR="$ctx"
  cp "$REPO_ROOT/target/$CROSS_TARGET/release/git-cache-api" \
     "$REPO_ROOT/target/$CROSS_TARGET/release/git-cache" \
     "$ctx/"

  local build_args=(--platform "$DOCKER_PLATFORM" -t "$IMAGE_URI")
  if [[ "$ECR_PUSH_LATEST" == "true" ]]; then
    build_args+=(-t "$LATEST_URI")
  fi
  if [[ "$PUSH" == "true" ]]; then
    aws_cli ecr describe-repositories --repository-names "$ECR_REPOSITORY" >/dev/null 2>&1 \
      || die "ECR repository not found; run scripts/aws/bootstrap.sh first"
    aws_cli ecr get-login-password | docker login --username AWS --password-stdin \
      "${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"
    build_args+=(--push)
  else
    build_args+=(--load)
  fi
  docker buildx build "${build_args[@]}" -f "$REPO_ROOT/Dockerfile.cross" "$ctx"
}

timed "cross-compile ($CROSS_TARGET)" cross_compile
timed "build image ($IMAGE_URI)" build_image

printf 'IMAGE_URI=%s\n' "$IMAGE_URI"
if [[ "$PUSH" == "true" ]]; then
  printf 'Deploy with: ECS_SKIP_DOCKER_BUILD_IF_IMAGE_EXISTS=true IMAGE_TAG=%s\n' "$IMAGE_TAG"
fi
