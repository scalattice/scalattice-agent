@echo off
setlocal
set "INSTALL=%~dp0"
set "LIB=%LOCALAPPDATA%\Scalattice\lib"
if not exist "%LIB%" set "LIB=%INSTALL%lib"
set "PATH=%INSTALL%;%LIB%;%PATH%"
cd /d "%INSTALL%"

rem Uninstall / set-token must run even when CUDA DLLs are missing or locked.
if /I "%~1"=="uninstall" goto :RunAgent
if /I "%~1"=="set-token" goto :RunAgent

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

call :CheckNvidiaDriver
if errorlevel 1 (
  echo.
  echo WARNING: NVIDIA driver not found (nvcuda.dll).
  echo GPU jobs will not run until you install a Game Ready or Studio driver from:
  echo   https://www.nvidia.com/Download/index.aspx
  echo The agent will still start for CPU-compatible models when this build supports it.
  echo.
  call :LogNvidiaDriverMissing
)

:RunAgent
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

:CheckNvidiaDriver
if exist "%SystemRoot%\System32\nvcuda.dll" exit /b 0
if exist "%SystemRoot%\SysWOW64\nvcuda.dll" exit /b 0
exit /b 1

:LogCudaMissing
set "LOGDIR=%LOCALAPPDATA%\Scalattice\logs"
if not exist "%LOGDIR%" mkdir "%LOGDIR%" >nul 2>&1
>>"%LOGDIR%\agent.log" echo [%DATE% %TIME%] CUDA runtime missing under %LIB% — reinstall Scalattice Agent
exit /b 0

:LogNvidiaDriverMissing
set "LOGDIR=%LOCALAPPDATA%\Scalattice\logs"
if not exist "%LOGDIR%" mkdir "%LOGDIR%" >nul 2>&1
>>"%LOGDIR%\agent.log" echo [%DATE% %TIME%] NVIDIA driver missing (nvcuda.dll) — install Game Ready/Studio driver for GPU jobs
exit /b 0
