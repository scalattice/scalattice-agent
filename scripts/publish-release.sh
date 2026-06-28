#!/usr/bin/env bash
# Fast release path: build on your machine, upload to GitHub Releases, skip CI compile.
#
# Usage:
#   ./scripts/publish-release.sh v1.0.31
#   ./scripts/publish-release.sh v1.0.31 origin
#   ./scripts/publish-release.sh v1.0.31 origin dist/scalattice-agent-aarch64-unknown-linux-gnu.tar.gz
#
# Builds x86_64 on this machine. For aarch64, run build-release.sh on ARM hardware first,
# then pass the extra tarball as shown above.
#
# Tags include [local] so GitHub Actions skips the ~1h GPU compile job.
set -euo pipefail

cd "$(dirname "$0")/.."

TAG="${1:?usage: publish-release.sh vX.Y.Z [remote] [extra-tarball ...]}"
REMOTE="${2:-origin}"
shift $(( $# > 1 ? 2 : 1 ))

if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Tag must look like v1.0.31 (got: $TAG)" >&2
  exit 1
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "gh CLI required: https://cli.github.com" >&2
  exit 1
fi

VERSION="${TAG#v}"
CURRENT="$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')"
if [[ "$CURRENT" != "$VERSION" ]]; then
  echo "Cargo.toml version is $CURRENT but tag is $VERSION — bump Cargo.toml first." >&2
  exit 1
fi

echo "==> Building x86_64 release (llama.cpp + CUDA/Vulkan — not clap — this is the slow step)"
./scripts/build-release.sh x86_64-unknown-linux-gnu

ASSETS=(dist/scalattice-agent-x86_64-unknown-linux-gnu.tar.gz "$@")
for asset in "${ASSETS[@]}"; do
  if [[ ! -f "$asset" ]]; then
    echo "Missing release asset: $asset" >&2
    exit 1
  fi
done

echo "==> Pushing main"
git push "$REMOTE" main

if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "==> Replacing existing tag $TAG"
  git tag -d "$TAG"
  git push "$REMOTE" ":refs/tags/$TAG" 2>/dev/null || true
  gh release delete "$TAG" --yes 2>/dev/null || true
fi

echo "==> Tagging $TAG with [local] (CI will skip compile)"
git tag -a "$TAG" -m "${TAG} [local]" -m "Binaries built on maintainer hardware via scripts/publish-release.sh"

echo "==> Creating GitHub release with prebuilt tarballs"
gh release create "$TAG" "${ASSETS[@]}" \
  --generate-notes \
  --title "$TAG"

echo "==> Pushing tag (Release workflow verifies assets only — no compile)"
git push "$REMOTE" "$TAG"

echo ""
echo "Done. Release: https://github.com/$(gh repo view --json nameWithOwner -q .nameWithOwner)/releases/tag/$TAG"
