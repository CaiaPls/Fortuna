@echo off
setlocal

set "APP_DIR=%~dp0profit-crafter"
set "RELEASE_EXE=%APP_DIR%\target\release\profit-crafter.exe"
set "DEBUG_EXE=%APP_DIR%\target\debug\profit-crafter.exe"

if exist "%RELEASE_EXE%" (
    start "" "%RELEASE_EXE%"
    exit /b 0
)

if exist "%DEBUG_EXE%" (
    start "" "%DEBUG_EXE%"
    exit /b 0
)

cd /d "%APP_DIR%"
cargo run

endlocal
