@echo off
setlocal
set "INSTALL=%~dp0"
set "LIB=%LOCALAPPDATA%\Scalattice\lib"
if not exist "%LIB%" set "LIB=%INSTALL%lib"
set "PATH=%INSTALL%;%LIB%;%PATH%"
cd /d "%INSTALL%"
if /I "%~1"=="tray" (
  if exist "%INSTALL%launch-tray.vbs" (
    wscript.exe //nologo "%INSTALL%launch-tray.vbs"
    exit /b 0
  )
)
"%INSTALL%scalattice-agent.exe" %*
