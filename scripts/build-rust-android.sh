#!/bin/sh
set -eu

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

if ! command -v cargo >/dev/null 2>&1; then
  echo "Error: cargo not found in PATH. Install Rust from https://rustup.rs" >&2
  exit 1
fi

# Определяем путь к Android NDK (поддержка GitHub Actions ubuntu-latest, Linux и macOS)
NDK_DIR="${ANDROID_NDK_HOME:-${ANDROID_NDK_LATEST_HOME:-${NDK_HOME:-}}}"
if [ -z "$NDK_DIR" ] || [ ! -d "$NDK_DIR" ]; then
  SDK_DIR="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
  if [ -n "$SDK_DIR" ] && [ -d "$SDK_DIR/ndk" ]; then
    NDK_DIR="$(ls -1d "$SDK_DIR"/ndk/* 2>/dev/null | sort -V | tail -n 1 || true)"
  fi
fi

if [ -z "$NDK_DIR" ] || [ ! -d "$NDK_DIR" ]; then
  echo "Error: Android NDK not found. Set ANDROID_NDK_HOME or ANDROID_HOME." >&2
  exit 1
fi

echo "Using Android NDK: $NDK_DIR"

HOST_TAG=""
for candidate in linux-x86_64 darwin-x86_64 darwin-arm64; do
  if [ -d "$NDK_DIR/toolchains/llvm/prebuilt/$candidate/bin" ]; then
    HOST_TAG="$candidate"
    break
  fi
done

if [ -z "$HOST_TAG" ]; then
  echo "Error: NDK LLVM prebuilt toolchain not found under $NDK_DIR/toolchains/llvm/prebuilt" >&2
  exit 1
fi

NDK_BIN="$NDK_DIR/toolchains/llvm/prebuilt/$HOST_TAG/bin"

rustup target add aarch64-linux-android armv7-linux-androideabi

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android24-clang"
export CC_aarch64_linux_android="$NDK_BIN/aarch64-linux-android24-clang"
export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"

export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$NDK_BIN/armv7a-linux-androideabi21-clang"
export CC_armv7_linux_androideabi="$NDK_BIN/armv7a-linux-androideabi21-clang"
export AR_armv7_linux_androideabi="$NDK_BIN/llvm-ar"

mkdir -p "$ROOT_DIR/app/src/main/jniLibs/arm64-v8a"
mkdir -p "$ROOT_DIR/app/src/main/jniLibs/armeabi-v7a"

echo "[1/2] Building arm64-v8a (release, API 24)..."
cargo build --release --target aarch64-linux-android
cp -f "target/aarch64-linux-android/release/libtgwsproxy.so" "$ROOT_DIR/app/src/main/jniLibs/arm64-v8a/libtgwsproxy.so"

echo "[2/2] Building armeabi-v7a (release, API 21)..."
cargo build --release --target armv7-linux-androideabi
cp -f "target/armv7-linux-androideabi/release/libtgwsproxy.so" "$ROOT_DIR/app/src/main/jniLibs/armeabi-v7a/libtgwsproxy.so"

echo "=== Android Rust Core Build Success ==="
ls -lh "$ROOT_DIR/app/src/main/jniLibs/arm64-v8a/libtgwsproxy.so" \
       "$ROOT_DIR/app/src/main/jniLibs/armeabi-v7a/libtgwsproxy.so"
