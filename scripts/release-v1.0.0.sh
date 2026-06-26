#!/usr/bin/env bash
# Re-publish v1.0.0: push main, move tag to HEAD, trigger GitHub release workflow.
set -euo pipefail

cd "$(dirname "$0")/.."

REMOTE="${1:-origin}"
TAG="v1.0.0"

echo "==> Remote"
git remote -v

echo "==> Status"
git status -sb

echo "==> Latest commit"
git log -1 --oneline

echo "==> Pushing main"
git push "$REMOTE" main

echo "==> Moving tag $TAG to HEAD"
git tag -d "$TAG" 2>/dev/null || true
git push "$REMOTE" ":refs/tags/$TAG" 2>/dev/null || true
git tag "$TAG"
git push "$REMOTE" "$TAG"

echo ""
echo "Done. Open GitHub Actions and wait for the Release workflow on $TAG."
echo "https://github.com/Robottik-Software/Scalattice-Client/actions"
