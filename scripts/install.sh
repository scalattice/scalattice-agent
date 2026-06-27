#!/bin/sh
# Scalattice GPU agent installer
# Usage:
#   curl -fsSL https://scalattice.cloud/install/agent | sh
#   curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_...
set -eu

INSTALL_DIR="${SCALATTICE_INSTALL_DIR:-$HOME/.local/bin}"
LIB_DIR="${SCALATTICE_LIB_DIR:-$HOME/.local/lib/scalattice}"
GITHUB_REPO="${SCALATTICE_AGENT_REPO:-Robottik-Software/Scalattice-Client}"
VERSION="${SCALATTICE_AGENT_VERSION:-latest}"
TOKEN=""
SKIP_DEPS=0
AUTO_REBOOT=0
NO_AUTO_REBOOT=0
REBOOT_REQUIRED=0

HAS_NVIDIA_GPU=0
HAS_AMD_GPU=0
HAS_INTEL_GPU=0

ENV_FILE="$HOME/.config/scalattice/agent.env"
SYSTEMD_ENV_FILE="$HOME/.config/scalattice/agent.systemd.env"
UNIT_FILE="$HOME/.config/systemd/user/scalattice-agent.service"

while [ $# -gt 0 ]; do
  case "$1" in
    --token)
      TOKEN="${2:-}"
      shift 2
      ;;
    --dir)
      INSTALL_DIR="${2:-}"
      shift 2
      ;;
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    --skip-deps)
      SKIP_DEPS=1
      shift
      ;;
    --auto-reboot)
      AUTO_REBOOT=1
      shift
      ;;
    --no-auto-reboot)
      AUTO_REBOOT=0
      NO_AUTO_REBOOT=1
      shift
      ;;
    -h|--help)
      echo "Usage: curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_..."
      echo "       Add --skip-deps to skip automatic GPU driver / library setup"
      echo "       Add --auto-reboot to reboot automatically after NVIDIA driver install"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

mkdir -p "$INSTALL_DIR"

arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) ARCH="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) ARCH="aarch64-unknown-linux-gnu" ;;
  *)
    echo "Unsupported architecture: $arch" >&2
    exit 1
    ;;
esac

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
if [ "$OS" != "linux" ]; then
  echo "This installer supports Linux only. Build from source: https://github.com/$GITHUB_REPO" >&2
  exit 1
fi

BIN_NAME="scalattice-agent"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

read_existing_token() {
  if [ -n "$TOKEN" ]; then
    return 0
  fi

  for file in "$ENV_FILE" "$SYSTEMD_ENV_FILE"; do
    [ -f "$file" ] || continue
    line="$(grep -E '^[[:space:]]*(export[[:space:]]+)?SCALATTICE_AGENT_TOKEN=' "$file" 2>/dev/null | head -1 || true)"
    [ -n "$line" ] || continue
    TOKEN="$(printf '%s\n' "$line" | sed -E "s/^[[:space:]]*(export[[:space:]]+)?SCALATTICE_AGENT_TOKEN=//; s/^['\"]//; s/['\"]$//")"
    [ -n "$TOKEN" ] && return 0
  done
}

detect_compute_hardware() {
  HAS_NVIDIA_GPU=0
  HAS_AMD_GPU=0
  HAS_INTEL_GPU=0

  if command -v lspci >/dev/null 2>&1; then
    pci="$(lspci -nn 2>/dev/null || true)"
    echo "$pci" | grep -Eiq 'nvidia|10de:' && HAS_NVIDIA_GPU=1
    echo "$pci" | grep -Eiq 'amd/ati|advanced micro devices|1002:' && HAS_AMD_GPU=1
    echo "$pci" | grep -Eiq 'intel.*graphics|8086:' && HAS_INTEL_GPU=1
  fi

  if [ "$HAS_NVIDIA_GPU" -eq 0 ]; then
    if [ -f /etc/nv_tegra_release ] || [ -d /sys/module/nvidia ]; then
      HAS_NVIDIA_GPU=1
    fi
  fi
}

describe_compute_hardware() {
  detect_compute_hardware
  echo "==> Compute hardware scan"
  if [ "$HAS_NVIDIA_GPU" -eq 1 ]; then
    echo "    NVIDIA GPU present"
  fi
  if [ "$HAS_AMD_GPU" -eq 1 ]; then
    echo "    AMD GPU present"
  fi
  if [ "$HAS_INTEL_GPU" -eq 1 ]; then
    echo "    Intel graphics present"
  fi
  if [ "$HAS_NVIDIA_GPU" -eq 0 ] && [ "$HAS_AMD_GPU" -eq 0 ] && [ "$HAS_INTEL_GPU" -eq 0 ]; then
    echo "    No discrete GPU detected (CPU-only inference)"
  fi
}

describe_driver_status() {
  if [ "$HAS_NVIDIA_GPU" -eq 1 ]; then
    if nvidia_driver_working; then
      echo "    NVIDIA driver: ready ($(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1 | tr -d ' '))"
    elif libcuda_available; then
      echo "    NVIDIA driver: libcuda found but nvidia-smi is not working (reboot or reinstall driver)"
    else
      echo "    NVIDIA driver: not installed"
    fi
  fi
  if [ "$HAS_AMD_GPU" -eq 1 ]; then
    if amd_driver_working; then
      echo "    AMD ROCm: ready"
    else
      echo "    AMD ROCm: not detected (install ROCm for GPU inference)"
    fi
  fi
}

libcuda_available() {
  ldconfig -p 2>/dev/null | grep -q 'libcuda\.so' && return 0
  for p in \
    /usr/lib/x86_64-linux-gnu/libcuda.so* \
    /usr/lib/aarch64-linux-gnu/libcuda.so* \
    /usr/lib/libcuda.so* \
    /lib/x86_64-linux-gnu/libcuda.so* \
    /lib/aarch64-linux-gnu/libcuda.so*
  do
    for f in $p; do
      [ -e "$f" ] && return 0
    done
  done
  return 1
}

nvidia_driver_working() {
  command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1
}

amd_driver_working() {
  command -v rocm-smi >/dev/null 2>&1 && rocm-smi >/dev/null 2>&1
}

gpu_compute_ready() {
  detect_compute_hardware
  if [ "$HAS_NVIDIA_GPU" -eq 1 ]; then
    nvidia_driver_working
    return $?
  fi
  if [ "$HAS_AMD_GPU" -eq 1 ]; then
    amd_driver_working
    return $?
  fi
  return 0
}

apt_install() {
  pkgs="$1"
  [ -n "$pkgs" ] || return 0

  if ! command -v apt-get >/dev/null 2>&1; then
    echo "==> Install OS packages manually: $pkgs"
    return 1
  fi

  echo "==> Installing host packages: $pkgs"
  if sudo -n apt-get update -qq >/dev/null 2>&1 && sudo -n apt-get install -y -qq $pkgs >/dev/null 2>&1; then
    echo "==> Installed: $pkgs"
    return 0
  fi

  if [ -t 0 ]; then
    echo "==> Sudo required to install: $pkgs"
    if sudo apt-get update && sudo apt-get install -y $pkgs; then
      echo "==> Installed: $pkgs"
      return 0
    fi
  fi

  echo "==> Could not install automatically. Run:"
  echo "    sudo apt-get update && sudo apt-get install -y $pkgs"
  return 1
}

human_bytes() {
  n="$1"
  [ -n "$n" ] || return 1
  case "$n" in
    *[!0-9]*) return 1 ;;
  esac
  if command -v numfmt >/dev/null 2>&1; then
    numfmt --to=iec-i --suffix=B "$n" 2>/dev/null
  else
    echo "${n} bytes"
  fi
}

release_download_size() {
  url="$1"
  if ! command -v curl >/dev/null 2>&1; then
    return 1
  fi
  curl -fsSLI "$url" 2>/dev/null \
    | awk 'tolower($1)=="content-length:" {gsub(/\r/,"",$2); if ($2+0 > 0) size=$2} END {if (size) print size}'
}

download_file() {
  url="$1"
  dest="$2"

  if command -v curl >/dev/null 2>&1; then
    if [ -t 2 ]; then
      curl -fL --progress-bar "$url" -o "$dest"
    else
      curl -fsSL "$url" -o "$dest"
    fi
  elif command -v wget >/dev/null 2>&1; then
    if [ -t 2 ]; then
      wget --progress=bar:force:noscroll -qO "$dest" "$url"
    else
      wget -qO "$dest" "$url"
    fi
  else
    return 1
  fi
}

maybe_reboot() {
  [ "$REBOOT_REQUIRED" -eq 1 ] || return 0
  echo ""
  echo "==> NVIDIA driver installed — reboot required"
  if [ "$AUTO_REBOOT" -eq 1 ]; then
    echo "==> Rebooting now…"
    sudo reboot
    exit 0
  fi
  if [ -t 0 ]; then
    printf "Reboot now? [y/N] "
    read -r ans || ans=""
    case "$ans" in
      y|Y|yes|YES)
        echo "==> Rebooting…"
        sudo reboot
        exit 0
        ;;
    esac
  fi
  echo "==> After reboot, re-run this installer with your token"
  exit 0
}

ensure_nvidia_driver() {
  [ "$HAS_NVIDIA_GPU" -eq 1 ] || return 0
  if nvidia_driver_working; then
    return 0
  fi

  echo "==> NVIDIA GPU detected but driver is not ready"
  if libcuda_available; then
    echo "==> libcuda is present but nvidia-smi failed — driver may need a reboot or reinstall"
  fi

  driver_pkg=""
  if command -v ubuntu-drivers >/dev/null 2>&1; then
    driver_pkg="$(ubuntu-drivers devices 2>/dev/null | awk '/recommended/ {print $3; exit}')"
  fi
  if [ -z "$driver_pkg" ]; then
    driver_pkg="nvidia-driver-550"
  fi

  if apt_install "ubuntu-drivers-common $driver_pkg"; then
    REBOOT_REQUIRED=1
    echo "==> NVIDIA driver installed ($driver_pkg)"
    return 0
  fi

  echo "==> Then reboot and re-run this installer:"
  echo "    sudo apt-get update && sudo apt-get install -y ubuntu-drivers-common $driver_pkg"
  echo "    sudo reboot"
  return 1
}

ensure_vulkan_stack() {
  if [ "$HAS_AMD_GPU" -eq 1 ] || [ "$HAS_INTEL_GPU" -eq 1 ]; then
    apt_install "mesa-vulkan-drivers libvulkan1" || true
  fi
}

needs_nvidia_driver() {
  [ "$SKIP_DEPS" -eq 1 ] && return 1
  detect_compute_hardware
  [ "$HAS_NVIDIA_GPU" -eq 1 ] && ! nvidia_driver_working
}

ensure_gpu_host_ready() {
  [ "$SKIP_DEPS" -eq 1 ] && return 0

  detect_compute_hardware
  describe_driver_status

  if needs_nvidia_driver; then
    echo "==> Setting up NVIDIA driver before starting the agent"
    ensure_nvidia_driver || true
    maybe_reboot
    return 0
  fi

  if ! binary_runs; then
    if [ "$HAS_AMD_GPU" -eq 1 ] || [ "$HAS_INTEL_GPU" -eq 1 ]; then
      echo "==> Setting up Vulkan stack"
      ensure_vulkan_stack
    fi
    ensure_binary_deps
  fi
}

prepare_host() {
  detect_compute_hardware
  if [ "$HAS_NVIDIA_GPU" -eq 1 ]; then
    ensure_nvidia_driver || true
  elif [ "$HAS_AMD_GPU" -eq 1 ]; then
    echo "==> AMD GPU detected"
    ensure_vulkan_stack
    if ! amd_driver_working; then
      echo "==> Install ROCm for AMD GPU inference: https://rocm.docs.amd.com/"
    fi
  elif [ "$HAS_INTEL_GPU" -eq 1 ]; then
    echo "==> Intel GPU detected"
    ensure_vulkan_stack
  fi
}

stop_agent_service() {
  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop scalattice-agent.service 2>/dev/null || true
  fi
}

install_release_libs() {
  [ -d "$TMP/lib" ] || return 0
  [ -n "$(ls -A "$TMP/lib" 2>/dev/null)" ] || return 0
  mkdir -p "$LIB_DIR"
  install -m 0755 "$TMP/lib"/* "$LIB_DIR/"
}

library_path_export() {
  if [ -n "$(ls -A "$LIB_DIR" 2>/dev/null)" ]; then
    printf '%s' "$LIB_DIR"
  fi
}

ensure_binary_deps() {
  bin="$INSTALL_DIR/$BIN_NAME"
  [ -x "$bin" ] || return 0
  [ "$SKIP_DEPS" -eq 1 ] && return 0

  missing="$(ldd "$bin" 2>/dev/null | awk '/not found/ {print $1}' || true)"
  [ -n "$missing" ] || return 0

  echo "$missing" | grep -q 'libcuda.so.1' && ensure_nvidia_driver || true

  apt_pkgs=""
  echo "$missing" | grep -q 'libvulkan.so.1' && apt_pkgs="$apt_pkgs libvulkan1"
  apt_pkgs="$(printf '%s\n' $apt_pkgs | awk 'NF && !seen[$0]++' | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
  [ -n "$apt_pkgs" ] && apt_install "$apt_pkgs" || true
}

binary_runs() {
  bin="$INSTALL_DIR/$BIN_NAME"
  [ -x "$bin" ] && "$bin" --version >/dev/null 2>&1
}

verify_binary() {
  if binary_runs; then
    return 0
  fi

  ensure_binary_deps

  if [ "$REBOOT_REQUIRED" -eq 1 ]; then
    echo "==> Reboot required to load the NVIDIA driver, then re-run this installer"
    return 1
  fi

  if binary_runs; then
    return 0
  fi

  echo "==> Binary failed to start. Diagnostics:"
  ldd "$INSTALL_DIR/$BIN_NAME" 2>/dev/null | grep -E 'not found|cuda|vulkan' || ldd "$INSTALL_DIR/$BIN_NAME" || true
  return 1
}

download_release() {
  if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/$GITHUB_REPO/releases/latest/download/scalattice-agent-${ARCH}.tar.gz"
  else
    URL="https://github.com/$GITHUB_REPO/releases/download/${VERSION}/scalattice-agent-${ARCH}.tar.gz"
  fi

  size_bytes="$(release_download_size "$URL" 2>/dev/null || true)"
  size_label="$(human_bytes "$size_bytes" 2>/dev/null || true)"
  if [ -n "$size_label" ]; then
    echo "==> Downloading release ($size_label)"
  else
    echo "==> Downloading release from GitHub"
  fi

  download_file "$URL" "$TMP/agent.tar.gz"
  tar -xzf "$TMP/agent.tar.gz" -C "$TMP"
  install -m 0755 "$TMP/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
  install_release_libs
}

build_from_source() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "No release binary found and cargo is not installed." >&2
    echo "Install Rust from https://rustup.rs then re-run this script," >&2
    echo "or download a release from https://github.com/$GITHUB_REPO/releases" >&2
    exit 1
  fi

  echo "==> Building from source (may take several minutes)…"
  cargo install --git "https://github.com/${GITHUB_REPO}.git" --locked --root "$TMP/cargo-root" "$BIN_NAME"
  install -m 0755 "$TMP/cargo-root/bin/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
}

install_agent_binary() {
  if download_release 2>/dev/null; then
    return 0
  fi
  echo "==> Release unavailable — building from source"
  build_from_source
}

write_env_files() {
  mkdir -p "$(dirname "$ENV_FILE")"
  lib_path="$(library_path_export)"

  {
    echo "# Scalattice agent environment (source this file: . $ENV_FILE)"
    echo "export PATH=\"$INSTALL_DIR:\$PATH\""
    if [ -n "$lib_path" ]; then
      echo "export LD_LIBRARY_PATH=\"$lib_path:\${LD_LIBRARY_PATH:-}\""
    fi
    if [ -n "$TOKEN" ]; then
      echo "export SCALATTICE_AGENT_TOKEN='$TOKEN'"
    fi
  } > "$ENV_FILE"

  {
    echo "PATH=$INSTALL_DIR:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    if [ -n "$lib_path" ]; then
      echo "LD_LIBRARY_PATH=$lib_path"
    fi
    if [ -n "$TOKEN" ]; then
      echo "SCALATTICE_AGENT_TOKEN=$TOKEN"
    fi
  } > "$SYSTEMD_ENV_FILE"
}

if [ -z "${SCALATTICE_AUTO_REBOOT:-}" ]; then
  :
elif [ "$SCALATTICE_AUTO_REBOOT" = "1" ] || [ "$SCALATTICE_AUTO_REBOOT" = "true" ]; then
  AUTO_REBOOT=1
fi

if [ -n "$TOKEN" ] && [ "${NO_AUTO_REBOOT:-0}" -eq 0 ] && [ "${SCALATTICE_NO_AUTO_REBOOT:-}" != "1" ]; then
  AUTO_REBOOT=1
fi

read_existing_token

describe_compute_hardware
describe_driver_status

echo "==> Installing scalattice-agent to $INSTALL_DIR"
stop_agent_service

if needs_nvidia_driver; then
  echo "==> NVIDIA driver required — installing before agent startup"
  ensure_nvidia_driver || true
  maybe_reboot
fi

if install_agent_binary; then
  :
else
  exit 1
fi

ensure_gpu_host_ready

verify_binary || true

if ! gpu_compute_ready; then
  echo ""
  echo "==> Warning: GPU hardware was detected but compute drivers are not ready."
  echo "    The agent may fall back to CPU until drivers are installed and the machine is rebooted."
  if [ "$HAS_NVIDIA_GPU" -eq 1 ] && ! nvidia_driver_working; then
    echo "    Fix: install NVIDIA drivers, reboot, then re-run this installer."
  fi
  if [ "$HAS_AMD_GPU" -eq 1 ] && ! amd_driver_working; then
    echo "    Fix: install ROCm, then re-run this installer."
  fi
fi

if ! echo ":$PATH:" | grep -q ":$INSTALL_DIR:"; then
  echo ""
  echo "Add $INSTALL_DIR to your PATH:"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

write_env_files

if [ -n "$TOKEN" ] && command -v systemctl >/dev/null 2>&1; then
  export PATH="$INSTALL_DIR:$PATH"
  lib_path="$(library_path_export)"
  if [ -n "$lib_path" ]; then
    export LD_LIBRARY_PATH="$lib_path${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  fi
  "$INSTALL_DIR/$BIN_NAME" service install || {
    echo "==> Could not start background service — run: scalattice-agent connect"
  }
fi

echo ""
echo "Done."
if [ -n "$TOKEN" ]; then
  echo "  Dashboard: https://scalattice.cloud/providers"
else
  echo "  1. Create a machine token at https://scalattice.cloud/providers"
  echo "  2. Run: curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_…"
fi
echo "  Status: scalattice-agent status"
