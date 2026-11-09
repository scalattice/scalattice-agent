#!/usr/bin/env bash
# Create (or reuse) a GitHub release tag for CI. Used by .github/workflows/release.yml.
#
# On a production merge, pick the next patch after the highest semver tag that
# still exists on origin. Cargo.toml is NOT a floor — a failed release that
# bumped Cargo.toml and then deleted its tag must reuse that number, not skip
# ahead. An orphan GitHub release without an origin tag is reused (assets can
# be re-uploaded), not skipped.
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

normalize_tag() {
  local raw="$1"
  raw="${raw#v}"
  echo "v${raw}"
}

semver_sort_highest() {
  grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | sort -t. -k1,1n -k2,2n -k3,3n | tail -1
}

# Origin tags are the only chronological source of truth. Ignore local tags
# and Cargo.toml (both drift after a deleted failed release).
latest_origin_tag_version() {
  git ls-remote --tags origin 'refs/tags/v*' 2>/dev/null \
    | awk '{print $2}' \
    | sed 's|refs/tags/||; s/\^{}$//' \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    | sed 's/^v//' \
    | semver_sort_highest || true
}

# Cold-start fallback when origin has no v* tags yet.
latest_github_version() {
  gh release list --repo "$REPO" --limit 100 --json tagName -q '.[].tagName' 2>/dev/null \
    | sed 's/^v//' \
    | semver_sort_highest || true
}

origin_has_tag() {
  git ls-remote --tags origin "refs/tags/v$1" 2>/dev/null | grep -q .
}

next_free_version() {
  local tip next
  tip="$(latest_origin_tag_version)"
  if [[ -z "$tip" ]]; then
    tip="$(latest_github_version)"
  fi
  if [[ -z "$tip" ]]; then
    echo "1.0.0"
    return
  fi
  next="$(bump_patch "$tip")"
  # Only origin tags block a number. A leftover GitHub release from a deleted
  # tag is reused so a failed build can retry the same version.
  while origin_has_tag "$next"; do
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
