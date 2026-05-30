#!/usr/bin/env bash
# Build the void-box smoke image.
# Usage: build.sh <path-to-voidbox-binary> [<image-tag>]
set -euo pipefail

VOIDBOX_BIN="${1:?path to voidbox binary required}"
IMAGE="${2:-void-box:smoke}"

if [[ ! -x "$VOIDBOX_BIN" ]]; then
  echo "voidbox binary not executable: $VOIDBOX_BIN" >&2
  exit 2
fi

CTX=$(mktemp -d)
trap 'rm -rf "$CTX"' EXIT

cp "$VOIDBOX_BIN" "$CTX/voidbox"
cp "$(dirname "$0")/Dockerfile" "$CTX/Dockerfile"
cp "$(dirname "$0")/voidbox-entrypoint.sh" "$CTX/voidbox-entrypoint.sh"

docker build -t "$IMAGE" "$CTX"
echo "Built: $IMAGE"
