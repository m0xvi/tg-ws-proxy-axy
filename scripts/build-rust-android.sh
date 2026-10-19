#!/bin/sh
set -eu

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

if ! command -v cargo >/dev/null 2>&1; then
  echo "Error: cargo not found in PATH. Install Rust from https://rustup.rs" >&2
  exit 1
fi

if ! command -v cargo-ndk >/dev/null 2>&1; then
  echo "Installing cargo-ndk..."
  cargo install cargo-ndk --locked
fi

rustup target add aarch64-linux-android armv7-linux-androideabi

mkdir -p "$ROOT_DIR/app/src/main/jniLibs/arm64-v8a"
mkdir -p "$ROOT_DIR/app/src/main/jniLibs/armeabi-v7a"

echo "[1/2] Building arm64-v8a (release, API 24)..."
cargo ndk -t arm64-v8a --platform 24 -o "$ROOT_DIR/app/src/main/jniLibs" build --release --locked

echo "[2/2] Building armeabi-v7a (release, API 21)..."
cargo ndk -t armeabi-v7a --platform 21 -o "$ROOT_DIR/app/src/main/jniLibs" build --release --locked

echo "=== Android Rust Core Build Success ==="
ls -lh "$ROOT_DIR/app/src/main/jniLibs/arm64-v8a/libtgwsproxy.so" \
       "$ROOT_DIR/app/src/main/jniLibs/armeabi-v7a/libtgwsproxy.so"
