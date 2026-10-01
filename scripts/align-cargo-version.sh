#!/usr/bin/env bash
# Align Cargo.toml / Cargo.lock to the highest published agent tag on origin
# (and GitHub releases). Do not invent versions by hand — release CI bumps the
# next free patch from this tip via scripts/ci-prepare-release.sh.
#
# Usage:
#   ./scripts/align-cargo-version.sh           # set to latest published tip
#   ./scripts/align-cargo-version.sh --check   # exit 1 if Cargo.toml is behind tip
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

semver_sort_highest() {
  grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | sort -t. -k1,1n -k2,2n -k3,3n | tail -1
}

latest_origin_tag_version() {
  git ls-remote --tags origin 'refs/tags/v*' 2>/dev/null \
    | awk '{print $2}' \
    | sed 's|refs/tags/||; s/\^{}$//' \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    | sed 's/^v//' \
    | semver_sort_highest || true
}

latest_github_version() {
  local repo
  repo="$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || echo scalattice/scalattice-agent)"
  gh release list --repo "$repo" --limit 100 --json tagName -q '.[].tagName' 2>/dev/null \
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

git fetch origin --tags --force >/dev/null 2>&1 || true

TIP="$(max_version "$(latest_origin_tag_version)" "$(latest_github_version)")"
if [[ -z "$TIP" ]]; then
  echo "error: could not resolve latest agent tag from origin/GitHub" >&2
  exit 1
fi

CURRENT="$(cargo_version)"
MODE="${1:-}"

if [[ "$MODE" == "--check" ]]; then
  if [[ "$CURRENT" == "$TIP" ]]; then
    echo "==> Cargo.toml already matches published tip v${TIP}"
    exit 0
  fi
  if version_ge "$CURRENT" "$TIP" && [[ "$CURRENT" != "$TIP" ]]; then
    # One unreleased bump ahead of tip is fine (about to ship).
    NEXT_MAJOR="${TIP%%.*}"
    REST="${TIP#*.}"
    NEXT_MINOR="${REST%%.*}"
    NEXT_PATCH="${REST##*.}"
    EXPECTED="${NEXT_MAJOR}.${NEXT_MINOR}.$((NEXT_PATCH + 1))"
    if [[ "$CURRENT" == "$EXPECTED" ]]; then
      echo "==> Cargo.toml v${CURRENT} is the next unreleased patch after tip v${TIP}"
      exit 0
    fi
    echo "error: Cargo.toml v${CURRENT} is ahead of tip v${TIP} by more than one patch" >&2
    echo "       run: ./scripts/align-cargo-version.sh" >&2
    exit 1
  fi
  echo "error: Cargo.toml v${CURRENT} is behind published tip v${TIP}" >&2
  echo "       run: ./scripts/align-cargo-version.sh" >&2
  exit 1
fi

if [[ "$CURRENT" == "$TIP" ]]; then
  echo "==> Cargo.toml already at v${TIP}"
  exit 0
fi

set_cargo_version "$TIP"
echo "==> Cargo.toml aligned to published tip v${TIP} (was v${CURRENT})"
