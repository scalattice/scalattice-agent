#!/usr/bin/env bash
# One-command release: x86_64 Linux local + Windows on self-hosted runner + optional aarch64 CI.
#
# Usage:
#   ./scripts/release.sh --dev      # x86_64 Linux here + Windows on self-hosted runner
#   ./scripts/release.sh            # above + aarch64 on GitHub ARM runners
#
# First-time Windows setup (once, on a Windows PC, Admin PowerShell):
#   .\scripts\setup-windows-build.ps1
#   .\scripts\install-windows-runner.ps1
#
# Requires: rust, CUDA 12.6 dev, gh auth login. Full script map: scripts/README.md
set -euo pipefail

cd "$(dirname "$0")/.."

REMOTE="origin"
BUMP="patch"
EXPLICIT_VERSION=""
SKIP_BUILD="false"
DEV_RELEASE="false"
LOCAL_WINDOWS="false"
GITHUB_HOSTED_WINDOWS="false"
NO_PUSH="false"
WORKFLOW_FILE=".github/workflows/release.yml"
WIN_INSTALLER="dist/ScalatticeAgentSetup-x86_64.exe"
WIN_ARCHIVE="dist/scalattice-agent-x86_64-pc-windows-msvc.zip"

usage() {
  cat <<'EOF'
Usage: ./scripts/release.sh [options]

  ./scripts/release.sh --dev
    1. Build x86_64 Linux locally
    2. Push + create GitHub Release
    3. Build Windows on your self-hosted runner (fast, warm cache)
    4. Upload .exe + zip to the release

  One-time Windows machine setup (Admin PowerShell):
    .\scripts\setup-windows-build.ps1
    .\scripts\install-windows-runner.ps1

Options:
  --dev                   Day-to-day: Linux + Windows (skip aarch64)
  --skip-build            Reuse existing dist/ Linux tarball
  --skip-aarch64          Same as --dev
  --local-windows         Use dist/*.exe from disk; skip Windows CI
  --github-hosted-windows Slow fallback (~1h); use GitHub windows-2022 runner
  --version X.Y.Z         Explicit version
  --minor                 Bump minor instead of patch
  --no-push               Dry run (no push/release)
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
    --local-windows) LOCAL_WINDOWS="true"; shift ;;
    --github-hosted-windows) GITHUB_HOSTED_WINDOWS="true"; shift ;;
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

windows_dist_ready() {
  [[ -f "$WIN_INSTALLER" || -f "$WIN_ARCHIVE" ]]
}

collect_windows_release_files() {
  RELEASE_WINDOWS_FILES=()
  [[ -f "$WIN_INSTALLER" ]] && RELEASE_WINDOWS_FILES+=("$WIN_INSTALLER")
  [[ -f "$WIN_ARCHIVE" ]] && RELEASE_WINDOWS_FILES+=("$WIN_ARCHIVE")
}

resolve_ci_targets() {
  if [[ "$DEV_RELEASE" == "true" ]]; then
    echo "windows-only"
  else
    echo "full"
  fi
}

windows_runner_input() {
  if [[ "$GITHUB_HOSTED_WINDOWS" == "true" ]]; then
    echo "github-hosted"
  else
    echo "self-hosted"
  fi
}

should_run_windows_ci() {
  [[ "$LOCAL_WINDOWS" != "true" ]]
}

build_ci_assets() {
  local tag="$1"
  local targets="$2"
  local win_runner="$3"
  echo ""
  echo "==> CI: ${targets} (Windows runner: ${win_runner})"
  if [[ "$win_runner" == "self-hosted" ]]; then
    echo "    Waiting for self-hosted Windows runner (usually minutes with warm cache)."
  else
    echo "    Waiting for GitHub-hosted Windows runner (~1h cold cache)."
  fi

  gh workflow run "$WORKFLOW_FILE" \
    --ref main \
    -f "tag=${tag}" \
    -f "targets=${targets}" \
    -f "windows_runner=${win_runner}"

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

verify_release_assets() {
  local tag="$1"
  local need_aarch64="$2"
  local need_windows="$3"
  local need_macos="${4:-false}"
  local assets x86 aarch darwin win

  assets="$(gh release view "$tag" --json assets -q '.assets[].name')"
  x86="$(echo "$assets" | grep -c 'x86_64-unknown-linux-gnu' || true)"
  aarch="$(echo "$assets" | grep -c 'aarch64-unknown-linux-gnu' || true)"
  darwin="$(echo "$assets" | grep -c 'aarch64-apple-darwin' || true)"
  win="$(echo "$assets" | grep -cE 'pc-windows-msvc|ScalatticeAgentSetup' || true)"

  if [[ "$x86" -lt 1 ]]; then
    echo "Release ${tag} is missing x86_64 Linux tarball." >&2
    exit 1
  fi
  if [[ "$need_aarch64" == "true" && "$aarch" -lt 1 ]]; then
    echo "Release ${tag} is missing aarch64 Linux tarball." >&2
    exit 1
  fi
  if [[ "$need_macos" == "true" && "$darwin" -lt 1 ]]; then
    echo "Release ${tag} is missing Apple Silicon (aarch64-apple-darwin) tarball." >&2
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
set_cargo_version "$VERSION"
X86_ARCHIVE="dist/scalattice-agent-x86_64-unknown-linux-gnu.tar.gz"
CI_TARGETS="$(resolve_ci_targets)"
WIN_RUNNER="$(windows_runner_input)"

if [[ "$DEV_RELEASE" == "true" ]]; then
  echo "==> Release ${TAG} (Linux local + Windows CI; aarch64 Linux + macOS skipped)"
else
  echo "==> Release ${TAG} (Linux local + Windows + aarch64 Linux + macOS CI)"
fi

if [[ "$LOCAL_WINDOWS" == "true" ]]; then
  windows_dist_ready || {
    echo "Missing Windows dist/ artifacts. Run on Windows: .\\scripts\\build-release.ps1" >&2
    exit 1
  }
  echo "==> Windows: uploading local dist/ (skipping CI)"
elif [[ "$GITHUB_HOSTED_WINDOWS" == "true" ]]; then
  echo "==> Windows: GitHub-hosted runner (slow)"
else
  echo "==> Windows: self-hosted runner"
  # shellcheck source=scripts/check-windows-runner.sh
  source "$(dirname "$0")/check-windows-runner.sh"
  require_windows_runner
fi

if [[ "$SKIP_BUILD" == "true" ]]; then
  [[ -f "$X86_ARCHIVE" ]] || {
    echo "Missing ${X86_ARCHIVE}; drop --skip-build to compile." >&2
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

RELEASE_FILES=("$X86_ARCHIVE")
if [[ "$LOCAL_WINDOWS" == "true" ]]; then
  collect_windows_release_files
  RELEASE_FILES+=("${RELEASE_WINDOWS_FILES[@]}")
  RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux + Windows local dist/)."
elif [[ "$DEV_RELEASE" == "true" ]]; then
  RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux local; Windows via ${WIN_RUNNER} runner)."
else
  RELEASE_NOTES="Built via scripts/release.sh (x86_64 Linux local; aarch64 Linux + macOS + Windows via CI)."
fi

echo "==> Creating GitHub release"
gh release create "$TAG" "${RELEASE_FILES[@]}" \
  --target main \
  --title "$TAG" \
  --notes "$RELEASE_NOTES"

git fetch --tags "$REMOTE" 2>/dev/null || true

if should_run_windows_ci; then
  if [[ "$DEV_RELEASE" == "true" ]]; then
    build_ci_assets "$TAG" "windows-only" "$WIN_RUNNER"
  else
    build_ci_assets "$TAG" "full" "$WIN_RUNNER"
  fi
fi

if [[ "$DEV_RELEASE" == "true" ]]; then
  verify_release_assets "$TAG" false true false
else
  verify_release_assets "$TAG" true true true
fi

REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
echo ""
echo "Done. ${TAG} published with:"
gh release view "$TAG" --json assets -q '.assets[].name' | sed 's/^/  - /'
echo ""
echo "  https://github.com/${REPO}/releases/tag/${TAG}"
