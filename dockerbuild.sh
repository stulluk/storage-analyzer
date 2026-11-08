#!/bin/sh
# Build the Docker image used to compile storage-analyzer (UID/GID mapped to the host user).
set -eu

IMAGE="storage-analyzer-build"
TOP="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "${TOP}/logs"
LOG="${TOP}/logs/dockerbuild_$(date +%Y%m%d_%H%M%S).log"

echo "building image ${IMAGE}, log: ${LOG}"
docker build \
  --build-arg UID="$(id -u)" \
  --build-arg GID="$(id -g)" \
  -t "${IMAGE}" "${TOP}" 2>&1 | tee "${LOG}"
