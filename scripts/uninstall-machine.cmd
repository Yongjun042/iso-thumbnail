@echo off
setlocal
rem Removes the machine-wide registration and files. Run from an elevated prompt.

net session >nul 2>&1
if errorlevel 1 (
    echo This script needs an elevated ^(Administrator^) prompt.
    exit /b 1
)

set "DEST=%ProgramFiles%\IsoPreview"

rem Unregister with the first CLI that runs, falling back to the DLL's own
rem DllUnregisterServer through regsvr32. Files are deleted only once one worked.
set "UNREGISTERED="
set "FOUND="
for %%C in ("%DEST%\isopreview-cli.exe" "%~dp0isopreview-cli.exe") do (
    if not defined UNREGISTERED if exist "%%~C" (
        set "FOUND=1"
        "%%~C" --uninstall && set "UNREGISTERED=1"
    )
)
for %%D in ("%DEST%\IsoPreview.dll" "%~dp0IsoPreview.dll") do (
    if not defined UNREGISTERED if exist "%%~D" (
        set "FOUND=1"
        regsvr32 /s /u "%%~D" && set "UNREGISTERED=1"
    )
)
if not defined UNREGISTERED (
    if defined FOUND (
        echo Could not remove the registration: isopreview-cli.exe and regsvr32 both failed.
    ) else (
        echo Could not find isopreview-cli.exe or IsoPreview.dll to remove the registration with.
    )
    echo Nothing was deleted.
    exit /b 1
)

if exist "%DEST%" (
    rd /s /q "%DEST%" 2>nul
    if exist "%DEST%" (
        echo The registration was removed. "%DEST%" could not be deleted yet because a
        echo COM surrogate still has the DLL loaded; delete it after restarting Explorer.
    )
)
exit /b 0
