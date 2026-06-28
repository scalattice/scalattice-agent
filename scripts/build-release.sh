#!/usr/bin/env bash
# Build and package scalattice-agent for one Linux target (same output as CI).
#
# Usage:
#   ./scripts/build-release.sh                          # x86_64, gpu features
#   ./scripts/build-release.sh aarch64-unknown-linux-gnu arm-gpu
#
# Requires: rust stable, CUDA 12.6 dev libs, Vulkan/glslc, patchelf (see release.yml).
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="${1:-x86_64-unknown-linux-gnu}"
FEATURES="${2:-}"
NO_DEFAULT="false"

case "$TARGET" in
  x86_64-unknown-linux-gnu)
    FEATURES="${FEATURES:-gpu}"
    NO_DEFAULT="false"
    ;;
  aarch64-unknown-linux-gnu)
    FEATURES="${FEATURES:-arm-gpu}"
    NO_DEFAULT="true"
    ;;
  *)
    echo "Unsupported target: $TARGET" >&2
    echo "Use x86_64-unknown-linux-gnu or aarch64-unknown-linux-gnu" >&2
    exit 1
    ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found — install Rust stable: https://rustup.rs" >&2
  exit 1
fi

if [[ ! -f Cargo.lock ]]; then
  echo "==> Generating Cargo.lock (commit this file — it keeps CI cache warm)"
  cargo generate-lockfile
fi

rustup target add "$TARGET" >/dev/null 2>&1 || true

args=(build --release --target "$TARGET")
if [[ "$NO_DEFAULT" == "true" ]]; then
  args+=(--no-default-features)
fi
args+=(--features "$FEATURES")

echo "==> cargo ${args[*]}"
echo "    (llama.cpp + CUDA/Vulkan dominate build time — clap is seconds, not an hour)"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CMAKE_PREFIX_PATH="${CMAKE_PREFIX_PATH:-/usr}"
export CUDA_PATH="${CUDA_PATH:-/usr/local/cuda-12.6}"
export Vulkan_GLSLC_EXECUTABLE="${Vulkan_GLSLC_EXECUTABLE:-/usr/bin/glslc}"
cargo "${args[@]}"

RELEASE="target/${TARGET}/release"
BIN="${RELEASE}/scalattice-agent"
if [[ ! -x "$BIN" ]]; then
  echo "Missing binary: $BIN" >&2
  exit 1
fi

mkdir -p dist
cp "$BIN" dist/scalattice-agent
chmod +x scripts/bundle-release-libs.sh
scripts/bundle-release-libs.sh dist/scalattice-agent dist "${RELEASE}"

ARCHIVE="dist/scalattice-agent-${TARGET}.tar.gz"
if [[ -d dist/lib ]]; then
  tar -czf "$ARCHIVE" -C dist scalattice-agent lib
else
  tar -czf "$ARCHIVE" -C dist scalattice-agent
fi

echo ""
echo "==> Built ${ARCHIVE}"
tar -tzf "$ARCHIVE"
echo ""
echo "==> dynamic deps (host driver libs should stay external)"
ldd dist/scalattice-agent || true
