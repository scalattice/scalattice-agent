#!/usr/bin/env bash
# One-command release: x86_64 locally + aarch64 via GitHub Actions → one GitHub Release.
#
# Usage:
#   ./scripts/release.sh                 # bump patch, build both archs, publish
#   ./scripts/release.sh --dev           # x86_64 Linux local + Windows CI (skip aarch64)
#   ./scripts/release.sh --skip-build    # reuse dist/ x86 tarball; still builds aarch64 in CI
#   ./scripts/release.sh --skip-aarch64  # same as --dev (x86_64 only)
#   ./scripts/release.sh --version 1.0.2
#   ./scripts/release.sh --minor
#
# Requires: rust, CUDA 12.6 dev; gh auth login for full release (see scripts/README.md).
set -euo pipefail

cd "$(dirname "$0")/.."

REMOTE="origin"
BUMP="patch"
EXPLICIT_VERSION=""
SKIP_BUILD="false"
SKIP_AARCH64="false"
NO_PUSH="false"
WORKFLOW_FILE=".github/workflows/release.yml"

usage() {
  cat <<'EOF'
Usage: ./scripts/release.sh [options]

  Default: bump version, build x86_64 here, build aarch64 in GitHub Actions,
  upload both tarballs to one GitHub Release.

  --dev: x86_64 Linux local + Windows CI (skips aarch64 only).

Options:
  --dev             Day-to-day release: x86_64 Linux + Windows (no aarch64 CI)
  --skip-build      Skip local x86_64 compile (use existing dist/ tarball)
  --skip-aarch64    Same as --dev (Windows CI still runs)
  --version X.Y.Z   Explicit version
  --minor           Bump minor instead of patch
  --no-push         Dry run (no push/release)
  -h, --help
EOF
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --dev) SKIP_AARCH64="true"; shift ;;
    --skip-build) SKIP_BUILD="true"; shift ;;
    --skip-aarch64) SKIP_AARCH64="true"; shift ;;
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
    *)
      echo "Unknown option: $1" >&2
      usage 1
      ;;
  esac
done

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing required command: $1" >&2
    exit 1
  }
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
  local ver="$1" kind="$2" major minor patch
  IFS=. read -r major minor patch <<< "$ver"
  case "$kind" in
    minor) echo "${major}.$((minor + 1)).0" ;;
    patch) echo "${major}.${minor}.$((patch + 1))" ;;
    *) echo "$ver" ;;
  esac
}

latest_tag_version() {
  git tag -l 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null | sed 's/^v//' | sort -V | tail -1
}

release_published() {
  gh release view "$1" >/dev/null 2>&1
}

resolve_version() {
  if [[ -n "$EXPLICIT_VERSION" ]]; then
    echo "$EXPLICIT_VERSION"
    return
  fi
  local cargo_tag latest published_ver
  cargo_tag="$(cargo_version)"
  latest="$(latest_tag_version)"
  if [[ -n "$latest" ]] && version_gt "$latest" "$cargo_tag"; then
    published_ver="$latest"
  else
    published_ver="$cargo_tag"
  fi
  if release_published "v${cargo_tag}"; then
    bump_version "$published_ver" "$BUMP"
  else
    echo "$cargo_tag"
  fi
}

build_ci_assets() {
  local tag="$1"
  local targets="$2"
  echo ""
  echo "==> Building ${targets} in GitHub Actions (tag ${tag})"
  echo "    This is ~30–90 min on a cold cache; the script waits until it finishes."

  gh workflow run "$WORKFLOW_FILE" \
    --ref main \
    -f "tag=${tag}" \
    -f "targets=${targets}"

  local run_id=""
  for _ in $(seq 1 30); do
    sleep 2
    run_id="$(gh run list --workflow="$WORKFLOW_FILE" --limit 5 --json databaseId,status -q \
      '.[] | select(.status=="queued" or .status=="in_progress") | .databaseId' | head -1)"
    [[ -n "$run_id" ]] && break
  done

  if [[ -z "$run_id" ]]; then
    echo "Could not find started workflow run." >&2
    exit 1
  fi

  echo "==> Watching run ${run_id}"
  gh run watch "$run_id" --exit-status
}

build_aarch64_in_ci() {
  build_ci_assets "$1" "aarch64-only"
}

verify_release_assets() {
  local tag="$1"
  local need_aarch64="$2"
  local need_windows="$3"
  local assets x86 aarch win

  assets="$(gh release view "$tag" --json assets -q '.assets[].name')"
  x86="$(echo "$assets" | grep -c 'x86_64-unknown-linux-gnu' || true)"
  aarch="$(echo "$assets" | grep -c 'aarch64' || true)"
  win="$(echo "$assets" | grep -cE 'pc-windows-msvc|ScalatticeAgentSetup' || true)"

  if [[ "$x86" -lt 1 ]]; then
    echo "Release ${tag} is missing x86_64 Linux tarball." >&2
    exit 1
  fi
  if [[ "$need_aarch64" == "true" && "$aarch" -lt 1 ]]; then
    echo "Release ${tag} is missing aarch64 tarball." >&2
    exit 1
  fi
  if [[ "$need_windows" == "true" && "$win" -lt 1 ]]; then
    echo "Release ${tag} is missing Windows installer (.exe or zip)." >&2
    exit 1
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
X86_ARCHIVE="dist/scalattice-agent-x86_64-unknown-linux-gnu.tar.gz"

if [[ "$(cargo_version)" != "$VERSION" ]]; then
  echo "==> Bumping Cargo.toml: $(cargo_version) → ${VERSION}"
  set_cargo_version "$VERSION"
fi

if [[ "$SKIP_AARCH64" == "true" ]]; then
  echo "==> Release ${TAG} (x86_64 Linux local + Windows CI, skipping aarch64)"
else
  echo "==> Release ${TAG} (x86_64 local + aarch64 + Windows CI)"
fi

if [[ "$SKIP_BUILD" == "true" ]]; then
  [[ -f "$X86_ARCHIVE" ]] || {
    echo "Missing ${X86_ARCHIVE} — drop --skip-build to compile." >&2
    exit 1
  }
  echo "==> Using existing ${X86_ARCHIVE}"
else
  ./scripts/build-release.sh x86_64-unknown-linux-gnu
fi

if [[ -n "$(git status --porcelain Cargo.toml Cargo.lock 2>/dev/null)" ]]; then
  echo "==> Committing version files"
  git add Cargo.toml
  [[ -f Cargo.lock ]] && git add Cargo.lock
  git commit -m "Release ${TAG}"
fi

if [[ "$NO_PUSH" == "true" ]]; then
  echo "==> --no-push: stopping before publish"
  exit 0
fi

echo "==> Pushing main"
git push "$REMOTE" main

if git rev-parse "$TAG" >/dev/null 2>&1 || gh release view "$TAG" >/dev/null 2>&1; then
  echo "==> Replacing existing ${TAG}"
  git tag -d "$TAG" 2>/dev/null || true
  git push "$REMOTE" ":refs/tags/${TAG}" 2>/dev/null || true
  gh release delete "$TAG" --yes 2>/dev/null || true
fi

echo "==> Creating GitHub release with x86_64 tarball"
if [[ "$SKIP_AARCH64" == "true" ]]; then
  RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux local + Windows CI, dev release, no aarch64)."
else
  RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux local, aarch64 + Windows via GitHub Actions)."
fi
gh release create "$TAG" "$X86_ARCHIVE" \
  --target main \
  --title "$TAG" \
  --notes "$RELEASE_NOTES"

git fetch --tags "$REMOTE" 2>/dev/null || true

if [[ "$SKIP_AARCH64" == "true" ]]; then
  build_ci_assets "$TAG" "windows-only"
else
  build_ci_assets "$TAG" "full"
fi

if [[ "$SKIP_AARCH64" == "true" ]]; then
  verify_release_assets "$TAG" false true
else
  verify_release_assets "$TAG" true true
fi

REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
echo ""
echo "Done. ${TAG} published with:"
gh release view "$TAG" --json assets -q '.assets[].name' | sed 's/^/  - /'
echo ""
echo "  https://github.com/${REPO}/releases/tag/${TAG}"
