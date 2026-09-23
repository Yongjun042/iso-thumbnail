@echo off
setlocal
rem Removes the per-user registration and the installed files.

set "DEST=%LocalAppData%\Programs\IsoPreview"

rem Unregister with the first CLI that runs. The CLI can be blocked (antivirus
rem heuristics sometimes stop unsigned tools), so fall back to the DLL's own
rem DllInstall through regsvr32. Files are deleted only once one of them worked.
set "UNREGISTERED="
set "FOUND="
for %%C in ("%DEST%\isopreview-cli.exe" "%~dp0isopreview-cli.exe" "%~dp0..\target\release\isopreview-cli.exe") do (
    if not defined UNREGISTERED if exist "%%~C" (
        set "FOUND=1"
        "%%~C" --uninstall && set "UNREGISTERED=1"
    )
)
for %%D in ("%DEST%\IsoPreview.dll" "%~dp0IsoPreview.dll" "%~dp0..\target\release\IsoPreview.dll") do (
    if not defined UNREGISTERED if exist "%%~D" (
        set "FOUND=1"
        regsvr32 /s /u /n /i:user "%%~D" && set "UNREGISTERED=1"
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
        echo The registration was removed. "%DEST%" could not be deleted yet because
        echo Explorer's COM surrogate still has the DLL loaded; delete it after
        echo running restart-explorer.cmd or after signing out.
    )
)
exit /b 0
