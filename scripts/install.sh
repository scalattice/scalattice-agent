#!/bin/sh
# Scalattice GPU agent installer
# Usage:
#   curl -fsSL https://scalattice.cloud/install/agent | sh
#   curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_…
set -eu

INSTALL_DIR="${SCALATTICE_INSTALL_DIR:-$HOME/.local/bin}"
GITHUB_REPO="${SCALATTICE_AGENT_REPO:-Robottik-Software/Scalattice-Client}"
VERSION="${SCALATTICE_AGENT_VERSION:-latest}"
TOKEN=""

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
      echo "Usage: curl -fsSL https://scalattice.cloud/install/agent | sh -s -- --token slt_provider_…"
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

  echo "==> Building scalattice-agent from source (this may take a few minutes)…"
  cargo install --git "https://github.com/${GITHUB_REPO}.git" --locked --root "$TMP/cargo-root" "$BIN_NAME"
  install -m 0755 "$TMP/cargo-root/bin/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
}

echo "==> Installing scalattice-agent to $INSTALL_DIR"

if download_release 2>/dev/null; then
  echo "==> Installed release binary"
else
  echo "==> Release download unavailable, falling back to source build…"
  build_from_source
fi

ENV_FILE="$HOME/.config/scalattice/agent.env"
mkdir -p "$(dirname "$ENV_FILE")"

needs_path=
if ! echo ":$PATH:" | grep -q ":$INSTALL_DIR:"; then
  needs_path=1
  echo ""
  echo "Add $INSTALL_DIR to your PATH:"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

if [ -n "$TOKEN" ] || [ -n "$needs_path" ]; then
  {
    echo "# Scalattice agent environment (source this file: . $ENV_FILE)"
    echo "export PATH=\"$INSTALL_DIR:\$PATH\""
    if [ -n "$TOKEN" ]; then
      echo "export SCALATTICE_AGENT_TOKEN='$TOKEN'"
      echo "export SCALATTICE_AGENT_WS='wss://api.scalattice.cloud/v1/operators/agent/ws'"
    fi
  } > "$ENV_FILE"
  echo "==> Wrote $ENV_FILE"
fi

echo ""
echo "Done. Next steps:"
echo "  1. Create an agent token at https://scalattice.cloud/providers"
if [ -n "$TOKEN" ]; then
  echo "  2. source $ENV_FILE"
else
  echo "  2. export SCALATTICE_AGENT_TOKEN=slt_provider_…"
fi
echo "  3. scalattice-agent status"
echo "  4. scalattice-agent connect"
echo ""
echo "Demo / connectivity testing without model weights:"
echo "  SCALATTICE_AGENT_DEMO=1 scalattice-agent connect"
