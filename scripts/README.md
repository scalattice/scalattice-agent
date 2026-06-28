# Scripts

| Script | Purpose |
|--------|---------|
| **`release.sh --dev`** | **Day-to-day releases.** Bump, build x86_64 Linux, publish + Windows `.exe` in CI — skip aarch64. |
| **`release.sh`** | Full release: x86_64 Linux + aarch64 + Windows → GitHub Releases. |
| `build-release.sh` | Build one Linux target locally (used by `release.sh` for x86_64). |
| `build-release.ps1` | Build Windows x86_64 locally (same output as CI). |
| `bundle-release-libs.sh` | Internal — bundles shared libs into Linux tarballs. |
| `bundle-release-windows.ps1` | Internal — bundles DLLs into Windows zip. |
| `reset-releases.sh` | Wipe all tags/releases and reset semver (keeps commit history). |

## Dev release (daily workflow)

```bash
./scripts/release.sh --dev
```

Same as a full production release, but **skips aarch64 CI** only:

1. Bumps patch version
2. Builds **x86_64 Linux** locally
3. Commits, pushes `main`, creates GitHub Release with x86 tarball
4. Triggers **Windows** build in GitHub Actions and waits (~30–60 min on cold cache)
5. Verifies x86 Linux tarball + `ScalatticeAgentSetup-x86_64.exe` are on the release

Use this while iterating on x86 + Windows. Run `./scripts/release.sh` (no flags) when ARM providers need the latest version.

## Full release

```bash
./scripts/release.sh
```

1. Bumps patch version (or publishes current if not on GitHub yet)
2. Builds **x86_64 Linux** on your machine (~minutes with cache)
3. Commits, pushes `main`, creates GitHub Release with x86 tarball
4. Triggers **aarch64 + Windows** builds in GitHub Actions and **waits** for them
5. Verifies all three platform artifacts are on the release

Install scripts pick the right artifact automatically:

| Platform | GitHub asset |
|----------|----------------|
| Linux x86_64 | `scalattice-agent-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `scalattice-agent-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `ScalatticeAgentSetup-x86_64.exe` (GUI installer) · `scalattice-agent-x86_64-pc-windows-msvc.zip` (advanced) |

### Options

| Flag | When |
|------|------|
| `--dev` | Day-to-day: x86_64 Linux + Windows CI (no aarch64) |
| `--skip-build` | Reuse existing `dist/` x86 tarball |
| `--skip-aarch64` | Same as `--dev` |
| `--version 1.0.2` | Pin version |
| `--minor` | Bump minor instead of patch |
| `--no-push` | Dry run |

### Prerequisites (your build machine)

**Linux x86_64 (local build):**

- Rust: https://rustup.rs
- GitHub CLI: `sudo apt install gh && gh auth login`
- CUDA 12.6 dev + Vulkan (see `build-release.sh` error output if missing)

**Windows x86_64 (local build, optional — CI also builds this):**

- Rust stable + Visual Studio C++ build tools
- CUDA 12.6+ (`CUDA_PATH` if not in default location)
- Vulkan SDK
- Run: `.\scripts\build-release.ps1`

### Reset semver

```bash
./scripts/reset-releases.sh --confirm --dry-run
./scripts/reset-releases.sh --confirm
git push origin main
./scripts/release.sh --version 1.0.0
```

### Add platform assets to an existing release

```bash
# aarch64 only
gh workflow run .github/workflows/release.yml --ref main \
  -f tag=v1.0.2 -f targets=aarch64-only

# Windows only
gh workflow run .github/workflows/release.yml --ref main \
  -f tag=v1.0.2 -f targets=windows-only

# aarch64 + Windows
gh workflow run .github/workflows/release.yml --ref main \
  -f tag=v1.0.2 -f targets=full

gh run watch   # pick the run id from gh run list
```
