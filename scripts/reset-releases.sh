#!/usr/bin/env bash
# Delete all v* tags and GitHub Releases. Does NOT touch git commits on main.
#
# Usage:
#   ./scripts/reset-releases.sh --confirm          # wipe tags/releases, set Cargo.toml to 1.0.0
#   ./scripts/reset-releases.sh --confirm --dry-run
#
# After reset, publish a fresh 1.0.0 (rebuild so --version matches):
#   ./scripts/release.sh --version 1.0.0
set -euo pipefail

cd "$(dirname "$0")/.."

REMOTE="origin"
CONFIRM="false"
DRY_RUN="false"
RESET_CARGO="true"

usage() {
  cat <<'EOF'
Delete all v* tags and GitHub Releases. Commit history on main is unchanged.

  ./scripts/reset-releases.sh --confirm
  ./scripts/reset-releases.sh --confirm --dry-run
  ./scripts/reset-releases.sh --confirm --no-reset-cargo   # only delete tags/releases

Then publish fresh 1.0.0 (rebuild — embedded version comes from Cargo.toml):
  ./scripts/release.sh --version 1.0.0

Requires: gh auth login
EOF
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --confirm) CONFIRM="true"; shift ;;
    --dry-run) DRY_RUN="true"; shift ;;
    --remote) REMOTE="${2:?}"; shift 2 ;;
    --no-reset-cargo) RESET_CARGO="false"; shift ;;
    *) echo "Unknown option: $1" >&2; usage 1 ;;
  esac
done

if [[ "$CONFIRM" != "true" ]]; then
  echo "This deletes every v* tag and GitHub Release. Commits on main are kept." >&2
  echo "Re-run with --confirm to proceed (add --dry-run to preview)." >&2
  exit 1
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "gh CLI required: sudo apt install gh && gh auth login" >&2
  exit 1
fi

if ! gh auth status >/dev/null 2>&1; then
  echo "Run: gh auth login" >&2
  exit 1
fi

mapfile -t TAGS < <(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' | sort -V)
if [[ ${#TAGS[@]} -eq 0 ]]; then
  echo "No semver tags found locally."
else
  echo "==> Tags to remove (${#TAGS[@]}):"
  printf '    %s\n' "${TAGS[@]}"
fi

mapfile -t RELEASES < <(gh release list --limit 500 --json tagName -q '.[].tagName' 2>/dev/null | sort -V || true)
if [[ ${#RELEASES[@]} -gt 0 ]]; then
  echo "==> GitHub releases to delete (${#RELEASES[@]}):"
  printf '    %s\n' "${RELEASES[@]}"
fi

if [[ "$DRY_RUN" == "true" ]]; then
  echo ""
  echo "Dry run — nothing changed."
  [[ "$RESET_CARGO" == "true" ]] && echo "Would set Cargo.toml version to 1.0.0"
  exit 0
fi

for tag in "${RELEASES[@]}"; do
  echo "==> Deleting GitHub release ${tag}"
  gh release delete "$tag" --yes --cleanup-tag 2>/dev/null || gh release delete "$tag" --yes || true
done

for tag in "${TAGS[@]}"; do
  echo "==> Deleting remote tag ${tag}"
  git push "$REMOTE" ":refs/tags/${tag}" 2>/dev/null || true
  git tag -d "$tag" 2>/dev/null || true
done

# Catch tags that exist on remote but not locally
mapfile -t REMOTE_TAGS < <(git ls-remote --tags "$REMOTE" 'refs/tags/v*' 2>/dev/null \
  | awk '{print $2}' | sed 's|refs/tags/||' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V || true)
for tag in "${REMOTE_TAGS[@]}"; do
  echo "==> Deleting orphan remote tag ${tag}"
  git push "$REMOTE" ":refs/tags/${tag}" 2>/dev/null || true
done

if [[ "$RESET_CARGO" == "true" ]]; then
  CURRENT="$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')"
  if [[ "$CURRENT" != "1.0.0" ]]; then
    echo "==> Setting Cargo.toml: ${CURRENT} → 1.0.0"
    sed -i 's/^version = ".*"/version = "1.0.0"/' Cargo.toml
    if [[ -n "$(git status --porcelain Cargo.toml)" ]]; then
      git add Cargo.toml
      git commit -m "Reset package version to 1.0.0"
      echo "==> Committed version reset (push main before releasing)"
    fi
  fi
fi

echo ""
echo "Done. Tags and releases cleared; commit history unchanged."
echo ""
echo "Next:"
echo "  git push ${REMOTE} main          # if version commit was created"
echo "  ./scripts/release.sh --version 1.0.0   # rebuild — do not --skip-build old tarball"
