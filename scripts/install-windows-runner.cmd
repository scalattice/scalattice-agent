@echo off
REM Run as Administrator. Bypasses PowerShell execution policy for this session only.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-windows-runner.ps1" %*
