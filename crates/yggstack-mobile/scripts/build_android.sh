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
    --platform "$ANDROID_MIN_SDK" \
    -o "$OUT_DIR/jni" \
    -- build --release -p yggstack-mobile --manifest-path "$MOBILE_CRATE/Cargo.toml"
done

# Generate Kotlin bindings.
# uniffi-bindgen --library mode loads the library at runtime to extract the
# interface metadata, so it needs a library the HOST can load: the Android
# .so artifacts (ELF, foreign arch/bionic) cannot be dlopened on macOS or
# Linux. Build the cdylib for the host and generate from it — the Kotlin
# output is target-independent.
echo "=== Generating Kotlin bindings ==="

cargo build --release -p yggstack-mobile

BINDGEN="$WORKSPACE_ROOT/target/release/uniffi-bindgen"
HOST_LIB=""
for cand in "$WORKSPACE_ROOT"/target/release/libyggstack_mobile.dylib "$WORKSPACE_ROOT"/target/release/libyggstack_mobile.so; do
  if [[ -f "$cand" ]]; then
    HOST_LIB="$cand"
    break
  fi
done

if [[ -z "$HOST_LIB" ]]; then
  echo "ERROR: host build of libyggstack_mobile not found under target/release; cannot generate bindings."
  exit 1
fi

"$BINDGEN" generate \
  --library "$HOST_LIB" \
  --language kotlin \
  --out-dir "$OUT_DIR/kotlin"

BINDINGS_OUT="$OUT_DIR/kotlin/uniffi/yggstack_mobile/yggstack_mobile.kt"
if [[ ! -f "$BINDINGS_OUT" ]]; then
  echo "ERROR: uniffi-bindgen exited 0 but $BINDINGS_OUT was not written."
  exit 1
fi
echo "Kotlin bindings written to $BINDINGS_OUT"

echo "=== Android build complete ==="
echo "Libraries: $OUT_DIR/jni/"
ls -lh "$OUT_DIR/jni/"*/libyggstack_mobile.so 2>/dev/null || true
