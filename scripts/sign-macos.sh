#!/usr/bin/env bash
# Codesign + notarize Apple Silicon agent (.app and/or .dmg).
# Must run on macOS. See docs/macos-signing.md.
set -euo pipefail

TARGET="${1:?usage: sign-macos.sh path/to/Scalattice Agent.app [path/to.dmg]}"
DMG="${2:-}"
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

sign_bin() {
  local bin="$1"
  if [[ ! -f "$bin" ]]; then
    echo "Missing binary: $bin" >&2
    exit 1
  fi
  local arch
  arch="$(lipo -archs "$bin" 2>/dev/null || true)"
  if [[ -n "$arch" && "$arch" != "arm64" ]]; then
    echo "Binary is not arm64-only (lipo: ${arch}). Refusing Intel/universal." >&2
    exit 1
  fi
  echo "==> codesign $(basename "$bin")"
  codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" \
    --sign "$IDENTITY" \
    "$bin"
  codesign --verify --verbose=2 "$bin"
}

if [[ -d "$TARGET" ]]; then
  INNER="${TARGET}/Contents/MacOS/scalattice-agent"
  sign_bin "$INNER"
  echo "==> codesign $(basename "$TARGET")"
  codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" \
    --sign "$IDENTITY" \
    "$TARGET"
  codesign --verify --verbose=2 "$TARGET"
elif [[ -f "$TARGET" ]]; then
  sign_bin "$TARGET"
else
  echo "Missing $TARGET" >&2
  exit 1
fi

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

SUBMIT="$DMG"
if [[ -z "$SUBMIT" ]]; then
  echo "==> No .dmg given; skipping notarytool (bare Mach-O cannot be stapled)."
  exit 0
fi
if [[ ! -f "$SUBMIT" ]]; then
  echo "Missing dmg: $SUBMIT" >&2
  exit 1
fi

echo "==> notarytool submit $(basename "$SUBMIT")"
xcrun notarytool submit "$SUBMIT" \
  --key "$KEY_FILE" \
  --key-id "$APPLE_API_KEY_ID" \
  --issuer "$APPLE_API_ISSUER_ID" \
  --wait

echo "==> stapler staple"
xcrun stapler staple "$SUBMIT"
echo "==> Notarized and stapled $(basename "$SUBMIT")"
