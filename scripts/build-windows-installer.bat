@echo off
REM Build the full Windows installer: the engine bundled with the GUI as a sidecar.
REM Run from the repository root: scripts\build-windows-installer.bat
REM Output: target\release\bundle\nsis\pbsgui_<version>_x64-setup.exe
setlocal

REM Stamp the build with the short commit so you can tell builds apart in-app.
set SHORT=local
for /f %%i in ('git rev-parse --short HEAD 2^>nul') do set SHORT=%%i
set PBSGUI_BUILD=0.1.0-local (%SHORT%)

cargo build -p pbsgui-engine --release || exit /b 1

if not exist src-tauri\binaries mkdir src-tauri\binaries
copy /Y target\release\pbsgui-engine.exe ^
  src-tauri\binaries\pbsgui-engine-x86_64-pc-windows-msvc.exe || exit /b 1

REM engine-sidecar.conf.json adds the engine as a bundled sidecar (externalBin).
tauri build --config src-tauri\engine-sidecar.conf.json

echo.
echo Installer written to target\release\bundle\nsis\
