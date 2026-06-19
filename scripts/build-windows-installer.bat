@echo off
REM Build the full Windows installer: the engine bundled with the GUI as a sidecar.
REM Can be run from anywhere; it changes to the repository root itself.
REM Output: target\release\bundle\nsis\pbsgui_<version>_x64-setup.exe
setlocal

REM Move to the repository root (the directory above this script).
cd /d "%~dp0.." || exit /b 1

REM Stamp the build with the short commit so installer + in-app are identifiable.
REM The "g" prefix keeps the version a valid semver pre-release for any hex SHA.
set SHORT=local
for /f %%i in ('git rev-parse --short HEAD 2^>nul') do set SHORT=%%i
set PBSGUI_BUILD=0.1.0-g%SHORT%
echo {"version":"0.1.0-g%SHORT%"}>build-version.json

cargo build -p pbsgui-engine --release || exit /b 1

if not exist src-tauri\binaries mkdir src-tauri\binaries
copy /Y target\release\pbsgui-engine.exe ^
  src-tauri\binaries\pbsgui-engine-x86_64-pc-windows-msvc.exe || exit /b 1

REM engine-sidecar.conf.json adds the engine as a bundled sidecar (externalBin);
REM build-version.json sets the version so the installer filename carries the SHA.
tauri build --config src-tauri\engine-sidecar.conf.json --config build-version.json

echo.
echo Installer written to target\release\bundle\nsis\
