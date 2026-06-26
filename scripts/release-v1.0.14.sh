#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
REMOTE="${1:-origin}"
TAG="v1.0.14"
git push "$REMOTE" main
if git rev-parse "$TAG" >/dev/null 2>&1; then
  git tag -d "$TAG"
  git push "$REMOTE" ":refs/tags/$TAG" 2>/dev/null || true
fi
git tag "$TAG"
git push "$REMOTE" "$TAG"
echo "Tagged $TAG - wait for GitHub Actions release workflow."
