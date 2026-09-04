@echo off
rem replaycut installer: unblock the downloaded files, then let the
rem executable do the work. Keep this window open at the end.
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -Command "Get-ChildItem -LiteralPath '%~dp0' -Recurse -File | Unblock-File -ErrorAction SilentlyContinue"
"%~dp0replaycut.exe" install
echo.
pause
