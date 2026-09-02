#!/usr/bin/env bash
# Create (or reuse) a GitHub release tag for CI. Used by .github/workflows/release.yml.
#
# On a production merge, pick the next free patch from the highest of Cargo.toml,
# origin tags, and GitHub releases, then skip any tag/release that already exists.
# A stale development Cargo.toml cannot ship behind production.
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

semver_sort_highest() {
  grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | sort -t. -k1,1n -k2,2n -k3,3n | tail -1
}

latest_github_version() {
  gh release list --repo "$REPO" --limit 100 --json tagName -q '.[].tagName' 2>/dev/null \
    | sed 's/^v//' \
    | semver_sort_highest || true
}

# Origin tags are the source of truth (a release can exist without Cargo.toml
# on development matching it). Ignore local tags: they drift and clobber fetches.
latest_origin_tag_version() {
  git ls-remote --tags origin 'refs/tags/v*' 2>/dev/null \
    | awk '{print $2}' \
    | sed 's|refs/tags/||; s/\^{}$//' \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    | sed 's/^v//' \
    | semver_sort_highest || true
}

max_version() {
  local best="" v
  for v in "$@"; do
    [[ -n "$v" ]] || continue
    if [[ -z "$best" ]] || version_ge "$v" "$best"; then
      best="$v"
    fi
  done
  echo "$best"
}

origin_has_tag() {
  git ls-remote --tags origin "refs/tags/v$1" 2>/dev/null | grep -q .
}

next_free_version() {
  local cargo latest next
  cargo="$(cargo_version)"
  latest="$(max_version "$cargo" "$(latest_origin_tag_version)" "$(latest_github_version)")"
  if [[ -z "$latest" ]]; then
    echo "1.0.0"
    return
  fi
  next="$(bump_patch "$latest")"
  while origin_has_tag "$next" || gh release view "v${next}" -R "$REPO" >/dev/null 2>&1; do
    next="$(bump_patch "$next")"
  done
  echo "$next"
}

REPO="${GH_REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
EXPLICIT="${RELEASE_TAG:-}"
VERSION=""
TAG=""

# Annotated vs lightweight retags on GitHub make `git pull --tags` fail locally
# ("would clobber existing tag"). CI must follow origin, not a stale checkout.
git fetch origin --tags --force >/dev/null 2>&1 || true

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

if origin_has_tag "${TAG#v}"; then
  echo "==> Tag ${TAG} already on origin"
else
  git tag -d "$TAG" 2>/dev/null || true
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
