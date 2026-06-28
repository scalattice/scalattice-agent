#!/usr/bin/env bash
# One-command release: bump version, build, commit, push, publish to GitHub.
#
# Usage:
#   ./scripts/release.sh                 # publish current or next patch version
#   ./scripts/release.sh --skip-build    # upload existing dist/ tarball (same version)
#   ./scripts/release.sh --version 1.0.32
#   ./scripts/release.sh --minor         # bump minor instead of patch
#   ./scripts/release.sh --extra dist/scalattice-agent-aarch64-unknown-linux-gnu.tar.gz
#
# Requires: rust, CUDA 12.6 dev, gh auth login (see scripts/README.md).
set -euo pipefail

cd "$(dirname "$0")/.."

REMOTE="origin"
BUMP="patch"
EXPLICIT_VERSION=""
SKIP_BUILD="false"
NO_PUSH="false"
EXTRA_ASSETS=()

usage() {
  sed -n '2,12p' "$0" | tr -d '#'
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --skip-build) SKIP_BUILD="true"; shift ;;
    --no-push) NO_PUSH="true"; shift ;;
    --minor) BUMP="minor"; shift ;;
    --patch) BUMP="patch"; shift ;;
    --version)
      EXPLICIT_VERSION="${2:?--version requires X.Y.Z}"
      shift 2
      ;;
    --remote)
      REMOTE="${2:?--remote requires name}"
      shift 2
      ;;
    --extra)
      EXTRA_ASSETS+=("${2:?--extra requires a tarball path}")
      shift 2
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage 1
      ;;
  esac
done

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

cargo_version() {
  grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'
}

set_cargo_version() {
  local ver="$1"
  sed -i "s/^version = \".*\"/version = \"${ver}\"/" Cargo.toml
}

version_gt() {
  printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1 | grep -qx "$2"
}

bump_version() {
  local ver="$1"
  local kind="$2"
  local major minor patch
  IFS=. read -r major minor patch <<< "$ver"
  case "$kind" in
    minor) echo "${major}.$((minor + 1)).0" ;;
    patch) echo "${major}.${minor}.$((patch + 1))" ;;
    *) echo "$ver" ;;
  esac
}

latest_tag_version() {
  git tag -l 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null \
    | sed 's/^v//' \
    | sort -V \
    | tail -1
}

release_published() {
  local tag="$1"
  gh release view "$tag" >/dev/null 2>&1
}

resolve_version() {
  if [[ -n "$EXPLICIT_VERSION" ]]; then
    echo "$EXPLICIT_VERSION"
    return
  fi

  local cargo_tag latest published_ver next
  cargo_tag="$(cargo_version)"
  latest="$(latest_tag_version)"

  if [[ -n "$latest" ]] && version_gt "$latest" "$cargo_tag"; then
    published_ver="$latest"
  else
    published_ver="$cargo_tag"
  fi

  if release_published "v${cargo_tag}"; then
    next="$(bump_version "$published_ver" "$BUMP")"
    echo "$next"
  else
    echo "$cargo_tag"
  fi
}

require_cmd cargo
require_cmd gh
require_cmd git

if ! gh auth status >/dev/null 2>&1; then
  echo "Run: gh auth login" >&2
  exit 1
fi

dirty_other="$(git status --porcelain | grep -Ev '^.. (Cargo.toml|Cargo.lock)$' || true)"
if [[ -n "$dirty_other" ]]; then
  echo "Commit or stash unrelated changes before releasing:" >&2
  echo "$dirty_other" >&2
  exit 1
fi

VERSION="$(resolve_version)"
TAG="v${VERSION}"
ARCHIVE="dist/scalattice-agent-x86_64-unknown-linux-gnu.tar.gz"

if [[ "$(cargo_version)" != "$VERSION" ]]; then
  echo "==> Bumping Cargo.toml: $(cargo_version) → ${VERSION}"
  set_cargo_version "$VERSION"
fi

echo "==> Release ${TAG}"

if [[ "$SKIP_BUILD" == "true" ]]; then
  if [[ ! -f "$ARCHIVE" ]]; then
    echo "Missing ${ARCHIVE} — drop --skip-build to compile first." >&2
    exit 1
  fi
  echo "==> Skipping build (using existing ${ARCHIVE})"
else
  ./scripts/build-release.sh x86_64-unknown-linux-gnu
fi

ASSETS=("$ARCHIVE" "${EXTRA_ASSETS[@]}")
for asset in "${ASSETS[@]}"; do
  if [[ ! -f "$asset" ]]; then
    echo "Missing release asset: ${asset}" >&2
    exit 1
  fi
done

if [[ -n "$(git status --porcelain Cargo.toml Cargo.lock 2>/dev/null)" ]]; then
  echo "==> Committing version files"
  git add Cargo.toml
  [[ -f Cargo.lock ]] && git add Cargo.lock
  git commit -m "Release ${TAG}"
fi

if [[ "$NO_PUSH" == "true" ]]; then
  echo "==> --no-push: stopping before push/release"
  echo "    Would publish: ${TAG}"
  echo "    Assets: ${ASSETS[*]}"
  exit 0
fi

echo "==> Pushing main"
git push "$REMOTE" main

if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "==> Replacing existing tag ${TAG}"
  git tag -d "$TAG"
  git push "$REMOTE" ":refs/tags/${TAG}" 2>/dev/null || true
  gh release delete "$TAG" --yes 2>/dev/null || true
fi

echo "==> Creating GitHub release (${TAG} — tag push will not trigger CI compile)"
# Upload assets first, then create tag on GitHub. Do not git push tag before this.
gh release create "$TAG" "${ASSETS[@]}" \
  --target main \
  --title "$TAG" \
  --notes "[local] Published via scripts/release.sh"

git fetch --tags "$REMOTE"
git tag -d "$TAG" 2>/dev/null || true
git fetch --tags "$REMOTE" "$TAG"

REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
echo ""
echo "Done."
echo "  Release: https://github.com/${REPO}/releases/tag/${TAG}"
echo "  Asset:   https://github.com/${REPO}/releases/download/${TAG}/scalattice-agent-x86_64-unknown-linux-gnu.tar.gz"
