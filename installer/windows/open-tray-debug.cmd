@echo off
rem Debug launcher: starts tray with console logging, detached from this window.
setlocal
set "INSTALL=%~dp0"
set "LIB=%LOCALAPPDATA%\Scalattice\lib"
if not exist "%LIB%" set "LIB=%INSTALL%lib"
set "PATH=%INSTALL%;%LIB%;%PATH%"
cd /d "%INSTALL%"
echo Starting Scalattice tray (this window can be closed without stopping the tray)...
start "" /B "%INSTALL%scalattice-agent.exe" tray --force
echo Tray launched. Check the notification area for the Scalattice icon.
timeout /t 5 >nul
