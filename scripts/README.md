# Scripts

| Script | Purpose |
|--------|---------|
| **`release.sh`** | **Run this after code changes.** Full release: x86_64 + aarch64 → GitHub Releases. |
| `build-release.sh` | Build one target locally (used by `release.sh` for x86_64). |
| `bundle-release-libs.sh` | Internal — bundles shared libs into tarballs. |
| `reset-releases.sh` | Wipe all tags/releases and reset semver (keeps commit history). |

## Release workflow

```bash
./scripts/release.sh
```

One command:

1. Bumps patch version (or publishes current if not on GitHub yet)
2. Builds **x86_64** on your machine (~minutes with cache)
3. Commits, pushes `main`, creates GitHub Release with x86 tarball
4. Triggers **aarch64** build in GitHub Actions and **waits** for it
5. Verifies both tarballs are on the release

The install script picks the right arch automatically:
`scalattice-agent-x86_64-unknown-linux-gnu.tar.gz` or `scalattice-agent-aarch64-unknown-linux-gnu.tar.gz`.

### Options

| Flag | When |
|------|------|
| `--skip-build` | Reuse existing `dist/` x86 tarball; still builds aarch64 in CI |
| `--skip-aarch64` | x86 only (not recommended — breaks ARM installs) |
| `--version 1.0.2` | Pin version |
| `--minor` | Bump minor instead of patch |
| `--no-push` | Dry run |

### Prerequisites (your build machine)

- Rust: https://rustup.rs
- GitHub CLI: `sudo apt install gh && gh auth login`
- CUDA 12.6 dev + Vulkan (see `build-release.sh` error output if missing)

### Reset semver

```bash
./scripts/reset-releases.sh --confirm --dry-run
./scripts/reset-releases.sh --confirm
git push origin main
./scripts/release.sh --version 1.0.0
```

### Fix v1.0.1 (x86 only, missing aarch64)

```bash
gh workflow run .github/workflows/release.yml --ref main \
  -f tag=v1.0.1 -f targets=aarch64-only
gh run watch   # pick the run id from gh run list
```
