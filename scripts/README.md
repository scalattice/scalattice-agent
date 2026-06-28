# Scripts

| Script | Purpose |
|--------|---------|
| **`release.sh`** | **Start here.** Bump version, build, commit, push, and publish to GitHub Releases. |
| `build-release.sh` | Build + package one Linux target (called by `release.sh`; use alone to compile only). |
| `bundle-release-libs.sh` | Internal helper — copies shared libs into the release tarball. Not run directly. |

## Release (maintainers)

One command after setup:

```bash
./scripts/release.sh
```

That will:

1. Pick the version (publish current `Cargo.toml` if not on GitHub yet, otherwise bump patch)
2. Build the x86_64 tarball (~30 min first time, faster after)
3. Commit `Cargo.toml` / `Cargo.lock`
4. Push `main`, create the GitHub release, push tag `vX.Y.Z [local]`

**Already built?** Skip recompile:

```bash
./scripts/release.sh --skip-build
```

**Options:** `--version 1.0.32` · `--minor` · `--extra dist/scalattice-agent-aarch64-unknown-linux-gnu.tar.gz` · `--no-push` (dry run)

### Prerequisites (Ubuntu 24.04 x86_64)

- Rust: https://rustup.rs
- GitHub CLI: `sudo apt install gh && gh auth login`
- Build deps: `clang cmake build-essential pkg-config patchelf glslc libvulkan-dev libshaderc-dev spirv-tools`
- CUDA 12.6 dev: see error output from `build-release.sh` or `.github/workflows/release.yml`

### aarch64

Build on ARM hardware, then attach to the same release:

```bash
./scripts/build-release.sh aarch64-unknown-linux-gnu
./scripts/release.sh --skip-build --extra dist/scalattice-agent-aarch64-unknown-linux-gnu.tar.gz
```

(Use `--version` if you already published x86_64 for that tag.)

### Reset tag/release history (keep commits)

To wipe all `v*` tags and GitHub Releases and start semver at **1.0.0** again
without rewriting `main`:

```bash
./scripts/reset-releases.sh --confirm --dry-run   # preview
./scripts/reset-releases.sh --confirm             # delete tags/releases, set Cargo.toml to 1.0.0
git push origin main
./scripts/release.sh --version 1.0.0              # must rebuild (old tarballs embed old version)
```

**Note:** Anyone using `releases/latest` or an old `v1.0.31` URL must reinstall after
the new `v1.0.0` is published.
