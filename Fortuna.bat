@echo off
setlocal

set "APP_DIR=%~dp0Fortuna"
set "RELEASE_EXE=%APP_DIR%\target\release\Fortuna.exe"

cd /d "%APP_DIR%"
cargo build --release
if errorlevel 1 exit /b %errorlevel%

"%RELEASE_EXE%"

endlocal
