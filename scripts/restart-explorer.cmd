@echo off
rem Restarts Explorer so it picks up the new handler registration.
taskkill /f /im explorer.exe >nul 2>&1
timeout /t 1 /nobreak >nul
start "" explorer.exe
