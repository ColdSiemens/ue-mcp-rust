@echo off
rem Starts ue-mcp-rs for the project this plugin sits in. .mcp.json runs it as
rem   cmd /d /c Plugins\UE_MCP_Bridge\Server\ue-mcp.cmd [server flags]
rem
rem The server ships next to this file, so it syncs with the plugin it must
rem match. It runs from a copy under %LOCALAPPDATA%\ue-mcp-rs\<hash>, never from
rem here: a running exe is locked, and a sync could not replace it. Each build
rem gets its own folder, so a session still running an older one keeps it.
rem
rem stdout is the MCP stream: nothing here may print to it.
setlocal EnableDelayedExpansion
set "SRC=%~dp0ue-mcp-rs.exe"
for %%I in ("%~dp0..\..\..") do set "PROJECT=%%~fI"
if not exist "%SRC%" (
    echo ue-mcp: %SRC% is missing. Sync Plugins\UE_MCP_Bridge\Server, or see its README.md. 1>&2
    exit /b 1
)

set "HASH="
for /f "skip=1 delims=" %%H in ('certutil -hashfile "%SRC%" SHA256 2^>nul') do if not defined HASH set "HASH=%%H"
set "HASH=!HASH: =!"
set "HASH=!HASH:~0,16!"
if not defined HASH (
    echo ue-mcp: could not hash %SRC%. 1>&2
    exit /b 1
)

set "ROOT=%LOCALAPPDATA%\ue-mcp-rs"
set "EXE=%ROOT%\%HASH%\ue-mcp-rs.exe"
if not exist "%EXE%" (
    mkdir "%ROOT%\%HASH%" >nul 2>&1
    copy /y "%SRC%" "%EXE%.tmp" >nul 2>&1 && move /y "%EXE%.tmp" "%EXE%" >nul 2>&1
    rem Drop older builds; one still running stays until its session ends.
    for /d %%D in ("%ROOT%\*") do if /i not "%%~nxD"=="%HASH%" rd /s /q "%%D" >nul 2>&1
)
if not exist "%EXE%" (
    echo ue-mcp: could not copy the server to %EXE%. 1>&2
    exit /b 1
)

"%EXE%" "%PROJECT%" %*
