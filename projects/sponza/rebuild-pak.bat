@echo off
setlocal

where cargo >nul 2>nul
if %ERRORLEVEL% neq 0 (
    if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
        set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
    )
)

set "SPONZA_DIR=%~dp0"
if "%SPONZA_DIR:~-1%"=="\" set "SPONZA_DIR=%SPONZA_DIR:~0,-1%"
set "ROOT_DIR=%SPONZA_DIR%\..\.."

cd /d "%SPONZA_DIR%"

echo [rebuild-pak] Rebuilding Sponza scene package (assets\scenes.pak)...
cargo run -p prism-build-pipeline --manifest-path "%ROOT_DIR%\Cargo.toml" -- build-scenes %*
if %ERRORLEVEL% neq 0 (
    echo [rebuild-pak] ERROR: Failed to rebuild scenes.pak.
    exit /b %ERRORLEVEL%
)

echo [rebuild-pak] Successfully updated assets\scenes.pak!
endlocal
