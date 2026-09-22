@echo off
setlocal
rem Removes the machine-wide registration and files. Run from an elevated prompt.

net session >nul 2>&1
if errorlevel 1 (
    echo This script needs an elevated ^(Administrator^) prompt.
    exit /b 1
)

set "DEST=%ProgramFiles%\IsoPreview"
if exist "%DEST%\isopreview-cli.exe" (
    "%DEST%\isopreview-cli.exe" --uninstall
) else if exist "%~dp0isopreview-cli.exe" (
    "%~dp0isopreview-cli.exe" --uninstall
) else (
    regsvr32 /s /u "%DEST%\IsoPreview.dll"
)

if exist "%DEST%" (
    rd /s /q "%DEST%" 2>nul
    if exist "%DEST%" (
        echo The registration was removed. %DEST% could not be deleted yet because a
        echo COM surrogate still has the DLL loaded; delete it after restarting Explorer.
    )
)
exit /b 0
