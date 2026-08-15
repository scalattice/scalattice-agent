#!/usr/bin/env bash
# Codesign + notarize an Apple Silicon scalattice-agent binary.
# Must run on macOS (GitHub macos-14 or a Mac). See docs/macos-signing.md.
set -euo pipefail

BIN="${1:?usage: sign-macos.sh path/to/scalattice-agent}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENTITLEMENTS="${ROOT}/installer/macos/entitlements.plist"
IDENTITY="${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "sign-macos.sh must run on macOS (Metal/codesign/notarytool)." >&2
  exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
  echo "Refusing to sign on Intel ($(uname -m)). Apple Silicon only." >&2
  exit 1
fi
if [[ ! -f "$BIN" ]]; then
  echo "Missing binary: $BIN" >&2
  exit 1
fi

arch="$(lipo -archs "$BIN" 2>/dev/null || true)"
if [[ -n "$arch" && "$arch" != "arm64" ]]; then
  echo "Binary is not arm64-only (lipo: ${arch}). Refusing Intel/universal." >&2
  exit 1
fi

echo "==> codesign (hardened runtime)"
codesign --force --options runtime --timestamp \
  --entitlements "$ENTITLEMENTS" \
  --sign "$IDENTITY" \
  "$BIN"
codesign --verify --verbose=2 "$BIN"

if [[ -z "${APPLE_API_KEY_ID:-}" || -z "${APPLE_API_ISSUER_ID:-}" ]]; then
  echo "==> Skipping notarization (APPLE_API_KEY_ID / APPLE_API_ISSUER_ID not set)"
  exit 0
fi

KEY_FILE="${APPLE_API_KEY_P8_FILE:-}"
CLEANUP_KEY=""
if [[ -z "$KEY_FILE" ]]; then
  if [[ -z "${APPLE_API_KEY_P8:-}" ]]; then
    echo "Set APPLE_API_KEY_P8 or APPLE_API_KEY_P8_FILE for notarization." >&2
    exit 1
  fi
  KEY_FILE="$(mktemp /tmp/AuthKey.XXXXXX.p8)"
  CLEANUP_KEY="$KEY_FILE"
  printf '%s\n' "$APPLE_API_KEY_P8" >"$KEY_FILE"
  trap 'rm -f "$CLEANUP_KEY"' EXIT
fi

WORKDIR="$(mktemp -d /tmp/scalattice-notary.XXXXXX)"
ZIP="${WORKDIR}/scalattice-agent.zip"
(
  cd "$(dirname "$BIN")"
  zip -j "$ZIP" "$(basename "$BIN")"
)

echo "==> notarytool submit"
xcrun notarytool submit "$ZIP" \
  --key "$KEY_FILE" \
  --key-id "$APPLE_API_KEY_ID" \
  --issuer "$APPLE_API_ISSUER_ID" \
  --wait

echo "==> Notarized. Stapling is not supported for a bare Mach-O; Gatekeeper uses the online ticket."
rm -rf "$WORKDIR"
