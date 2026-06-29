@echo off
setlocal
set "INSTALL=%~dp0"
set "LIB=%LOCALAPPDATA%\Scalattice\lib"
if not exist "%LIB%" set "LIB=%INSTALL%lib"
set "PATH=%INSTALL%;%LIB%;%PATH%"
cd /d "%INSTALL%"
if /I "%~1"=="tray" (
  rem Hidden tray (Startup / installer). Use open-tray-debug.cmd to see errors.
  if exist "%INSTALL%launch-tray.vbs" (
    wscript.exe //nologo "%INSTALL%launch-tray.vbs"
    exit /b 0
  )
)
if /I "%~1"=="tray-debug" (
  "%INSTALL%scalattice-agent.exe" tray --force
  exit /b %ERRORLEVEL%
)
"%INSTALL%scalattice-agent.exe" %*
