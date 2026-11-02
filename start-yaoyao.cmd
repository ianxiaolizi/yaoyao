@echo off
setlocal
rem ASCII alias for the one-click GUI launcher.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\gui\start.ps1" %*
endlocal
