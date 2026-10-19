@echo off
setlocal
cd /d "%~dp0"
rem Rust builds are always self-contained; --self-contained is accepted for compatibility.
echo Building Roch CPU...
cargo build --release
if errorlevel 1 (
  echo Build failed.
  exit /b 1
)
if not exist dist mkdir dist
if not exist dist\pawnio mkdir dist\pawnio
copy /y "target\release\roch-cpu.exe" "dist\Roch CPU.exe" >nul || exit /b 1
copy /y "drivers\WinRing0x64.sys" "dist\" >nul || exit /b 1
copy /y "drivers\pawnio\RyzenSMU.bin" "dist\pawnio\" >nul || exit /b 1
copy /y "drivers\pawnio\LICENSE-LGPL-2.1.txt" "dist\pawnio\" >nul || exit /b 1
copy /y "README.md" "dist\" >nul
copy /y "assets\RochCPU.ico" "dist\" >nul
copy /y "assets\mark.png" "dist\" >nul
echo.
echo Output: %~dp0dist\Roch CPU.exe
endlocal
