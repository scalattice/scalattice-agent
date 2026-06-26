#!/usr/bin/env bash
# Bundle non-glibc shared libraries so the agent runs without apt install steps.
set -euo pipefail

BINARY="${1:?binary path}"
OUT_DIR="${2:?output directory}"
BUILD_ROOT="${3:-$(dirname "$BINARY")}"
LIB_DIR="$OUT_DIR/lib"
mkdir -p "$LIB_DIR"

# glibc + base toolchain — always on Linux; never bundle.
SKIP_RE='/(libc\.so|libm\.so|libpthread|libdl\.so|librt\.so|libresolv\.so|libstdc\+\+|libgcc_s|ld-linux)'

# NVIDIA user-space driver — must come from the host GPU driver, not our tarball.
SKIP_RE="${SKIP_RE}|libcuda\.so|libnvidia"

bundle_from() {
  local bin="$1"
  ldd "$bin" 2>/dev/null | awk '/=> \// {print $3}' | while read -r lib; do
    [[ -f "$lib" ]] || continue
    echo "$lib" | grep -Eq "$SKIP_RE" && continue
    cp -L "$lib" "$LIB_DIR/$(basename "$lib")"
  done
}

bundle_from "$BINARY"

# llama.cpp dynamic backend modules (when enabled).
find "$BUILD_ROOT/build" -type f \( -name 'libggml*.so*' -o -name 'libllama*.so*' \) 2>/dev/null \
  | while read -r lib; do
      cp -L "$lib" "$LIB_DIR/$(basename "$lib")"
    done || true

# Pull in transitive deps (e.g. libvulkan pulled in by libggml-vulkan).
for _ in 1 2 3 4 5; do
  before="$(find "$LIB_DIR" -maxdepth 1 -type f | wc -l)"
  for lib in "$LIB_DIR"/*; do
    [[ -f "$lib" ]] || continue
    bundle_from "$lib"
  done
  after="$(find "$LIB_DIR" -maxdepth 1 -type f | wc -l)"
  [[ "$after" -le "$before" ]] && break
done

if command -v patchelf >/dev/null 2>&1; then
  patchelf --set-rpath '$ORIGIN/../lib/scalattice' "$BINARY"
  for lib in "$LIB_DIR"/*.so*; do
    [[ -f "$lib" ]] || continue
    patchelf --set-rpath '$ORIGIN' "$lib" 2>/dev/null || true
  done
fi

if [[ -z "$(ls -A "$LIB_DIR" 2>/dev/null)" ]]; then
  rmdir "$LIB_DIR"
fi
