# Scripts

Most day-to-day work is **one command on Linux**. Everything else is setup or troubleshooting.

## What to run

| You want to… | Command | Where |
|--------------|---------|--------|
| **Ship a release** (Linux + Windows) | `./scripts/release.sh --dev` | Linux build host |
| **Reclaim cargo `deps/` agent hashes** | `./scripts/prune-cargo-target.sh` | Linux (safe; keeps llama.cpp) |
| **Ship all platforms** (+ aarch64 CI) | `./scripts/release.sh` | Linux build host |
| **First-time Windows build machine** | `scripts\setup-windows-build.cmd` | Windows (Admin) |
| **Register Windows CI runner** | `scripts\install-windows-runner.cmd` | Windows (Admin) |
| **Check runner is online** | `./scripts/check-windows-runner.sh` | Linux |
| **Troubleshoot Windows install** | `.\scripts\diagnose-windows.ps1` | Windows |
| **Reset GitHub tags/releases** | `./scripts/reset-releases.sh --confirm` | Linux (rare) |

---

## Release flow

```
Linux build host                       Windows (self-hosted runner)
─────────────────                      ───────────────────────────
./scripts/release.sh --dev
  ├─ build-release.sh        local x86_64 tarball
  ├─ gh release create
  └─ trigger GHA workflow ─────────────► prepare-windows-ci.ps1
                                           build-release.ps1
                                             ├─ sync-cargo-version.ps1
                                             ├─ bundle-release-windows.ps1
                                             └─ build-windows-installer.ps1
```

**`release.sh --dev`** — Linux x86_64 here + Windows on your runner (skip aarch64 Linux and macOS).  
**`release.sh`** — same + aarch64 Linux on GitHub `ubuntu-24.04-arm` + macOS Metal on `macos-14`.  
Merging `development` → `production` runs the full Release workflow (including Linux x86_64 in CI). Each platform job vets auto-update **before** the asset is uploaded: a loopback mock serves the just-built package as v99.0.0, then the agent must download, replace itself, and come back running (remote `control/update` and CLI `update`). There is never a future GitHub release to update to while cutting this one.

Useful flags: `--version 1.0.2`, `--skip-build`, `--local-windows`, `--github-hosted-windows`, `--no-push`.

---

## File map

### Release orchestration

| Script | Purpose |
|--------|---------|
| `release.sh` | Main entry: build Linux, create GitHub release, trigger Windows/aarch64 CI, upload assets |
| `ci-prepare-release.sh` | Used by the Release workflow: tag + GitHub release from `Cargo.toml` |
| `ci-update-smoke.py` | Release-gate: mock Cloud + fake newer version, prove live/remote and CLI update come back running (Linux/macOS/Windows) |
| `run-ci-update-smoke.ps1` | Windows wrapper: use PATH Python or the embeddable zip (no admin installer) |
| `reset-releases.sh` | Delete all tags/releases and reset `Cargo.toml` to 1.0.0 (destructive; needs `--confirm`) |
| `check-windows-runner.sh` | Verify an online self-hosted runner with label `scalattice-release` (used by `release.sh`) |

### Linux build

| Script | Purpose |
|--------|---------|
| `build-release.sh` | `cargo build` x86_64/aarch64 + tarball (called by `release.sh` or manually) |
| `sign-macos.sh` | Codesign + notarize an arm64 binary (run on macOS; used by CI) |

### Windows build (CI + local)

| Script | Purpose |
|--------|---------|
| `windows-build-common.ps1` | Shared library: Rust/CUDA/MSVC paths, runner bootstrap (**do not run directly**) |
| `prepare-windows-ci.ps1` | GHA self-hosted prep: PATH, Rust, `LIBCLANG_PATH`, short `CARGO_TARGET_DIR` |
| `sync-cargo-version.ps1` | Set `Cargo.toml` version to match release tag |
| `build-release.ps1` | Full Windows release build (exe + zip + installer) |
| `bundle-release-windows.ps1` | Copy CUDA/runtime DLLs into `dist/lib` |
| `package-macos.sh` | Build `Scalattice Agent.app` + `ScalatticeAgentSetup-aarch64.dmg` |
| `sign-macos.sh` | Codesign + notarize the Mac app/dmg |

### Windows one-time setup

| Script | Purpose |
|--------|---------|
| `setup-windows-build.ps1` | Install VS C++, CUDA 12.6, LLVM, Inno Setup, system Rust at `C:\Rust` |
| `setup-windows-build.cmd` | Admin wrapper (bypasses execution policy) |
| `install-windows-runner.ps1` | Register machine as GHA runner (`scalattice-release` label) |
| `install-windows-runner.cmd` | Admin wrapper |

### Windows diagnostics

| Script | Purpose |
|--------|---------|
| `diagnose-windows.ps1` | Installed agent health, logs, autostart, CUDA DLLs (exits 1 if CUDA runtime missing) |
| | `-Bundle` — check `dist/` before shipping |
| | `-InstalledOnly` — CUDA DLLs under `%LOCALAPPDATA%\Scalattice` |
| | `-LaunchTray` — open tray UI in current console |

---

## One-time Windows setup

Administrator **cmd** or PowerShell:

```cmd
git clone https://github.com/scalattice/scalattice-agent.git
cd scalattice-agent
gh auth login
scripts\setup-windows-build.cmd
scripts\install-windows-runner.cmd
```

Verify from Linux:

```bash
./scripts/check-windows-runner.sh
```

---

## Linux build prerequisites

- Rust: https://rustup.rs  
- GitHub CLI: `gh auth login`  
- CUDA 12.6 dev + Vulkan (`build-release.sh` prints apt hints if missing)

---

## Emergency: Windows CI only

```bash
gh workflow run release.yml -R scalattice/scalattice-agent \
  -f tag=v1.0.0 -f targets=windows-only -f windows_runner=self-hosted
gh run watch
```

---

## GitHub release assets

| Platform | Asset |
|----------|--------|
| Linux x86_64 | `scalattice-agent-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `scalattice-agent-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `ScalatticeAgentSetup-aarch64.dmg`, `scalattice-agent-aarch64-apple-darwin.tar.gz` |
| Windows | `ScalatticeAgentSetup-x86_64.exe`, `scalattice-agent-x86_64-pc-windows-msvc.zip` |
