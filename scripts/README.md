# Scripts

| Script | Purpose |
|--------|---------|
| **`release.sh --dev`** | **Day-to-day releases.** Bump, build x86_64, publish to GitHub — skip aarch64 CI. |
| **`release.sh`** | Full release: x86_64 + aarch64 → GitHub Releases. |
| `build-release.sh` | Build one target locally (used by `release.sh` for x86_64). |
| `bundle-release-libs.sh` | Internal — bundles shared libs into tarballs. |
| `reset-releases.sh` | Wipe all tags/releases and reset semver (keeps commit history). |

## Dev release (daily workflow)

```bash
./scripts/release.sh --dev
```

Same as a full production release, but **skips aarch64 CI** (~30–60 min saved):

1. Bumps patch version
2. Builds **x86_64** locally
3. Commits, pushes `main`, creates GitHub Release with x86 tarball
4. Skips aarch64 — ARM installs need a later full `./scripts/release.sh` or manual CI run

Use this while iterating on x86 hardware. Run `./scripts/release.sh` (no flags) before you need ARM providers on the latest version.

## Full release

```bash
./scripts/release.sh
```

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
| `--dev` | Production release, x86_64 only (no aarch64 CI) |
| `--skip-build` | Reuse existing `dist/` x86 tarball |
| `--skip-aarch64` | Same as `--dev` |
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

### Add aarch64 to an existing x86-only release

```bash
gh workflow run .github/workflows/release.yml --ref main \
  -f tag=v1.0.2 -f targets=aarch64-only
gh run watch   # pick the run id from gh run list
```
