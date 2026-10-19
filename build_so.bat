@echo off
setlocal enabledelayedexpansion

echo === Building Rust proxy library for Android (cdylib) ===
echo === Architectures: arm64-v8a (SDK 24), armeabi-v7a (SDK 21) ===
echo.

set "ROOT_DIR=%~dp0"

set "SDK_PATH=%ANDROID_SDK_ROOT%"
if not defined SDK_PATH set "SDK_PATH=%ANDROID_HOME%"
if defined SDK_PATH goto :SdkResolved
if not exist "%ROOT_DIR%local.properties" goto :SdkFromDefault
set "SDK_PATH="
for /f "usebackq delims=" %%A in (`powershell -NoProfile -Command "$bs=[string][char]92; $co=[string][char]58; $l=(Get-Content '%ROOT_DIR%local.properties' | Select-String '^sdk\.dir=' | Select-Object -First 1).ToString(); $l.Substring(8).Replace($bs+$co,$co).Replace($bs+$bs,$bs)"`) do set "SDK_PATH=%%A"
if defined SDK_PATH goto :SdkResolved

:SdkFromDefault
set "SDK_PATH=%LOCALAPPDATA%\Android\Sdk"

:SdkResolved
if exist "%SDK_PATH%" goto :SdkOk
echo Error: Android SDK not found (resolved: "%SDK_PATH%").
echo   Set ANDROID_SDK_ROOT, or add sdk.dir to local.properties
exit /b 1

:SdkOk
echo Using SDK: %SDK_PATH%

set "NDK_ROOT=%SDK_PATH%\ndk"
if not exist "%NDK_ROOT%" (
    echo Error: no NDK in "%NDK_ROOT%".
    echo   Install it: Android Studio > SDK Manager > SDK Tools > NDK
    exit /b 1
)
set "NDK_VER="
for /f "delims=" %%D in ('dir /b /ad /o-n "%NDK_ROOT%"') do (
    if not defined NDK_VER set "NDK_VER=%%D"
)
if not defined NDK_VER (
    echo Error: no NDK version found in %NDK_ROOT%
    exit /b 1
)
echo Using NDK: %NDK_VER%

set "ANDROID_NDK_HOME=%NDK_ROOT%\%NDK_VER%"
set "NDK_HOME=%ANDROID_NDK_HOME%"
set "NDK_BIN=%ANDROID_NDK_HOME%\toolchains\llvm\prebuilt\windows-x86_64\bin"

if not exist "%NDK_BIN%\aarch64-linux-android24-clang.cmd" (
    echo Error: NDK clang wrapper not found at %NDK_BIN%
    exit /b 1
)

where cargo >nul 2>nul
if %errorlevel% neq 0 (
    echo Error: cargo not found in PATH. Install Rust from https://rustup.rs
    exit /b 1
)

set "HOST_TC=stable-x86_64-pc-windows-msvc"
where link.exe >nul 2>nul
if %errorlevel% neq 0 (
    echo Note: MSVC link.exe not found - using the windows-gnu host toolchain.
    echo       If linking still fails, install LLVM-MinGW or MSYS2 and add it to PATH.
    set "HOST_TC=stable-x86_64-pc-windows-gnu"
)
set "RUSTUP_TOOLCHAIN=%HOST_TC%"

echo Using host toolchain: %RUSTUP_TOOLCHAIN%
rustup target add --toolchain %HOST_TC% aarch64-linux-android armv7-linux-androideabi >nul 2>nul

set "CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=%NDK_BIN%\aarch64-linux-android24-clang.cmd"
set "CC_aarch64_linux_android=%NDK_BIN%\aarch64-linux-android24-clang.cmd"
set "AR_aarch64_linux_android=%NDK_BIN%\llvm-ar.exe"

set "CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER=%NDK_BIN%\armv7a-linux-androideabi21-clang.cmd"
set "CC_armv7_linux_androideabi=%NDK_BIN%\armv7a-linux-androideabi21-clang.cmd"
set "AR_armv7_linux_androideabi=%NDK_BIN%\llvm-ar.exe"

set "RUSTFLAGS=--remap-path-prefix=%USERPROFILE%\.cargo=CARGO_HOME"

if not exist "%ROOT_DIR%app\src\main\jniLibs\arm64-v8a"   mkdir "%ROOT_DIR%app\src\main\jniLibs\arm64-v8a"
if not exist "%ROOT_DIR%app\src\main\jniLibs\armeabi-v7a" mkdir "%ROOT_DIR%app\src\main\jniLibs\armeabi-v7a"

cd /d "%ROOT_DIR%"

echo.
echo [1/2] arm64-v8a (release, API 24)...
cargo build --release --target aarch64-linux-android
if %errorlevel% neq 0 (
    echo BUILD FAILED for arm64-v8a!
    exit /b 1
)
copy /y "target\aarch64-linux-android\release\libtgwsproxy.so" "app\src\main\jniLibs\arm64-v8a\libtgwsproxy.so" >nul
if %errorlevel% neq 0 (
    echo COPY FAILED for arm64-v8a!
    exit /b 1
)

echo.
echo [2/2] armeabi-v7a (release, API 21)...
cargo build --release --target armv7-linux-androideabi
if %errorlevel% neq 0 (
    echo BUILD FAILED for armeabi-v7a!
    exit /b 1
)
copy /y "target\armv7-linux-androideabi\release\libtgwsproxy.so" "app\src\main\jniLibs\armeabi-v7a\libtgwsproxy.so" >nul
if %errorlevel% neq 0 (
    echo COPY FAILED for armeabi-v7a!
    exit /b 1
)

for %%F in ("%ROOT_DIR%app\src\main\jniLibs\arm64-v8a\libtgwsproxy.so") do (
    echo arm64-v8a:   OK [%%~zF bytes]
)
for %%F in ("%ROOT_DIR%app\src\main\jniLibs\armeabi-v7a\libtgwsproxy.so") do (
    echo armeabi-v7a: OK [%%~zF bytes]
)

echo.
echo === BUILD SUCCESS ===
echo   arm64-v8a:   app\src\main\jniLibs\arm64-v8a\libtgwsproxy.so
echo   armeabi-v7a: app\src\main\jniLibs\armeabi-v7a\libtgwsproxy.so
echo.
echo Next: gradlew.bat :app:assembleArm64Debug
echo.
exit /b 0
