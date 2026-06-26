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
STATE_FILE="$HOME/.config/scalattice/agent.state.json"
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

  # Jetson / Tegra often omit lspci labels — check platform and loaded driver.
  if [ "$HAS_NVIDIA_GPU" -eq 0 ]; then
    if [ -f /etc/nv_tegra_release ] || [ -d /sys/module/nvidia ]; then
      HAS_NVIDIA_GPU=1
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

maybe_reboot() {
  [ "$REBOOT_REQUIRED" -eq 1 ] || return 0
  echo ""
  echo "==> NVIDIA driver installed — reboot required"
  if [ "$AUTO_REBOOT" -eq 1 ]; then
    echo "==> Rebooting now (SCALATTICE_AUTO_REBOOT=1)…"
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
  nvidia_driver_working && return 0
  libcuda_available && return 0

  echo "==> NVIDIA GPU detected but CUDA driver is not ready"

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
  # Loader is bundled in our release, but AMD/Intel need a Vulkan ICD from Mesa.
  if [ "$HAS_AMD_GPU" -eq 1 ] || [ "$HAS_INTEL_GPU" -eq 1 ]; then
    apt_install "mesa-vulkan-drivers libvulkan1" || true
  fi
}

prepare_host() {
  [ "$SKIP_DEPS" -eq 1 ] && return 0

  echo "==> Detecting compute hardware"
  detect_compute_hardware

  if [ "$HAS_NVIDIA_GPU" -eq 1 ]; then
    echo "==> NVIDIA GPU detected"
    ensure_nvidia_driver || true
  elif [ "$HAS_AMD_GPU" -eq 1 ]; then
    echo "==> AMD GPU detected"
    ensure_vulkan_stack
  elif [ "$HAS_INTEL_GPU" -eq 1 ]; then
    echo "==> Intel GPU detected"
    ensure_vulkan_stack
  else
    echo "==> No discrete GPU detected — agent will use CPU inference if needed"
  fi
}

remove_previous_install() {
  echo "==> Removing previous scalattice-agent install"

  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop scalattice-agent.service 2>/dev/null || true
    systemctl --user disable scalattice-agent.service 2>/dev/null || true
    systemctl --user daemon-reload 2>/dev/null || true
  fi

  rm -f "$STATE_FILE" "$ENV_FILE" "$SYSTEMD_ENV_FILE" "$UNIT_FILE"
  rm -rf "$LIB_DIR"
}

install_release_libs() {
  [ -d "$TMP/lib" ] || return 0
  [ -n "$(ls -A "$TMP/lib" 2>/dev/null)" ] || return 0
  mkdir -p "$LIB_DIR"
  install -m 0755 "$TMP"/lib/* "$LIB_DIR/"
  echo "==> Installed runtime libraries to $LIB_DIR"
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

verify_binary() {
  bin="$INSTALL_DIR/$BIN_NAME"
  if "$bin" --version >/dev/null 2>&1; then
    return 0
  fi

  ensure_binary_deps

  if [ "$REBOOT_REQUIRED" -eq 1 ]; then
    echo "==> Reboot required to load the NVIDIA driver, then re-run this installer"
    return 1
  fi

  if "$bin" --version >/dev/null 2>&1; then
    return 0
  fi

  echo "==> Binary failed to start. Diagnostics:"
  ldd "$bin" 2>/dev/null | grep -E 'not found|cuda|vulkan' || ldd "$bin" || true
  return 1
}

download_release() {
  if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/$GITHUB_REPO/releases/latest/download/scalattice-agent-${ARCH}.tar.gz"
  else
    URL="https://github.com/$GITHUB_REPO/releases/download/${VERSION}/scalattice-agent-${ARCH}.tar.gz"
  fi

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$URL" -o "$TMP/agent.tar.gz"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TMP/agent.tar.gz" "$URL"
  else
    return 1
  fi

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

  echo "==> Building scalattice-agent from source (this may take a few minutes)..."
  cargo install --git "https://github.com/${GITHUB_REPO}.git" --locked --root "$TMP/cargo-root" "$BIN_NAME"
  install -m 0755 "$TMP/cargo-root/bin/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
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
      echo "export SCALATTICE_AGENT_WS='wss://api.scalattice.cloud/v1/operators/agent/ws'"
    fi
  } > "$ENV_FILE"

  {
    echo "PATH=$INSTALL_DIR:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    if [ -n "$lib_path" ]; then
      echo "LD_LIBRARY_PATH=$lib_path"
    fi
    if [ -n "$TOKEN" ]; then
      echo "SCALATTICE_AGENT_TOKEN=$TOKEN"
      echo "SCALATTICE_AGENT_WS=wss://api.scalattice.cloud/v1/operators/agent/ws"
    fi
  } > "$SYSTEMD_ENV_FILE"

  echo "==> Wrote $ENV_FILE"
}

enable_boot_linger() {
  USER_NAME="$(id -un)"
  if ! command -v loginctl >/dev/null 2>&1; then
    return 0
  fi
  if loginctl show-user "$USER_NAME" -p Linger --value 2>/dev/null | grep -q '^yes$'; then
    echo "==> Boot without login: already enabled"
    return 0
  fi
  if sudo -n loginctl enable-linger "$USER_NAME" 2>/dev/null; then
    echo "==> Boot without login: enabled"
    return 0
  fi
  echo "==> Boot without login: needs sudo - run once:"
  echo "    sudo loginctl enable-linger $USER_NAME"
}

if [ -z "${SCALATTICE_AUTO_REBOOT:-}" ]; then
  :
elif [ "$SCALATTICE_AUTO_REBOOT" = "1" ] || [ "$SCALATTICE_AUTO_REBOOT" = "true" ]; then
  AUTO_REBOOT=1
fi

# Token installs are cloud-managed: reboot after NVIDIA driver setup unless opted out.
if [ -n "$TOKEN" ] && [ "${NO_AUTO_REBOOT:-0}" -eq 0 ] && [ "${SCALATTICE_NO_AUTO_REBOOT:-}" != "1" ]; then
  AUTO_REBOOT=1
fi

read_existing_token
prepare_host
maybe_reboot

remove_previous_install

echo "==> Installing scalattice-agent to $INSTALL_DIR"

if download_release 2>/dev/null; then
  echo "==> Installed release binary"
else
  echo "==> Release download unavailable, falling back to source build..."
  build_from_source
fi

verify_binary || true

needs_path=
if ! echo ":$PATH:" | grep -q ":$INSTALL_DIR:"; then
  needs_path=1
  echo ""
  echo "Add $INSTALL_DIR to your PATH:"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

write_env_files

if [ -n "$TOKEN" ] && command -v systemctl >/dev/null 2>&1; then
  echo "==> Installing background service"
  export PATH="$INSTALL_DIR:$PATH"
  lib_path="$(library_path_export)"
  if [ -n "$lib_path" ]; then
    export LD_LIBRARY_PATH="$lib_path${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  fi
  if "$INSTALL_DIR/$BIN_NAME" service install; then
    echo "==> Agent is running in the background"
  else
    echo "==> Could not start background service - after sourcing agent.env, run: scalattice-agent connect"
  fi
fi

if [ -n "$TOKEN" ]; then
  enable_boot_linger
fi

echo ""
echo "Done."
if [ -n "$TOKEN" ]; then
  echo "  This machine will appear on https://scalattice.cloud/providers within a minute."
  echo "  Manage GPUs, models, demo mode, and schedules from the dashboard."
else
  echo "  1. Create a machine token at https://scalattice.cloud/providers"
  echo "  2. Run: scalattice-agent set-token --token slt_provider_…"
  echo "     or re-run: curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_…"
  echo "  Without a token this machine will not appear on the dashboard."
fi
echo "  Check connection: scalattice-agent status"
