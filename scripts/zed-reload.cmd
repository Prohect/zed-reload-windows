@echo off
rem zed-reload.cmd - restart Zed and inject a message into the Agent Panel.
rem Convenience shim for cmd/PowerShell users; runs in the FOREGROUND.
rem For flags and detached launch use the bash wrapper `zed-reload` (MSYS2)
rem or zed-reload.ps1 directly. Message words are joined.
if "%*"=="" (
  powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0zed-reload.ps1"
) else (
  powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0zed-reload.ps1" -Message "%*"
)
