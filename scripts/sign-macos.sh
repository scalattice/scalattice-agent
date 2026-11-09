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

keychain_args=()
if [[ -n "$KEYCHAIN" ]]; then
  keychain_args=(--keychain "$KEYCHAIN")
fi

# Apple's timestamp service flakes ("The timestamp service is not available").
codesign_retry() {
  local n=0 max=6 delay=5
  until codesign "$@"; do
    n=$((n + 1))
    if [ "$n" -ge "$max" ]; then
      echo "codesign failed after ${max} attempts: $*" >&2
      return 1
    fi
    echo "==> codesign failed (attempt ${n}/${max}); retrying in ${delay}s: $*"
    sleep "$delay"
    delay=$((delay + 5))
  done
}

# Unique bundle id per nested Mach-O. Reusing com.scalattice.agent on every
# library makes Apple's nested-code checks and ticket lookup flaky.
nested_identifier() {
  local path="$1" base safe
  base="$(basename "$path")"
  safe="$(printf '%s' "$base" | tr -c 'A-Za-z0-9._-' '_')"
  echo "com.scalattice.agent.${safe}"
}

is_macho() {
  local path="$1" kind
  [[ -f "$path" && ! -L "$path" ]] || return 1
  kind="$(file -b "$path" 2>/dev/null || true)"
  [[ "$kind" == *Mach-O* ]]
}

is_shared_lib() {
  local path="$1" base kind
  base="$(basename "$path")"
  case "$base" in
    *.dylib|*.so|*.so.*) return 0 ;;
  esac
  kind="$(file -b "$path" 2>/dev/null || true)"
  [[ "$kind" == *"shared library"* || "$kind" == *"dynamically linked shared library"* || "$kind" == *"bundle"* ]]
}

sign_macho() {
  local path="$1"
  local with_entitlements="${2:-0}"
  local arch id
  arch="$(lipo -archs "$path" 2>/dev/null || true)"
  if [[ -n "$arch" && "$arch" != *"arm64"* ]]; then
    echo "Skipping non-arm64 Mach-O: $path (lipo: ${arch})" >&2
    return 0
  fi
  if [[ -n "$arch" && "$arch" == *"x86_64"* && "$arch" != "arm64" ]]; then
    echo "Binary is not arm64-only (lipo: ${arch}): $path" >&2
    exit 1
  fi
  id="$(nested_identifier "$path")"
  # Main agent keeps the stable public identifier.
  if [[ "$(basename "$path")" == "scalattice-agent" ]]; then
    id="com.scalattice.agent"
  fi
  echo "==> codesign $(basename "$path") ($id)"
  if [[ "$with_entitlements" == "1" ]]; then
    codesign_retry --force --options runtime --timestamp \
      --identifier "$id" \
      --entitlements "$ENTITLEMENTS" \
      "${keychain_args[@]}" \
      --sign "$IDENTITY" \
      "$path"
  else
    # Libraries / plugins: hardened runtime + timestamp, no entitlements.
    codesign_retry --force --options runtime --timestamp \
      --identifier "$id" \
      "${keychain_args[@]}" \
      --sign "$IDENTITY" \
      "$path"
  fi
  codesign --verify --verbose=2 "$path"
}

# Sign every nested Mach-O inside-out, then the app bundle. TTS/SD ship ggml
# backends as *.so modules; sealing them as unsigned resources makes notarization
# return Invalid (stapler then fails with Error 65 / Record not found).
sign_app_bundle() {
  local app="$1"
  local inner="${app}/Contents/MacOS/scalattice-agent"
  local path
  local -a libs=() helpers=()

  if [[ ! -f "$inner" ]]; then
    echo "Missing app executable: $inner" >&2
    exit 1
  fi

  while IFS= read -r -d '' path; do
    is_macho "$path" || continue
    case "$path" in
      "$inner") continue ;;
    esac
    if is_shared_lib "$path"; then
      libs+=("$path")
    else
      helpers+=("$path")
    fi
  done < <(find "$app" -type f -print0)

  # Deepest paths first so dependents are signed after their libraries.
  # Tab-separated: app path contains spaces ("Scalattice Agent.app").
  if ((${#libs[@]})); then
    while IFS= read -r path; do
      [[ -n "$path" ]] || continue
      sign_macho "$path" 0
    done < <(printf '%s\n' "${libs[@]}" | awk '{ printf "%08d\t%s\n", length($0), $0 }' | sort -nr | cut -f2-)
  fi
  if ((${#helpers[@]})); then
    while IFS= read -r path; do
      [[ -n "$path" ]] || continue
      # Helper tools (scalattice-sd, llama-tts) need the same JIT entitlements.
      sign_macho "$path" 1
    done < <(printf '%s\n' "${helpers[@]}" | awk '{ printf "%08d\t%s\n", length($0), $0 }' | sort -nr | cut -f2-)
  fi

  sign_macho "$inner" 1
  echo "==> codesign $(basename "$app")"
  codesign_retry --force --options runtime --timestamp \
    --identifier com.scalattice.agent \
    --entitlements "$ENTITLEMENTS" \
    "${keychain_args[@]}" \
    --sign "$IDENTITY" \
    "$app"
  codesign --verify --deep --strict --verbose=2 "$app"
}

if [[ -d "$TARGET" ]]; then
  sign_app_bundle "$TARGET"
elif [[ -f "$TARGET" ]]; then
  sign_macho "$TARGET" 1
  # Nested-signed copies extracted from the .app fail this check. The curl
  # tarball must be a standalone-signed Mach-O.
  codesign --verify --deep --strict --verbose=4 "$TARGET"
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
CLEANUP_ZIP=""
cleanup_signing_temps() {
  rm -f "$CLEANUP_KEY" "$CLEANUP_ZIP"
}
if [[ -z "$KEY_FILE" ]]; then
  if [[ -z "${APPLE_API_KEY_P8:-}" ]]; then
    echo "Set APPLE_API_KEY_P8 or APPLE_API_KEY_P8_FILE for notarization." >&2
    exit 1
  fi
  KEY_FILE="$(mktemp /tmp/AuthKey.XXXXXX.p8)"
  CLEANUP_KEY="$KEY_FILE"
  printf '%s\n' "$APPLE_API_KEY_P8" >"$KEY_FILE"
  trap cleanup_signing_temps EXIT
fi

notary_auth=(
  --key "$KEY_FILE"
  --key-id "$APPLE_API_KEY_ID"
  --issuer "$APPLE_API_ISSUER_ID"
)

# Dump Apple's rejection JSON when status is Invalid so CI shows the real cause
# instead of a useless stapler Error 65 loop. notarytool often exits 0 on Invalid.
notarize_submit() {
  local submit="$1"
  local label="$2"
  local out id status tmp rc=0
  echo "==> notarytool submit $(basename "$submit")${label:+ $label}"
  tmp="$(mktemp /tmp/scalattice-notary.XXXXXX)"
  set +e
  xcrun notarytool submit "$submit" "${notary_auth[@]}" --wait >"$tmp" 2>&1
  rc=$?
  set -e
  cat "$tmp"
  out="$(cat "$tmp")"
  rm -f "$tmp"
  id="$(printf '%s\n' "$out" | awk '/^[[:space:]]*id:/{print $2; exit}')"
  status="$(printf '%s\n' "$out" | awk '/status:/{print $2}' | tail -1)"
  if [[ "$status" != "Accepted" || "$rc" -ne 0 ]]; then
    echo "==> notarization ${status:-failed} (exit=${rc}, id=${id:-unknown})" >&2
    if [[ -n "$id" ]]; then
      echo "==> notarytool log ${id}" >&2
      xcrun notarytool log "$id" "${notary_auth[@]}" >&2 || true
    fi
    exit 1
  fi
  NOTARY_SUBMISSION_ID="$id"
}

SUBMIT="$DMG"
if [[ -z "$SUBMIT" && -f "$TARGET" ]]; then
  # Bare Mach-O cannot be stapled, but Gatekeeper looks up the notarization
  # ticket by CDHash. Zip and submit so curl | sh installs are notarized.
  # BSD mktemp requires the XXXXXX template at the end of the path.
  CLEANUP_ZIP="$(mktemp /tmp/scalattice-agent-notarize.XXXXXX).zip"
  mv "${CLEANUP_ZIP%.zip}" "$CLEANUP_ZIP"
  SUBMIT="$CLEANUP_ZIP"
  trap cleanup_signing_temps EXIT
  (
    cd "$(dirname "$TARGET")"
    ditto -c -k --keepParent "$(basename "$TARGET")" "$SUBMIT"
  )
  notarize_submit "$SUBMIT" "(standalone CLI)"
  echo "==> Notarized standalone CLI (ticket only; cannot staple a Mach-O)"
  exit 0
fi
if [[ -z "$SUBMIT" ]]; then
  echo "==> No .dmg given; skipping notarytool (bare Mach-O cannot be stapled)."
  exit 0
fi
if [[ ! -f "$SUBMIT" ]]; then
  echo "Missing dmg: $SUBMIT" >&2
  exit 1
fi

notarize_submit "$SUBMIT" ""

# Notarization can be Accepted while CloudKit's ticket lookup drops the
# connection (NSURLError -1005, stapler exit 68). The ticket is already
# issued; staple again instead of failing the release.
echo "==> stapler staple"
stapled=0
for attempt in 1 2 3 4 5 6; do
  if xcrun stapler staple "$SUBMIT"; then
    stapled=1
    break
  fi
  echo "stapler failed (attempt ${attempt}); retrying ticket lookup" >&2
  sleep $((attempt * 15))
done
if [[ "$stapled" -ne 1 ]]; then
  echo "stapler staple failed after retries (submission id=${NOTARY_SUBMISSION_ID:-unknown})" >&2
  exit 1
fi
echo "==> Notarized and stapled $(basename "$SUBMIT")"
