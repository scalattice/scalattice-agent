# Windows GUI installer

Providers on Windows should use **`ScalatticeAgentSetup-x86_64.exe`**, not PowerShell.

## Build

After `scripts/build-release.ps1` populates `dist/`:

```powershell
.\scripts\build-windows-installer.ps1
```

Requires [Inno Setup 6](https://jrsoftware.org/isinfo.php) (`choco install innosetup`).

## What the installer does

1. Welcome + token input (`slt_provider_…`)
2. Copies `scalattice-agent.exe` to `%LOCALAPPDATA%\Scalattice\bin`
3. Copies bundled DLLs to `%LOCALAPPDATA%\Scalattice\lib`
4. Adds both folders to the user `PATH`
5. Runs `scalattice-agent set-token` to register the background scheduled task
6. Starts the notification area control panel (`scalattice-agent tray`)
7. Adds Start Menu shortcuts (tray panel + provider dashboard)

Silent install with token (IT/automation):

```text
ScalatticeAgentSetup-x86_64.exe /TOKEN=slt_provider_… /VERYSILENT
```

## Tray control panel

After install, a Scalattice icon stays in the Windows notification area (system tray). Click it to open a small panel with:

- Connection and service status
- Token change
- Live log tail from the background agent

Uninstall via Windows Settings → Apps, or:

```text
scalattice-agent uninstall --yes
```
