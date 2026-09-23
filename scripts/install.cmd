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
rem Only ever run the CLI copied in this run: older versions register the
rem handler with DisableProcessIsolation=1.
del /f /q "%DEST%\isopreview-cli.exe" 2>nul
set "CLI_COPIED="
copy /y "%SRC%isopreview-cli.exe" "%DEST%\" >nul && set "CLI_COPIED=1"
if not defined CLI_COPIED echo Could not copy isopreview-cli.exe.

rem The CLI can be missing or blocked (antivirus heuristics sometimes stop unsigned
rem tools), so fall back to the DLL's own DllInstall through regsvr32.
set "REGISTERED="
if defined CLI_COPIED (
    "%DEST%\isopreview-cli.exe" --install --dll "%DEST%\IsoPreview.dll" && set "REGISTERED=1"
)
if not defined REGISTERED (
    echo Registering with regsvr32 instead.
    regsvr32 /s /n /i:user "%DEST%\IsoPreview.dll" || goto :regfail
)

echo.
echo Installed to "%DEST%" and registered for the current user.
echo If .iso files you looked at earlier still show the plain disc icon, run
echo restart-explorer.cmd (or clear-thumbnail-cache.cmd to rebuild every thumbnail).
exit /b 0

:copyfail
echo Could not copy the files to "%DEST%".
echo An installed DLL may still be loaded by the thumbnail host (dllhost.exe), which
echo exits on its own after a short while, or by Explorer if an older version loaded
echo it in-process. Wait a moment or run restart-explorer.cmd, then try again.
exit /b 1

:regfail
echo Could not register "%DEST%\IsoPreview.dll".
exit /b 1
