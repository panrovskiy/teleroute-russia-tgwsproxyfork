@echo off
setlocal
cd /d "%~dp0"
where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust/Cargo is not installed.
  echo For a no-toolchain build, use the GitHub Actions workflow instead.
  pause
  exit /b 1
)
if not exist "wintun\wintun.dll" (
  echo wintun\wintun.dll is required for local builds.
  echo The official signed DLL is downloaded automatically by the GitHub Actions build.
  pause
  exit /b 1
)
cargo build --release
if errorlevel 1 (
  pause
  exit /b %errorlevel%
)
echo.
echo Built: target\release\tele-route.exe
pause
