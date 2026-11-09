#!/usr/bin/env bash
# Create (or reuse) a GitHub release tag for CI. Used by .github/workflows/release.yml.
#
# Version rule: next patch after the highest *complete* release (a GitHub release
# that still has assets). Empty tags/releases left by failed builds are NOT a
# floor — deleting them or leaving them both yields the same next number.
#
# Example: v1.1.164 has assets; v1.1.165–167 are empty leftovers → next is 1.1.165.
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

# Origin tags are the chronological ceiling we walk down from.
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

# A release "counts" only when it has uploaded assets (builds finished).
release_is_complete() {
  local ver="$1" count
  count="$(
    gh release view "v${ver}" -R "$REPO" --json assets -q '.assets | length' 2>/dev/null || echo 0
  )"
  count="$(printf '%s' "$count" | tr -d '[:space:]')"
  [[ -n "$count" && "$count" =~ ^[0-9]+$ && "$count" -gt 0 ]]
}

# Next patch after the highest complete release. Incomplete tags above that
# (failed macOS/notarize runs) are ignored so retries reuse the same number.
next_free_version() {
  local tip cursor major minor patch n=0
  tip="$(latest_origin_tag_version)"
  if [[ -z "$tip" ]]; then
    tip="$(latest_github_version)"
  fi
  if [[ -z "$tip" ]]; then
    echo "1.0.0"
    return
  fi

  cursor="$tip"
  while (( n < 80 )); do
    if release_is_complete "$cursor"; then
      echo "$(bump_patch "$cursor")"
      return
    fi
    IFS=. read -r major minor patch <<< "$cursor"
    if [[ -z "${patch:-}" || "$patch" -le 0 ]]; then
      # Nothing complete below tip — reuse the incomplete tip itself.
      echo "$tip"
      return
    fi
    cursor="${major}.${minor}.$((patch - 1))"
    n=$((n + 1))
  done
  echo "$(bump_patch "$tip")"
}

# Drop empty leftover tags/releases above the version we are about to ship so
# the next tip stays honest if this run fails again mid-build.
prune_incomplete_above() {
  local keep="$1" tip cursor major minor patch
  tip="$(latest_origin_tag_version)"
  [[ -n "$tip" ]] || return 0
  cursor="$tip"
  while true; do
    IFS=. read -r major minor patch <<< "$cursor"
    # Stop once we reach the version we are shipping (or below).
    if [[ "$cursor" == "$keep" ]]; then
      break
    fi
    # Semver compare: stop if cursor < keep (walked too far).
    if [[ "$(printf '%s\n%s\n' "$cursor" "$keep" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" == "$cursor" \
      && "$cursor" != "$keep" ]]; then
      break
    fi
    if ! release_is_complete "$cursor"; then
      echo "==> Pruning incomplete leftover v${cursor}"
      git push origin ":refs/tags/v${cursor}" >/dev/null 2>&1 || true
      gh release delete "v${cursor}" -R "$REPO" --yes >/dev/null 2>&1 || true
    fi
    if [[ "$patch" -le 0 ]]; then
      break
    fi
    cursor="${major}.${minor}.$((patch - 1))"
  done
}

REPO="${GH_REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
EXPLICIT="${RELEASE_TAG:-}"
VERSION=""
TAG=""
REUSE=0

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
  if origin_has_tag "$VERSION" && ! release_is_complete "$VERSION"; then
    REUSE=1
    echo "==> Reusing incomplete ${TAG} (last complete release is the prior patch)"
  fi
  if [[ "$(cargo_version)" != "$VERSION" ]]; then
    set_cargo_version "$VERSION"
  fi
fi

echo "==> Selected ${TAG} (cargo was v$(cargo_version))"

if [[ -n "$(git status --porcelain Cargo.toml Cargo.lock 2>/dev/null)" ]]; then
  git add Cargo.toml
  [[ -f Cargo.lock ]] && git add Cargo.lock
  git commit -m "chore(release): ${TAG}"
  git push origin HEAD:production
fi

# Always point the tag at this prepare commit. Force-update when retrying an
# incomplete number so builds check out the new merge, not the failed attempt.
git tag -d "$TAG" 2>/dev/null || true
git tag -a "$TAG" -m "$TAG"
if [[ "$REUSE" -eq 1 ]] || origin_has_tag "${TAG#v}"; then
  git push origin "refs/tags/${TAG}" --force
else
  git push origin "$TAG"
fi

prune_incomplete_above "$VERSION"

if ! gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
  gh release create "$TAG" \
    --repo "$REPO" \
    --title "$TAG" \
    --notes "Automated release from production (${GITHUB_SHA:-unknown})."
else
  # Clear stale notes; assets are uploaded by platform jobs (--clobber).
  echo "==> GitHub release ${TAG} already exists (assets will be replaced)"
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "tag=${TAG}" >>"$GITHUB_OUTPUT"
  echo "version=${VERSION}" >>"$GITHUB_OUTPUT"
fi

echo "==> Release ${TAG} ready"
