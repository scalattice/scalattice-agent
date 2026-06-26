#!/usr/bin/env bash
# Publish v1.0.3: push main, tag, trigger GitHub release workflow.
set -euo pipefail

cd "$(dirname "$0")/.."

REMOTE="${1:-origin}"
TAG="v1.0.3"

echo "==> Remote"
git remote -v

echo "==> Status"
git status -sb

echo "==> Latest commit"
git log -1 --oneline

echo "==> Pushing main"
git push "$REMOTE" main

echo "==> Tagging $TAG"
if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "Tag $TAG already exists locally; moving to HEAD"
  git tag -d "$TAG"
  git push "$REMOTE" ":refs/tags/$TAG" 2>/dev/null || true
fi
git tag "$TAG"
git push "$REMOTE" "$TAG"

echo ""
echo "Done. Wait for the Release workflow on $TAG:"
echo "https://github.com/Robottik-Software/Scalattice-Client/actions"
