@echo off
REM Bypass PowerShell execution policy for this session only.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup-windows-build.ps1" %*
