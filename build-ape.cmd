@echo off
rem Build the fat APE from Windows, by driving the build in WSL.
rem
rem The APE cannot be built from Windows itself: the linker is a shell script,
rem and Windows answers "%1 is not a valid Win32 application (os error 193)".
rem Linux or WSL only. wsl.exe inherits the current directory and translates it,
rem so there is nothing to configure here beyond pointing it at demo/.
rem
rem The toolchain override is needed because rust-toolchain.toml pins stable for
rem native builds, while an APE needs the nightly that cosmo-build's custom
rem target specs and -Zbuild-std require.
rem
rem     build-ape.cmd                 -> demo\target\cosmo\release\demo.com
rem
rem Anything after the command is passed through to cargo.

setlocal
set TOOLCHAIN=nightly-2026-08-23

where wsl.exe >nul 2>&1
if errorlevel 1 (
   echo build-ape: no wsl.exe on PATH. The APE needs Linux or WSL to link.
   exit /b 1
)

pushd "%~dp0demo" || exit /b 1
wsl.exe -e bash -lc "cargo +%TOOLCHAIN% build -F ape --release %*"
set RC=%errorlevel%
popd

if not "%RC%"=="0" (
   echo build-ape: failed. If cargo could not find the toolchain, install it in
   echo            WSL: rustup toolchain install %TOOLCHAIN%
   echo            and: rustup target add x86_64-unknown-linux-musl --toolchain %TOOLCHAIN%
   exit /b %RC%
)

echo.
echo build-ape: demo\target\cosmo\release\demo.com
echo            runs as-is on Windows; on Linux under WSL run it through
echo            ~/.cache/cargo-cosmo/cosmocc/bin/ape-x86_64.elf, because binfmt
echo            hands a bare APE to Windows there.
exit /b 0
