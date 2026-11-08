#!/bin/sh
# Compile storage-analyzer inside the build container; the static binary lands in ./out on the host.
# Usage: ./indockerbuild.sh [JOBS] [cargo args...]   (default: format check + tests + clippy + build)
set -eu

IMAGE="storage-analyzer-build"
TOP="$(cd "$(dirname "$0")" && pwd)"
JOBS="${1:-$(( $(nproc) * 8 / 10 ))}"
[ $# -gt 0 ] && shift

mkdir -p "${TOP}/logs" "${TOP}/out" "${TOP}/.docker-cargo"
LOG="${TOP}/logs/indockerbuild_$(date +%Y%m%d_%H%M%S).log"

if [ $# -gt 0 ]; then
  CMD="cargo $*"
else
  CMD="cargo fmt --check && cargo test --release -j ${JOBS} \
    && cargo clippy --release --all-targets -j ${JOBS} -- -D warnings \
    && cargo build --release -j ${JOBS} \
    && cp target/release/storage-analyzer out/storage-analyzer.new \
    && mv -f out/storage-analyzer.new out/storage-analyzer"
fi

echo "jobs: ${JOBS}, command: ${CMD}, log: ${LOG}"
docker run --rm --cpus="${JOBS}" \
  -u "$(id -u):$(id -g)" \
  -e CARGO_HOME=/work/.docker-cargo \
  -v "${TOP}:/work" \
  -w /work \
  "${IMAGE}" sh -c "${CMD}" 2>&1 | tee "${LOG}"
