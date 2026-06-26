#!/bin/sh
# Scalattice GPU agent installer
# Usage:
#   curl -fsSL https://scalattice.cloud/install/agent | sh
#   curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_...
set -eu

INSTALL_DIR="${SCALATTICE_INSTALL_DIR:-$HOME/.local/bin}"
GITHUB_REPO="${SCALATTICE_AGENT_REPO:-Robottik-Software/Scalattice-Client}"
VERSION="${SCALATTICE_AGENT_VERSION:-latest}"
TOKEN=""

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
    -h|--help)
      echo "Usage: curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_..."
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

remove_previous_install() {
  echo "==> Removing previous scalattice-agent install"

  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop scalattice-agent.service 2>/dev/null || true
    systemctl --user disable scalattice-agent.service 2>/dev/null || true
    systemctl --user daemon-reload 2>/dev/null || true
  fi

  rm -f "$STATE_FILE" "$ENV_FILE" "$SYSTEMD_ENV_FILE" "$UNIT_FILE"
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

  {
    echo "# Scalattice agent environment (source this file: . $ENV_FILE)"
    echo "export PATH=\"$INSTALL_DIR:\$PATH\""
    if [ -n "$TOKEN" ]; then
      echo "export SCALATTICE_AGENT_TOKEN='$TOKEN'"
      echo "export SCALATTICE_AGENT_WS='wss://api.scalattice.cloud/v1/operators/agent/ws'"
    fi
  } > "$ENV_FILE"

  {
    echo "PATH=$INSTALL_DIR:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
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

read_existing_token
remove_previous_install

echo "==> Installing scalattice-agent to $INSTALL_DIR"

if download_release 2>/dev/null; then
  echo "==> Installed release binary"
else
  echo "==> Release download unavailable, falling back to source build..."
  build_from_source
fi

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
echo "Done. Next steps:"
step=1
if [ -z "$TOKEN" ]; then
  echo "  $step. Create an agent token at https://scalattice.cloud/providers"
  step=$((step + 1))
  echo "  $step. Re-run this installer with --token slt_provider_..."
  step=$((step + 1))
fi
if [ -n "$TOKEN" ] || [ -n "$needs_path" ]; then
  echo "  $step. source $HOME/.config/scalattice/agent.env"
  step=$((step + 1))
fi
echo "  $step. scalattice-agent status"
step=$((step + 1))
echo "  $step. scalattice-agent connect"
echo ""
echo "Debug in foreground: scalattice-agent connect --foreground"
