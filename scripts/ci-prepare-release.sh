#!/usr/bin/env bash
# Create (or reuse) a GitHub release tag for CI. Used by .github/workflows/release.yml.
#
# On a production merge, pick the next free patch from the latest GitHub release
# and Cargo.toml (whichever is higher), then skip any tags that already exist.
# That way a stale development Cargo.toml cannot ship behind production.
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
  if [[ -f Cargo.lock ]]; then
    perl -0777 -i -pe "s/(name = \"scalattice-agent\"\\n)version = \"[^\"]+\"/\${1}version = \"${ver}\"/" Cargo.lock
  fi
}

bump_patch() {
  local ver="$1" major minor patch
  IFS=. read -r major minor patch <<< "$ver"
  echo "${major}.${minor}.$((patch + 1))"
}

version_ge() {
  local a="$1" b="$2"
  IFS=. read -r a1 a2 a3 <<< "$a"
  IFS=. read -r b1 b2 b3 <<< "$b"
  a1=${a1:-0}; a2=${a2:-0}; a3=${a3:-0}
  b1=${b1:-0}; b2=${b2:-0}; b3=${b3:-0}
  if (( a1 != b1 )); then
    (( a1 > b1 ))
    return
  fi
  if (( a2 != b2 )); then
    (( a2 > b2 ))
    return
  fi
  (( a3 >= b3 ))
}

normalize_tag() {
  local raw="$1"
  raw="${raw#v}"
  echo "v${raw}"
}

latest_github_version() {
  local tag
  tag="$(gh release list --repo "$REPO" --limit 20 --json tagName -q '.[].tagName' 2>/dev/null \
    | grep -E '^v?[0-9]+\.[0-9]+\.[0-9]+$' \
    | sed 's/^v//' \
    | sort -t. -k1,1n -k2,2n -k3,3n \
    | tail -1 || true)"
  echo "$tag"
}

next_free_version() {
  local cargo latest next
  cargo="$(cargo_version)"
  latest="$(latest_github_version)"
  if [[ -n "$latest" ]]; then
    next="$(bump_patch "$latest")"
    if version_ge "$cargo" "$next"; then
      next="$cargo"
    fi
  else
    next="$cargo"
  fi
  while gh release view "v${next}" -R "$REPO" >/dev/null 2>&1; do
    next="$(bump_patch "$next")"
  done
  echo "$next"
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
  VERSION="$(next_free_version)"
  TAG="v${VERSION}"
  if [[ "$(cargo_version)" != "$VERSION" ]]; then
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
