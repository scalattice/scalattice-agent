@echo off
rem Debug launcher: shows tray startup errors in this console window.
setlocal
set "INSTALL=%~dp0"
set "LIB=%LOCALAPPDATA%\Scalattice\lib"
set "PATH=%INSTALL%;%LIB%;%PATH%"
cd /d "%INSTALL%"
echo Starting Scalattice tray (close this window to stop the tray UI only)...
"%INSTALL%scalattice-agent.exe" tray --force
echo Exit code: %ERRORLEVEL%
pause
