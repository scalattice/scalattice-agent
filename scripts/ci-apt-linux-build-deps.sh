#!/usr/bin/env bash
# Install Linux release/CI build deps. Safe on Ubuntu 22.04 (jammy) and 24.04.
# Usage: ci-apt-linux-build-deps.sh [--minimal]
#   --minimal  clang/cmake only (CI cargo check)
#   default    + Vulkan / shaderc packages (release builds)
set -euo pipefail

MINIMAL=0
if [[ "${1:-}" == "--minimal" ]]; then
  MINIMAL=1
fi

export DEBIAN_FRONTEND=noninteractive

wait_dpkg() {
  local i=0
  while sudo fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 20 ]; then
      echo "==> dpkg lock still held after 40s; trying apt anyway"
      break
    fi
    sleep 2
  done
}

# Never wipe apt lists and retry install without update — that leaves an empty
# cache and every package becomes "Unable to locate".
apt_retry() {
  local n=0 max=4 delay=10
  wait_dpkg
  until sudo timeout 300 "$@"; do
    n=$((n + 1))
    if [ "$n" -ge "$max" ]; then
      echo "apt command failed after ${max} attempts: $*" >&2
      return 1
    fi
    echo "==> apt failed (attempt ${n}/${max}), retrying in ${delay}s: $*"
    sudo rm -rf /var/lib/apt/lists/*
    sleep "$delay"
    wait_dpkg
    sudo timeout 300 apt-get update -y || true
    wait_dpkg
    delay=$((delay + 10))
  done
}

wait_dpkg
apt_retry apt-get update -y

# glslc / libshaderc-dev live in universe on jammy; ensure it is enabled.
if command -v add-apt-repository >/dev/null 2>&1; then
  sudo add-apt-repository -y universe || true
elif apt-cache policy software-properties-common 2>/dev/null | grep -q Candidate; then
  apt_retry apt-get install -y software-properties-common
  sudo add-apt-repository -y universe || true
fi
apt_retry apt-get update -y

if [[ "$MINIMAL" -eq 1 ]]; then
  apt_retry apt-get install -y clang libclang-dev cmake build-essential pkg-config
  exit 0
fi

apt_retry apt-get install -y clang libclang-dev cmake build-essential pkg-config wget \
  libvulkan-dev spirv-headers spirv-tools libvulkan1 patchelf mesa-vulkan-drivers

# shaderc packages: prefer apt; jammy sometimes needs a second update after universe.
if ! apt_retry apt-get install -y glslc libshaderc-dev; then
  echo "==> apt glslc/libshaderc-dev failed; retrying after universe refresh" >&2
  sudo add-apt-repository -y universe || true
  apt_retry apt-get update -y
  apt_retry apt-get install -y glslc libshaderc-dev
fi

command -v glslc >/dev/null
glslc --version || true
