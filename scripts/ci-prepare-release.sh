#!/usr/bin/env bash
# Create (or reuse) a GitHub release tag for CI. Used by .github/workflows/release.yml.
#
# Env:
#   RELEASE_TAG          Optional explicit tag (v1.2.3)
#   GITHUB_OUTPUT        Set by Actions; writes tag=
#   GH_REPO              owner/name (defaults to current gh repo)
set -euo pipefail

cd "$(dirname "$0")/.."

cargo_version() {
  grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'
}

set_cargo_version() {
  local ver="$1"
  sed -i.bak "s/^version = \".*\"/version = \"${ver}\"/" Cargo.toml
  rm -f Cargo.toml.bak
}

bump_patch() {
  local ver="$1" major minor patch
  IFS=. read -r major minor patch <<< "$ver"
  echo "${major}.${minor}.$((patch + 1))"
}

normalize_tag() {
  local raw="$1"
  raw="${raw#v}"
  echo "v${raw}"
}

REPO="${GH_REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
EXPLICIT="${RELEASE_TAG:-}"
VERSION=""
TAG=""

if [[ -n "$EXPLICIT" ]]; then
  TAG="$(normalize_tag "$EXPLICIT")"
  VERSION="${TAG#v}"
  set_cargo_version "$VERSION"
else
  VERSION="$(cargo_version)"
  TAG="v${VERSION}"
  if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    VERSION="$(bump_patch "$VERSION")"
    TAG="v${VERSION}"
    set_cargo_version "$VERSION"
  fi
fi

if [[ -n "$(git status --porcelain Cargo.toml Cargo.lock 2>/dev/null)" ]]; then
  git add Cargo.toml
  [[ -f Cargo.lock ]] && git add Cargo.lock
  git commit -m "chore(release): ${TAG}"
  git push origin HEAD:production
fi

if ! git rev-parse "$TAG" >/dev/null 2>&1; then
  git tag -a "$TAG" -m "$TAG"
  git push origin "$TAG"
fi

if ! gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
  gh release create "$TAG" \
    --repo "$REPO" \
    --title "$TAG" \
    --notes "Automated release from production (${GITHUB_SHA:-unknown})."
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "tag=${TAG}" >>"$GITHUB_OUTPUT"
  echo "version=${VERSION}" >>"$GITHUB_OUTPUT"
fi

echo "==> Release ${TAG} ready"
