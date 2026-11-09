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
APP_ONLY=0
case "${1:-}" in
  --dmg-from-app) DMG_FROM_APP=1 ;;
  --app-only) APP_ONLY=1 ;;
esac

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "package-macos.sh must run on macOS." >&2
  exit 1
fi
if [[ "$(uname -m)" != "arm64" ]]; then
  echo "Refusing to package on Intel ($(uname -m)). Apple Silicon only." >&2
  exit 1
fi
if [[ ! -f "$BIN" && "$DMG_FROM_APP" -eq 0 ]]; then
  echo "Missing $BIN: build the Metal binary first." >&2
  exit 1
fi
if [[ "$DMG_FROM_APP" -eq 1 && ! -d "$APP" ]]; then
  echo "Missing $APP: run package-macos.sh before --dmg-from-app." >&2
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
  if [[ -d "${ROOT}/native/sd/out" ]]; then
    mkdir -p "${APP}/Contents/MacOS/sd"
    cp -a "${ROOT}/native/sd/out/." "${APP}/Contents/MacOS/sd/"
  fi
  if [[ -d "${ROOT}/native/tts/out" ]]; then
    mkdir -p "${APP}/Contents/MacOS/tts"
    cp -a "${ROOT}/native/tts/out/." "${APP}/Contents/MacOS/tts/"
  fi

  sed \
    -e "/CFBundleShortVersionString/{n;s|<string>.*</string>|<string>${VERSION}</string>|;}" \
    "${ROOT}/installer/macos/Info.plist" > "${APP}/Contents/Info.plist"
fi
if [[ "$APP_ONLY" -eq 1 ]]; then
  tar -czf "${DIST}/scalattice-agent-aarch64-apple-darwin.tar.gz" -C "$DIST" scalattice-agent
  echo "==> Packed ${APP_NAME} (v${VERSION}) [app only]"
  echo "==> ${DIST}/scalattice-agent-aarch64-apple-darwin.tar.gz"
  exit 0
fi

STAGE="$(mktemp -d /tmp/scalattice-dmg.XXXXXX)"
RW_DMG=""
cleanup_packaging() {
  if [[ -n "${RW_DMG:-}" && -f "${RW_DMG:-}" ]]; then
    # Detach if a prior attempt left the RW image mounted.
    while IFS= read -r dev; do
      [[ -n "$dev" ]] || continue
      hdiutil detach -force "$dev" >/dev/null 2>&1 || true
    done < <(hdiutil info 2>/dev/null | awk -v img="$RW_DMG" '
      /^\/dev\// { dev=$1 }
      $0 ~ img { if (dev != "") print dev; dev="" }
    ' | sort -u)
  fi
  rm -rf "$STAGE"
  rm -f "${RW_DMG:-}"
}
trap cleanup_packaging EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
# Drop Finder/Spotlight metadata that sometimes keeps -srcfolder busy on GHA.
find "$STAGE" -name '.DS_Store' -delete 2>/dev/null || true
xattr -cr "$STAGE" 2>/dev/null || true
sync

detach_scalattice_images() {
  while IFS= read -r dev; do
    [[ -n "$dev" ]] || continue
    hdiutil detach -force "$dev" >/dev/null 2>&1 || true
  done < <(hdiutil info 2>/dev/null | awk '
    /^\/dev\// { dev=$1 }
    /[Ss]calattice/ { if (dev != "") print dev; dev="" }
  ' | sort -u)
  # diskimages-helper wedges on GHA after codesign; bounce it between retries.
  killall -q diskimages-helper DiskImagesUIAgent 2>/dev/null || true
}

# GHA macOS runners flake on `hdiutil create -srcfolder -format UDZO`
# ("Resource busy"), especially right after codesign. Build a RW image, copy
# with ditto, detach, then convert to compressed UDZO.
hdiutil_create() {
  local dest="$1" attempt=1 max=8
  local size_mb mount_dir attach_out dev
  size_mb="$(du -sm "$STAGE" | awk '{print int($1 * 2) + 32}')"
  if (( size_mb < 64 )); then
    size_mb=64
  fi

  while true; do
    detach_scalattice_images
    # Destination itself may be the busy resource from the unsigned pre-sign dmg.
    if [[ -e "$dest" ]]; then
      rm -f "$dest" 2>/dev/null || true
      if [[ -e "$dest" ]]; then
        mv -f "$dest" "${dest}.old.$$" 2>/dev/null || true
        rm -f "${dest}.old.$$" 2>/dev/null || true
      fi
    fi
    RW_DMG="${dest}.rw.$$.dmg"
    rm -f "$RW_DMG"

    if hdiutil create \
      -fs HFS+ \
      -volname "Scalattice Agent" \
      -size "${size_mb}m" \
      -ov \
      "$RW_DMG"; then
      attach_out="$(hdiutil attach -readwrite -noverify -noautoopen "$RW_DMG" 2>&1)" || attach_out=""
      dev="$(printf '%s\n' "$attach_out" | awk '/^\/dev\// { print $1; exit }')"
      mount_dir="$(printf '%s\n' "$attach_out" | awk -F'\t' '/\/Volumes\// { print $NF; exit }')"
      if [[ -z "$mount_dir" ]]; then
        mount_dir="$(printf '%s\n' "$attach_out" | awk '/\/Volumes\// { print $NF; exit }')"
      fi
      if [[ -n "$mount_dir" && -d "$mount_dir" ]]; then
        # Clear any automounted empty volume contents, then copy the stage.
        find "$mount_dir" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true
        if ditto "$STAGE" "$mount_dir" && sync; then
          if [[ -n "$dev" ]]; then
            hdiutil detach "$dev" -force >/dev/null 2>&1 || hdiutil detach "$mount_dir" -force >/dev/null 2>&1 || true
          else
            hdiutil detach "$mount_dir" -force >/dev/null 2>&1 || true
          fi
          # Give diskimages-helper a moment before convert.
          sleep 2
          if hdiutil convert "$RW_DMG" -format UDZO -imagekey zlib-level=9 -ov -o "$dest"; then
            rm -f "$RW_DMG"
            RW_DMG=""
            return 0
          fi
        fi
        if [[ -n "$dev" ]]; then
          hdiutil detach -force "$dev" >/dev/null 2>&1 || true
        fi
        hdiutil detach -force "$mount_dir" >/dev/null 2>&1 || true
      fi
    fi

    rm -f "$RW_DMG"
    RW_DMG=""
    if (( attempt >= max )); then
      echo "hdiutil create failed after ${max} attempts" >&2
      return 1
    fi
    echo "==> hdiutil create busy/failed (attempt ${attempt}/${max}); retrying…" >&2
    sleep $((attempt * 3))
    attempt=$((attempt + 1))
  done
}

hdiutil_create "$DMG"

if [[ "$DMG_FROM_APP" -eq 0 ]]; then
  # Unsigned / local tarball only. Release CI re-signs this Mach-O as a
  # standalone CLI after the .app is signed; copying the nested-signed
  # inner binary here breaks Gatekeeper (`invalid Info.plist`).
  tar -czf "${DIST}/scalattice-agent-aarch64-apple-darwin.tar.gz" -C "$DIST" scalattice-agent
  echo "==> ${DIST}/scalattice-agent-aarch64-apple-darwin.tar.gz"
fi

echo "==> Packed ${APP_NAME} (v${VERSION})"
echo "==> ${DMG}"
