@echo off
setlocal
set "INSTALL=%~dp0"
set "LIB=%LOCALAPPDATA%\Scalattice\lib"
if not exist "%LIB%" set "LIB=%INSTALL%lib"
set "PATH=%INSTALL%;%LIB%;%PATH%"
cd /d "%INSTALL%"

call :CheckCudaRuntime
if errorlevel 1 (
  echo.
  echo Scalattice Agent cannot start: CUDA 12 runtime DLLs are missing.
  echo Expected under: %LIB%
  echo   cudart64_12.dll
  echo   cublas64_12.dll
  echo   cublasLt64_12.dll
  echo.
  echo Reinstall Scalattice Agent from https://scalattice.cloud
  echo Do not launch scalattice-agent.exe directly without the installer bundle.
  call :LogCudaMissing
  exit /b 1
)

if /I "%~1"=="tray" (
  rem Hidden tray (Startup / installer). Use open-tray-debug.cmd to see errors.
  if exist "%INSTALL%launch-tray.vbs" (
    wscript.exe //nologo "%INSTALL%launch-tray.vbs"
    exit /b 0
  )
)
if /I "%~1"=="tray-open" (
  if exist "%INSTALL%launch-tray-interactive.vbs" (
    wscript.exe //nologo "%INSTALL%launch-tray-interactive.vbs"
    exit /b 0
  )
)
if /I "%~1"=="tray-debug" (
  "%INSTALL%scalattice-agent.exe" tray --force
  exit /b %ERRORLEVEL%
)
"%INSTALL%scalattice-agent.exe" %*
exit /b %ERRORLEVEL%

:CheckCudaRuntime
if not exist "%LIB%\cudart64_12.dll" if not exist "%INSTALL%cudart64_12.dll" exit /b 1
if not exist "%LIB%\cublas64_12.dll" if not exist "%INSTALL%cublas64_12.dll" exit /b 1
if not exist "%LIB%\cublasLt64_12.dll" if not exist "%INSTALL%cublasLt64_12.dll" exit /b 1
exit /b 0

:LogCudaMissing
set "LOGDIR=%LOCALAPPDATA%\Scalattice\logs"
if not exist "%LOGDIR%" mkdir "%LOGDIR%" >nul 2>&1
>>"%LOGDIR%\agent.log" echo [%DATE% %TIME%] CUDA runtime missing under %LIB% — reinstall Scalattice Agent
exit /b 0
