@echo off
chcp 65001 >nul
cd /d "%~dp0"
echo Starting FTP mount GUI...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0gui.ps1"
if errorlevel 1 pause
