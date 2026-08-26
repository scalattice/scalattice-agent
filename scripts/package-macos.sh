#!/usr/bin/env bash
# Build Scalattice Agent.app + ScalatticeAgentSetup-aarch64.dmg from dist/scalattice-agent.
# Must run on macOS. See docs/macos-signing.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="${ROOT}/dist"
BIN="${DIST}/scalattice-agent"
APP_NAME="Scalattice Agent.app"
APP="${DIST}/${APP_NAME}"
DMG="${DIST}/ScalatticeAgentSetup-aarch64.dmg"
VERSION="${SCALATTICE_VERSION:-}"
DMG_FROM_APP=0
if [[ "${1:-}" == "--dmg-from-app" ]]; then
  DMG_FROM_APP=1
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "package-macos.sh must run on macOS." >&2
  exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
  echo "Refusing to package on Intel ($(uname -m)). Apple Silicon only." >&2
  exit 1
fi
if [[ ! -f "$BIN" && "$DMG_FROM_APP" -eq 0 ]]; then
  echo "Missing $BIN — build the Metal binary first." >&2
  exit 1
fi
if [[ "$DMG_FROM_APP" -eq 1 && ! -d "$APP" ]]; then
  echo "Missing $APP — run package-macos.sh before --dmg-from-app." >&2
  exit 1
fi

if [[ -z "$VERSION" ]]; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -1)"
fi

if [[ "$DMG_FROM_APP" -eq 0 ]]; then
  rm -rf "$APP"
  mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
  cp "$BIN" "${APP}/Contents/MacOS/scalattice-agent"
  chmod +x "${APP}/Contents/MacOS/scalattice-agent"

  sed \
    -e "s/<string>1\\.1\\.[0-9][0-9]*<\\/string>/<string>${VERSION}<\\/string>/" \
    "${ROOT}/installer/macos/Info.plist" > "${APP}/Contents/Info.plist"
else
  cp "${APP}/Contents/MacOS/scalattice-agent" "$BIN"
  chmod +x "$BIN"
fi
STAGE="$(mktemp -d /tmp/scalattice-dmg.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

# GHA macOS runners occasionally leave diskimages-helper wedged ("Resource busy").
hdiutil_create() {
  local dest="$1" attempt=1 max=6
  while true; do
    rm -f "$dest"
    # Best-effort cleanup of leftover Scalattice attaches from prior steps/jobs.
    while IFS= read -r dev; do
      [[ -n "$dev" ]] || continue
      hdiutil detach -force "$dev" >/dev/null 2>&1 || true
    done < <(hdiutil info 2>/dev/null | awk '
      /^\/dev\// { dev=$1 }
      /[Ss]calattice/ { if (dev != "") print dev; dev="" }
    ' | sort -u)
    if hdiutil create \
      -volname "Scalattice Agent" \
      -srcfolder "$STAGE" \
      -ov -format UDZO \
      "$dest" >/dev/null; then
      return 0
    fi
    if (( attempt >= max )); then
      echo "hdiutil create failed after ${max} attempts" >&2
      return 1
    fi
    echo "==> hdiutil create busy/failed (attempt ${attempt}/${max}); retrying…" >&2
    sleep $((attempt * 2))
    attempt=$((attempt + 1))
  done
}

hdiutil_create "$DMG"

tar -czf "${DIST}/scalattice-agent-aarch64-apple-darwin.tar.gz" -C "$DIST" scalattice-agent

echo "==> Packed ${APP_NAME} (v${VERSION})"
echo "==> ${DMG}"
echo "==> ${DIST}/scalattice-agent-aarch64-apple-darwin.tar.gz"
