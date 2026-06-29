# Scripts

| Script | Purpose |
|--------|---------|
| **`release.sh --dev`** | **One command:** Linux x86_64 local + Windows on self-hosted runner. |
| **`release.sh`** | Full release: Linux + Windows (self-hosted) + aarch64 (GitHub ARM). |
| `setup-windows-build.ps1` | One-time Windows build deps (VS, CUDA, Inno Setup, Rust). |
| `install-windows-runner.ps1` | Register Windows machine as GitHub Actions runner. |
| `check-windows-runner.sh` | Verify runner is online (called by `release.sh`). |
| `build-release.sh` | Build Linux x86_64 locally. |
| `build-release.ps1` | Build Windows (used by self-hosted runner). |

## One-command release (after setup)

**On Linux (onsite):**
```bash
./scripts/release.sh --dev
```

1. Builds **x86_64 Linux** locally  
2. Pushes, creates GitHub Release with Linux tarball  
3. Triggers Windows build on your **self-hosted runner**  
4. Waits and uploads `ScalatticeAgentSetup-x86_64.exe` + zip  

## One-time Windows setup

On a Windows PC or VM (**Administrator** — cmd or PowerShell):

```cmd
git clone https://github.com/Robottik-Software/Scalattice-Client.git
cd Scalattice-Client
gh auth login
scripts\setup-windows-build.cmd
scripts\install-windows-runner.cmd
```

If PowerShell blocks `.ps1` files (`running scripts is disabled`), use the `.cmd` wrappers above, or run:

```powershell
Set-ExecutionPolicy Bypass -Scope Process -Force
.\scripts\setup-windows-build.ps1
```

The runner installs as a Windows service (`C:\actions-runner-scalattice`) and stays online for future releases.

Verify from Linux:
```bash
./scripts/check-windows-runner.sh
```

## Full release

```bash
./scripts/release.sh
```

Same as `--dev`, plus **aarch64** on GitHub-hosted ARM runners.

## Options

| Flag | When |
|------|------|
| `--dev` | Linux + Windows only (no aarch64) |
| `--skip-build` | Reuse existing Linux `dist/` tarball |
| `--local-windows` | Upload `dist/*.exe` from disk; skip Windows CI |
| `--github-hosted-windows` | Slow fallback (~1h) on GitHub `windows-2022` |
| `--version 1.0.2` | Pin version |
| `--no-push` | Dry run |

## Artifacts on GitHub Releases

| Platform | Asset |
|----------|--------|
| Linux x86_64 | `scalattice-agent-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `scalattice-agent-aarch64-unknown-linux-gnu.tar.gz` |
| Windows | `ScalatticeAgentSetup-x86_64.exe` · `scalattice-agent-x86_64-pc-windows-msvc.zip` |

## Linux build prerequisites

- Rust: https://rustup.rs  
- GitHub CLI: `gh auth login`  
- CUDA 12.6 dev + Vulkan (see `build-release.sh` if missing)

## Emergency: re-run Windows CI only

```bash
gh workflow run .github/workflows/release.yml --ref main \
  -f tag=v1.0.2 -f targets=windows-only -f windows_runner=self-hosted
gh run watch
```
