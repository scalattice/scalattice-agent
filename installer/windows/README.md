# Windows GUI installer

Providers on Windows should use **`ScalatticeAgentSetup-x86_64.exe`**, not PowerShell.

## Build

After `scripts/build-release.ps1` populates `dist/`:

```powershell
.\scripts\build-windows-installer.ps1
```

Requires [Inno Setup 6](https://jrsoftware.org/isinfo.php) (`choco install innosetup`).

## What the installer does

1. Welcome
2. **If an NVIDIA GPU is present but `nvidia-smi` fails:** recommend the matching Game Ready driver (one-click when lookup works) + Recheck. CPU-only / non-NVIDIA PCs skip this page.
3. Token input (`slt_provider_…`)
4. **Reinstall only:** optional page to remove stored model weights (shows size in GB)
5. Copies `scalattice-agent.exe` to `%LOCALAPPDATA%\Scalattice\bin`
6. Copies bundled DLLs (including CUDA 12 runtime) to `%LOCALAPPDATA%\Scalattice\lib`
7. Adds both folders to the user `PATH`
8. Runs `scalattice-agent set-token` to register the background scheduled task
9. Starts the notification area control panel (`scalattice-agent tray`)
10. Adds Start Menu shortcuts (notification-area agent + provider dashboard). Upgrades remove the old "Scalattice Agent (debug)" shortcut if present.

Silent install with token (IT/automation):

```text
ScalatticeAgentSetup-x86_64.exe /TOKEN=slt_provider_… /VERYSILENT
```

**In-app / CLI update:** `scalattice-agent update` (and the tray Updates button) download the latest `ScalatticeAgentSetup-x86_64.exe` and launch the setup wizard. Finish the wizard to complete the upgrade — silent in-place replace is not used.

**Reinstall / upgrade:** Setup stops any running Scalattice Agent (tray + background), clears the bundled `lib` folder, then installs fresh CUDA/runtime DLLs. If setup reports that libraries could not be replaced, quit the agent from the notification area or Task Manager and run setup again.

## Tray control panel

After install, a Scalattice icon stays in the Windows notification area (system tray). Click it to open a small panel with:

- Connection and service status
- Token change
- Live log tail from the background agent

Uninstall via Windows Settings → Apps, or:

```text
scalattice-agent uninstall --yes
```

Add `--purge` to also delete cached model weights (`%USERPROFILE%\.cache\scalattice\models`).
