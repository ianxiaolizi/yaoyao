@echo off
powershell -NoProfile -File "%~dp0check.ps1"
exit /b %errorlevel%
