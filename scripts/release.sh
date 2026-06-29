#!/usr/bin/env bash
# One-command release: x86_64 locally + aarch64 via GitHub Actions → one GitHub Release.
#
# Usage:
#   ./scripts/release.sh                 # bump patch, build both archs, publish
#   ./scripts/release.sh --dev           # x86_64 Linux local + Windows (local dist/ if present, else CI)
#   ./scripts/release.sh --dev --windows-ci   # force slow GitHub Actions Windows build
#   ./scripts/release.sh --skip-build    # reuse dist/ tarballs; still runs CI unless Windows is in dist/
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
DEV_RELEASE="false"
FORCE_WINDOWS_CI="false"
WAIT_CI="false"
NO_PUSH="false"
WORKFLOW_FILE=".github/workflows/release.yml"
WIN_INSTALLER="dist/ScalatticeAgentSetup-x86_64.exe"
WIN_ARCHIVE="dist/scalattice-agent-x86_64-pc-windows-msvc.zip"

usage() {
  cat <<'EOF'
Usage: ./scripts/release.sh [options]

  Default: bump version, build x86_64 here, build aarch64 in GitHub Actions,
  upload both tarballs to one GitHub Release.

  --dev: x86_64 Linux local; Windows from dist/ (build on a Windows machine) or GitHub Actions.

  Build Windows locally (on a Windows PC), then copy dist/ artifacts to this machine:
    .\\scripts\\build-release.ps1
    # copies: dist/ScalatticeAgentSetup-x86_64.exe + dist/scalattice-agent-x86_64-pc-windows-msvc.zip

Options:
  --dev             Day-to-day: x86_64 Linux local + Windows (local dist/ preferred; CI fallback)
  --windows-ci      Force Windows build on GitHub Actions (slow; ~1h cold cache)
  --wait-ci         Block until CI finishes (default for full release; dev CI is fire-and-forget)
  --skip-build      Skip local x86_64 compile (use existing dist/ tarball)
  --skip-aarch64    Same as --dev
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
    --dev) DEV_RELEASE="true"; shift ;;
    --skip-build) SKIP_BUILD="true"; shift ;;
    --skip-aarch64) DEV_RELEASE="true"; shift ;;
    --windows-ci) FORCE_WINDOWS_CI="true"; shift ;;
    --wait-ci) WAIT_CI="true"; shift ;;
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
  local wait="${3:-true}"
  echo ""
  echo "==> Building ${targets} in GitHub Actions (tag ${tag})"
  if [[ "$wait" == "true" ]]; then
    echo "    Waiting for CI (~30–90 min on cold cache)."
  else
    echo "    Not waiting — check progress: gh run list --workflow=${WORKFLOW_FILE}"
  fi

  gh workflow run "$WORKFLOW_FILE" \
    --ref main \
    -f "tag=${tag}" \
    -f "targets=${targets}"

  if [[ "$wait" != "true" ]]; then
    echo "==> CI started (not waiting). Upload assets when the run finishes:"
    echo "    gh run watch   # pick run id from: gh run list --workflow=${WORKFLOW_FILE}"
    return 0
  fi

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

windows_dist_ready() {
  [[ -f "$WIN_INSTALLER" || -f "$WIN_ARCHIVE" ]]
}

collect_windows_release_files() {
  RELEASE_WINDOWS_FILES=()
  if [[ -f "$WIN_INSTALLER" ]]; then
    RELEASE_WINDOWS_FILES+=("$WIN_INSTALLER")
  fi
  if [[ -f "$WIN_ARCHIVE" ]]; then
    RELEASE_WINDOWS_FILES+=("$WIN_ARCHIVE")
  fi
}

should_build_windows_in_ci() {
  if [[ "$FORCE_WINDOWS_CI" == "true" ]]; then
    return 0
  fi
  if windows_dist_ready; then
    return 1
  fi
  return 0
}

ci_wait_flag() {
  if [[ "$WAIT_CI" == "true" ]]; then
    echo "true"
  elif [[ "$DEV_RELEASE" == "true" ]]; then
    echo "false"
  else
    echo "true"
  fi
}

# Dev releases skip aarch64. Windows: local dist/ preferred; CI only when missing (or --windows-ci).
resolve_ci_targets() {
  if [[ "$DEV_RELEASE" == "true" ]]; then
    echo "windows-only"
  else
    echo "full"
  fi
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

if [[ "$DEV_RELEASE" == "true" ]]; then
  echo "==> Release ${TAG} (x86_64 Linux local; aarch64 skipped)"
else
  echo "==> Release ${TAG} (x86_64 local + aarch64 + Windows)"
fi

CI_TARGETS="$(resolve_ci_targets)"
if windows_dist_ready; then
  echo "==> Windows: using local dist/ (skip GitHub Actions unless --windows-ci)"
  collect_windows_release_files
  printf '    %s\n' "${RELEASE_WINDOWS_FILES[@]}"
elif [[ "$DEV_RELEASE" == "true" ]]; then
  echo "==> Windows: no dist/ artifacts — will use GitHub Actions (build on Windows with build-release.ps1 to avoid this)"
else
  echo "==> Windows: will build in GitHub Actions (targets=${CI_TARGETS})"
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

echo "==> Creating GitHub release"
RELEASE_FILES=("$X86_ARCHIVE")
RELEASE_WINDOWS_FILES=()
if windows_dist_ready; then
  collect_windows_release_files
  RELEASE_FILES+=("${RELEASE_WINDOWS_FILES[@]}")
fi
if [[ "$DEV_RELEASE" == "true" ]]; then
  if windows_dist_ready && [[ "$FORCE_WINDOWS_CI" != "true" ]]; then
    RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux + Windows local dist/; dev release)."
  else
    RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux local; Windows via GitHub Actions; dev release)."
  fi
else
  RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux local; aarch64 + Windows via GitHub Actions)."
fi
gh release create "$TAG" "${RELEASE_FILES[@]}" \
  --target main \
  --title "$TAG" \
  --notes "$RELEASE_NOTES"

git fetch --tags "$REMOTE" 2>/dev/null || true

NEED_WINDOWS_VERIFY="true"
CI_WAIT="$(ci_wait_flag)"

if [[ "$DEV_RELEASE" == "true" ]]; then
  if should_build_windows_in_ci; then
    build_ci_assets "$TAG" "windows-only" "$CI_WAIT"
  else
    echo "==> Skipping Windows CI (local dist/ uploaded)"
    NEED_WINDOWS_VERIFY="true"
  fi
else
  if should_build_windows_in_ci; then
    build_ci_assets "$TAG" "full" "$CI_WAIT"
  else
    echo "==> Skipping Windows CI (local dist/ uploaded); aarch64 still in CI"
    build_ci_assets "$TAG" "aarch64-only" "$CI_WAIT"
  fi
fi

if [[ "$CI_WAIT" == "true" ]]; then
  if [[ "$DEV_RELEASE" == "true" ]]; then
    verify_release_assets "$TAG" false "$NEED_WINDOWS_VERIFY"
  else
    verify_release_assets "$TAG" true "$NEED_WINDOWS_VERIFY"
  fi
elif windows_dist_ready && ! should_build_windows_in_ci; then
  verify_release_assets "$TAG" false true
else
  echo ""
  echo "==> CI still running (or not started for Windows). Verify when done:"
  echo "    gh release view ${TAG} --json assets -q '.assets[].name'"
fi

REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
echo ""
echo "Done. ${TAG} published with:"
gh release view "$TAG" --json assets -q '.assets[].name' | sed 's/^/  - /'
echo ""
echo "  https://github.com/${REPO}/releases/tag/${TAG}"
