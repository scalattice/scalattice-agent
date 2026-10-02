#!/usr/bin/env bash
# Local canary loop for Onsite/Chillblast: build → stop → swap installed binary → restart.
# Does not touch GitHub releases or other machines. Pause routing in admin while testing.
#
# Usage:
#   ./scripts/dev-swap-linux.sh              # build + swap + restart
#   ./scripts/dev-swap-linux.sh --skip-build # swap existing target/release binary
#   SCALATTICE_VERSION=1.1.129 ./scripts/dev-swap-linux.sh
set -euo pipefail

cd "$(dirname "$0")/.."

SKIP_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    -h|--help)
      sed -n '2,10p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown arg: $arg (try --skip-build)" >&2
      exit 2
      ;;
  esac
done

UNIT="scalattice-agent.service"
WATCHDOG_TIMER="scalattice-agent-watchdog.timer"
SRC="target/release/scalattice-agent"
DEST="${HOME}/.local/bin/scalattice-agent"

if [[ -z "${SCALATTICE_VERSION:-}" ]]; then
  # Match the published tip so prod does not treat this build as "needs update".
  TIP="$(git ls-remote --tags origin 'refs/tags/v*' 2>/dev/null \
    | awk '{print $2}' | sed 's|refs/tags/||; s/\^{}$//' \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sed 's/^v//' \
    | sort -t. -k1,1n -k2,2n -k3,3n | tail -1 || true)"
  if [[ -n "${TIP:-}" ]]; then
    export SCALATTICE_VERSION="$TIP"
  else
    export SCALATTICE_VERSION="$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')"
  fi
fi
echo "==> version stamp: ${SCALATTICE_VERSION}"

if [[ -z "${CUDACXX:-}" && -x /usr/local/cuda/bin/nvcc ]]; then
  export CUDACXX=/usr/local/cuda/bin/nvcc
fi

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "==> cargo build --release --features gpu"
  cargo build --release --features gpu
else
  echo "==> skipping build (--skip-build)"
fi

if [[ ! -x "$SRC" ]]; then
  echo "Missing $SRC — build first (drop --skip-build)." >&2
  exit 1
fi

mkdir -p "$(dirname "$DEST")"

echo "==> stopping background agent (so the binary is not busy)"
if command -v systemctl >/dev/null 2>&1; then
  systemctl --user stop "$WATCHDOG_TIMER" 2>/dev/null || true
  systemctl --user stop "$UNIT" 2>/dev/null || true
fi
# Workers orphaned by process::exit; also any stray foreground.
pkill -f 'scalattice-agent worker' 2>/dev/null || true
pkill -x scalattice-agent 2>/dev/null || true
sleep 0.5
# Last resort if something still holds the inode.
if [[ -e "$DEST" ]] && fuser "$DEST" >/dev/null 2>&1; then
  fuser -k "$DEST" 2>/dev/null || true
  sleep 0.3
fi

echo "==> install $SRC -> $DEST"
cp -f "$SRC" "$DEST"
chmod +x "$DEST"

echo "==> restart"
if command -v systemctl >/dev/null 2>&1 && systemctl --user cat "$UNIT" >/dev/null 2>&1; then
  systemctl --user start "$WATCHDOG_TIMER" 2>/dev/null || true
  systemctl --user restart "$UNIT"
  systemctl --user --no-pager --full status "$UNIT" | head -20 || true
elif command -v scalattice-agent >/dev/null 2>&1; then
  scalattice-agent restart
else
  echo "No systemd unit found; start manually: $DEST foreground" >&2
  exit 1
fi

echo "==> done. Check admin (routing should stay paused while testing)."
"$DEST" --version 2>/dev/null || true
if ! "$DEST" --version 2>/dev/null | grep -qF "${SCALATTICE_VERSION}"; then
  echo "warning: binary version does not match stamp ${SCALATTICE_VERSION}" >&2
fi
