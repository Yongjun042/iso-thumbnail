@echo off
rem Restarts Explorer so it picks up the new handler registration.
taskkill /f /im explorer.exe >nul 2>&1
rem ping instead of timeout: timeout fails when stdin is redirected.
ping -n 2 127.0.0.1 >nul
start "" explorer.exe
