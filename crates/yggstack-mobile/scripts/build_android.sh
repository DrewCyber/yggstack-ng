#!/usr/bin/env bash
# Build the yggstack Android AAR for all ABI targets using cargo-ndk + uniffi-bindgen.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
MOBILE_CRATE="$WORKSPACE_ROOT/crates/yggstack-mobile"
OUT_DIR="$WORKSPACE_ROOT/android-build"

ANDROID_MIN_SDK=21

ABIS=(
  "arm64-v8a:aarch64-linux-android"
  "armeabi-v7a:armv7-linux-androideabi"
  "x86:i686-linux-android"
  "x86_64:x86_64-linux-android"
)

# Validate prerequisites
if ! command -v cargo-ndk &>/dev/null; then
  echo "Installing cargo-ndk..."
  cargo install cargo-ndk
fi

if [[ -z "${ANDROID_NDK_HOME:-}" && -z "${NDK_HOME:-}" ]]; then
  echo "ERROR: Set ANDROID_NDK_HOME or NDK_HOME to your NDK installation."
  exit 1
fi

cd "$WORKSPACE_ROOT"

mkdir -p "$OUT_DIR/jni"

echo "=== Building yggstack-mobile for Android ==="

for entry in "${ABIS[@]}"; do
  abi="${entry%%:*}"
  target="${entry##*:}"
  echo "--- Building $abi ($target) ---"

  rustup target add "$target" 2>/dev/null || true

  cargo ndk \
    --target "$target" \
    --android-platform "$ANDROID_MIN_SDK" \
    --manifest-path "$MOBILE_CRATE/Cargo.toml" \
    -o "$OUT_DIR/jni" \
    -- build --release -p yggstack-mobile
done

# Generate Kotlin bindings
echo "=== Generating Kotlin bindings ==="
BINDGEN="$WORKSPACE_ROOT/target/release/uniffi-bindgen"

cargo build --release -p yggstack-mobile --bin uniffi-bindgen 2>/dev/null || true

if [[ -f "$BINDGEN" ]]; then
  LIB_PATH="$(ls "$OUT_DIR/jni/arm64-v8a/libyggstack_mobile.so" 2>/dev/null | head -1)"
  if [[ -n "$LIB_PATH" ]]; then
    "$BINDGEN" generate \
      --library "$LIB_PATH" \
      --language kotlin \
      --out-dir "$OUT_DIR/kotlin"
    echo "Kotlin bindings written to $OUT_DIR/kotlin"
  fi
fi

echo "=== Android build complete ==="
echo "Libraries: $OUT_DIR/jni/"
ls -lh "$OUT_DIR/jni/"*/libyggstack_mobile.so 2>/dev/null || true
