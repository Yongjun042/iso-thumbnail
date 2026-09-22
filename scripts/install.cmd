@echo off
setlocal
rem Per-user installation: no administrator rights needed.
rem Copies IsoPreview.dll + isopreview-cli.exe to %LocalAppData%\Programs\IsoPreview
rem and registers the thumbnail handler for .iso files under HKEY_CURRENT_USER.

set "SRC=%~dp0"
if not exist "%SRC%IsoPreview.dll" set "SRC=%~dp0..\target\release\"
if not exist "%SRC%IsoPreview.dll" (
    echo IsoPreview.dll not found next to this script or in target\release.
    echo Build it first:  cargo build --release
    exit /b 1
)

set "DEST=%LocalAppData%\Programs\IsoPreview"
if not exist "%DEST%" mkdir "%DEST%"
copy /y "%SRC%IsoPreview.dll" "%DEST%\" >nul || goto :copyfail
copy /y "%SRC%isopreview-cli.exe" "%DEST%\" >nul || goto :copyfail

"%DEST%\isopreview-cli.exe" --install --dll "%DEST%\IsoPreview.dll"
if errorlevel 1 exit /b 1

echo.
echo Installed to %DEST% and registered for the current user.
echo If .iso files you looked at earlier still show the plain disc icon, run
echo restart-explorer.cmd (or clear-thumbnail-cache.cmd to rebuild every thumbnail).
exit /b 0

:copyfail
echo Could not copy the files to %DEST%.
echo If an older version is installed, its DLL may still be loaded: run
echo restart-explorer.cmd and try again.
exit /b 1
