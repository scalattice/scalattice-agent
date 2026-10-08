#!/usr/bin/env bash
# Install Linux release/CI build deps. Safe on Ubuntu 22.04 (jammy) and 24.04.
# Usage: ci-apt-linux-build-deps.sh [--minimal]
#   --minimal  clang/cmake only (CI cargo check)
#   default    + Vulkan / glslc (release builds)
#
# Jammy apt Vulkan headers are 1.3.204 — too old for current llama.cpp Vulkan.
# On 22.04 we install the full LunarG Vulkan SDK (headers + glslc + libs) and
# export VULKAN_SDK so CMake prefers it over /usr.
set -euo pipefail

MINIMAL=0
if [[ "${1:-}" == "--minimal" ]]; then
  MINIMAL=1
fi

export DEBIAN_FRONTEND=noninteractive
VULKAN_SDK_VERSION="${VULKAN_SDK_VERSION:-1.3.296.0}"

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

export_github_env() {
  local key="$1" val="$2"
  if [[ -n "${GITHUB_ENV:-}" ]]; then
    # multiline-safe: values here are single-line paths
    echo "${key}=${val}" >>"$GITHUB_ENV"
  fi
  export "${key}=${val}"
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
  libvulkan1 patchelf mesa-vulkan-drivers

# On jammy, skip apt libvulkan-dev / spirv (ancient headers). Prefer LunarG SDK.
if [[ "$UBUNTU_CODENAME" != "jammy" ]]; then
  apt_retry apt-get install -y libvulkan-dev spirv-headers spirv-tools || true
  apt_retry apt-get install -y glslc libshaderc-dev || true
fi

install_vulkan_sdk_from_lunarg() {
  local ver="$VULKAN_SDK_VERSION"
  local arch
  arch="$(uname -m)"
  if [[ "$arch" != "x86_64" ]]; then
    echo "LunarG Linux SDK fallback is x86_64-only (got ${arch})" >&2
    return 1
  fi
  local url="https://sdk.lunarg.com/sdk/download/${ver}/linux/vulkansdk-linux-x86_64-${ver}.tar.xz"
  local tarball="/tmp/vulkansdk-linux-x86_64-${ver}.tar.xz"
  local sdk_root="/opt/vulkan-sdk/${ver}/x86_64"
  echo "==> downloading LunarG Vulkan SDK ${ver} (headers + glslc)"
  curl -fsSL --retry 5 --retry-delay 5 -o "$tarball" "$url"
  sudo mkdir -p /opt/vulkan-sdk
  sudo tar -xJf "$tarball" -C /opt/vulkan-sdk
  rm -f "$tarball"
  if [[ ! -d "$sdk_root" ]]; then
    echo "SDK root missing after extract: ${sdk_root}" >&2
    find /opt/vulkan-sdk -maxdepth 3 -type d 2>/dev/null | head -40 >&2 || true
    return 1
  fi
  if [[ ! -x "${sdk_root}/bin/glslc" ]]; then
    echo "glslc missing in SDK at ${sdk_root}/bin/glslc" >&2
    return 1
  fi
  if [[ ! -f "${sdk_root}/include/vulkan/vulkan.hpp" && ! -f "${sdk_root}/include/vulkan/vulkan_core.h" ]]; then
    echo "Vulkan headers missing under ${sdk_root}/include/vulkan" >&2
    ls -la "${sdk_root}/include" 2>/dev/null | head -20 >&2 || true
    return 1
  fi
  sudo ln -sfn "$sdk_root" /opt/vulkan-sdk/current
  sudo install -m 755 "${sdk_root}/bin/glslc" /usr/local/bin/glslc
  sudo ln -sfn /usr/local/bin/glslc /usr/bin/glslc

  export_github_env VULKAN_SDK "$sdk_root"
  export_github_env Vulkan_GLSLC_EXECUTABLE "${sdk_root}/bin/glslc"
  # SDK first so FindVulkan does not pick jammy's 1.3.204 headers from /usr.
  export_github_env CMAKE_PREFIX_PATH "${sdk_root}:/usr"
  export_github_env LD_LIBRARY_PATH "${sdk_root}/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  # Prepend SDK bin for subsequent steps.
  if [[ -n "${GITHUB_PATH:-}" ]]; then
    echo "${sdk_root}/bin" >>"$GITHUB_PATH"
  fi
  echo "==> VULKAN_SDK=${sdk_root}"
  echo "==> vulkan.hpp present: $(test -f "${sdk_root}/include/vulkan/vulkan.hpp" && echo yes || echo no)"
}

if [[ "$UBUNTU_CODENAME" == "jammy" ]]; then
  install_vulkan_sdk_from_lunarg
elif ! command -v glslc >/dev/null 2>&1; then
  echo "==> apt glslc unavailable; falling back to LunarG SDK" >&2
  install_vulkan_sdk_from_lunarg
fi

command -v glslc >/dev/null
glslc --version || true
echo "==> Vulkan_GLSLC_EXECUTABLE=${Vulkan_GLSLC_EXECUTABLE:-$(command -v glslc)}"
echo "==> VULKAN_SDK=${VULKAN_SDK:-}"
echo "==> CMAKE_PREFIX_PATH=${CMAKE_PREFIX_PATH:-}"
