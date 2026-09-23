@echo off
setlocal
rem Removes the per-user registration and the installed files.

set "DEST=%LocalAppData%\Programs\IsoPreview"
if exist "%DEST%\isopreview-cli.exe" (
    rem Fall back to regsvr32 if the CLI is blocked, e.g. by an antivirus heuristic.
    "%DEST%\isopreview-cli.exe" --uninstall || regsvr32 /s /u /n /i:user "%DEST%\IsoPreview.dll"
) else if exist "%~dp0isopreview-cli.exe" (
    "%~dp0isopreview-cli.exe" --uninstall
) else if exist "%~dp0..\target\release\isopreview-cli.exe" (
    "%~dp0..\target\release\isopreview-cli.exe" --uninstall
) else (
    regsvr32 /s /u /n /i:user "%DEST%\IsoPreview.dll"
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
