@echo off
rem replaycut uninstaller. Settings, clips and credentials stay; run
rem "replaycut.exe uninstall --purge" to remove settings and credentials too.
cd /d "%~dp0"
"%~dp0replaycut.exe" uninstall
echo.
pause
