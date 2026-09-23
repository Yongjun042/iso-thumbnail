@echo off
setlocal
rem Machine-wide installation for all users. Run from an elevated (Administrator) prompt.
rem Copies the binaries to %ProgramFiles%\IsoPreview and registers under HKEY_LOCAL_MACHINE.

net session >nul 2>&1
if errorlevel 1 (
    echo This script needs an elevated ^(Administrator^) prompt.
    exit /b 1
)

set "SRC=%~dp0"
if not exist "%SRC%IsoPreview.dll" set "SRC=%~dp0..\target\release\"
if not exist "%SRC%IsoPreview.dll" (
    echo IsoPreview.dll not found next to this script or in target\release.
    echo Build it first:  cargo build --release
    exit /b 1
)

set "DEST=%ProgramFiles%\IsoPreview"
if not exist "%DEST%" mkdir "%DEST%"
copy /y "%SRC%IsoPreview.dll" "%DEST%\" >nul || goto :copyfail
copy /y "%SRC%isopreview-cli.exe" "%DEST%\" >nul || goto :copyfail

"%DEST%\isopreview-cli.exe" --install-machine --dll "%DEST%\IsoPreview.dll"
if errorlevel 1 exit /b 1

echo.
echo Installed to "%DEST%" and registered for all users.
echo Users who already looked at .iso files may need to restart Explorer.
exit /b 0

:copyfail
echo Could not copy the files to "%DEST%". If an older version is installed its DLL
echo may still be loaded: run restart-explorer.cmd and try again.
exit /b 1
