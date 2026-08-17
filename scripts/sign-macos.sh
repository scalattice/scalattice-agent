#!/usr/bin/env bash
# Codesign + notarize Apple Silicon agent (.app and/or .dmg).
# Must run on macOS. See docs/macos-signing.md.
set -euo pipefail

TARGET="${1:?usage: sign-macos.sh path/to/Scalattice Agent.app [path/to.dmg]}"
DMG="${2:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENTITLEMENTS="${ROOT}/installer/macos/entitlements.plist"
KEYCHAIN="${APPLE_KEYCHAIN:-}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "sign-macos.sh must run on macOS (Metal/codesign/notarytool)." >&2
  exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
  echo "Refusing to sign on Intel ($(uname -m)). Apple Silicon only." >&2
  exit 1
fi

# codesign matches a valid identity in the keychain, not the secret string.
# Prefer the SHA-1 hash so a trailing newline / Ltd vs LTD in the secret cannot fail.
resolve_identity() {
  local want line
  want="$(printf '%s' "${APPLE_SIGNING_IDENTITY:-}" | tr -d '\r' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
  if [[ -n "$want" ]]; then
    line="$(security find-identity -v -p codesigning | grep -F "$want" | head -1 || true)"
  fi
  if [[ -z "${line:-}" ]]; then
    line="$(security find-identity -v -p codesigning | grep 'Developer ID Application:' | head -1 || true)"
  fi
  if [[ -z "${line:-}" ]]; then
    echo "No valid Developer ID Application identity in the keychain." >&2
    security find-identity -p codesigning >&2 || true
    exit 1
  fi
  awk '{print $2}' <<<"$line"
}

IDENTITY="$(resolve_identity)"
echo "==> signing identity hash ${IDENTITY}"

CODESIGN_BASE=(
  --force --options runtime --timestamp
  --entitlements "$ENTITLEMENTS"
  --sign "$IDENTITY"
)
if [[ -n "$KEYCHAIN" ]]; then
  CODESIGN_BASE+=(--keychain "$KEYCHAIN")
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
  codesign "${CODESIGN_BASE[@]}" "$bin"
  codesign --verify --verbose=2 "$bin"
}

if [[ -d "$TARGET" ]]; then
  INNER="${TARGET}/Contents/MacOS/scalattice-agent"
  sign_bin "$INNER"
  echo "==> codesign $(basename "$TARGET")"
  codesign "${CODESIGN_BASE[@]}" "$TARGET"
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
