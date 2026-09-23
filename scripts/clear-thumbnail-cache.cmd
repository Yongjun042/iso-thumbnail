@echo off
rem Deletes Explorer's thumbnail cache so every thumbnail (including .iso files
rem viewed before the handler was installed) is generated again.
taskkill /f /im explorer.exe >nul 2>&1
rem ping instead of timeout: timeout fails when stdin is redirected.
ping -n 2 127.0.0.1 >nul
del /f /q "%LocalAppData%\Microsoft\Windows\Explorer\thumbcache_*.db" 2>nul
start "" explorer.exe
