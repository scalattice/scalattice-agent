#!/usr/bin/env bash
# Install Linux release/CI build deps. Safe on Ubuntu 22.04 (jammy) and 24.04.
# Usage: ci-apt-linux-build-deps.sh [--minimal]
#   --minimal  clang/cmake only (CI cargo check)
#   default    + Vulkan / glslc (release builds)
#
# Note: jammy has no apt packages for glslc/libshaderc-dev (those land in noble).
# On 22.04 we install glslc from the LunarG Vulkan SDK tarball instead.
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

. /etc/os-release
UBUNTU_CODENAME="${VERSION_CODENAME:-}"

wait_dpkg
apt_retry apt-get update -y

if [[ "$MINIMAL" -eq 1 ]]; then
  apt_retry apt-get install -y clang libclang-dev cmake build-essential pkg-config
  exit 0
fi

apt_retry apt-get install -y clang libclang-dev cmake build-essential pkg-config wget curl \
  gcc g++ libgomp1 \
  libvulkan-dev spirv-headers spirv-tools libvulkan1 patchelf mesa-vulkan-drivers

install_glslc_from_apt() {
  apt_retry apt-get install -y glslc libshaderc-dev
}

# LunarG ships a ready glslc; jammy apt does not.
install_glslc_from_lunarg() {
  local ver="${VULKAN_SDK_VERSION:-1.3.296.0}"
  local arch
  arch="$(uname -m)"
  if [[ "$arch" != "x86_64" ]]; then
    echo "LunarG Linux SDK glslc fallback is x86_64-only (got ${arch})" >&2
    return 1
  fi
  local url="https://sdk.lunarg.com/sdk/download/${ver}/linux/vulkansdk-linux-x86_64-${ver}.tar.xz"
  local tarball="/tmp/vulkansdk-linux-x86_64-${ver}.tar.xz"
  echo "==> downloading LunarG Vulkan SDK ${ver} for glslc"
  curl -fsSL --retry 5 --retry-delay 5 -o "$tarball" "$url"
  tar -xJf "$tarball" -C /tmp
  local glslc_src="/tmp/${ver}/x86_64/bin/glslc"
  if [[ ! -x "$glslc_src" ]]; then
    echo "glslc not found in SDK tarball at ${glslc_src}" >&2
    find "/tmp/${ver}" -name glslc 2>/dev/null | head -20 >&2 || true
    return 1
  fi
  sudo install -m 755 "$glslc_src" /usr/local/bin/glslc
  # Keep /usr/bin/glslc working for workflows that hardcode that path.
  sudo ln -sfn /usr/local/bin/glslc /usr/bin/glslc
  rm -f "$tarball"
}

if command -v glslc >/dev/null 2>&1; then
  echo "==> glslc already present: $(command -v glslc)"
elif [[ "$UBUNTU_CODENAME" == "jammy" ]]; then
  install_glslc_from_lunarg
elif install_glslc_from_apt; then
  :
else
  echo "==> apt glslc unavailable; falling back to LunarG SDK" >&2
  install_glslc_from_lunarg
fi

command -v glslc >/dev/null
glslc --version || true
echo "==> Vulkan_GLSLC_EXECUTABLE=$(command -v glslc)"
