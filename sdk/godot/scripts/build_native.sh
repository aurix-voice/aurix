#!/usr/bin/env bash
# Build the aurix-client native library and stage it into the Godot addon's bin/ directory, then
# (unless --no-extension) build the GDExtension itself with SCons. Run from anywhere in the repo.
#
#   sdk/godot/scripts/build_native.sh                              # host, template_debug
#   sdk/godot/scripts/build_native.sh --release                    # template_release
#   sdk/godot/scripts/build_native.sh --target aarch64-unknown-linux-gnu
#   sdk/godot/scripts/build_native.sh --target aarch64-linux-android   # needs cargo-ndk + ANDROID_HOME/NDK
#   sdk/godot/scripts/build_native.sh --target aarch64-apple-ios       # macOS + Xcode; device + simulator slices
#   GODOT_CPP_PATH=~/godot-cpp sdk/godot/scripts/build_native.sh
#   ADDON_DIR=/path/to/MyGame/addons/aurix_voice sdk/godot/scripts/build_native.sh
#
# Mobile targets are staged by this script but are NOT built or exported in this repository's CI
# (no NDK / Xcode there); see docs/src/sdk/godot.md "Android and iOS".
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SDK="$ROOT/sdk/godot"
ADDON_DIR="${ADDON_DIR:-$SDK/addons/aurix_voice}"
PROFILE="debug"
TARGET=""
BUILD_EXTENSION=1
SCONS_JOBS="${SCONS_JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)}"

while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --release) PROFILE="release"; shift ;;
    --no-extension) BUILD_EXTENSION=0; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$TARGET" ]; then
  TARGET="$(rustc -vV | sed -n 's/^host: //p')"
fi

case "$TARGET" in
  x86_64-*-linux-*)   GODOT_PLATFORM="linux";   GODOT_ARCH="x86_64"; ARTIFACTS=("libaurix_client.so");
                      LINK_ARGS="-C link-arg=-Wl,-soname,libaurix_client.so" ;;
  aarch64-*-linux-*)  GODOT_PLATFORM="linux";   GODOT_ARCH="arm64";  ARTIFACTS=("libaurix_client.so");
                      LINK_ARGS="-C link-arg=-Wl,-soname,libaurix_client.so" ;;
  aarch64-linux-android)
                      GODOT_PLATFORM="android"; GODOT_ARCH="arm64";  ARTIFACTS=("libaurix_client.so"); NDK_ABI="arm64-v8a";
                      LINK_ARGS="-C link-arg=-Wl,-soname,libaurix_client.so" ;;
  armv7-linux-androideabi)
                      GODOT_PLATFORM="android"; GODOT_ARCH="arm32";  ARTIFACTS=("libaurix_client.so"); NDK_ABI="armeabi-v7a";
                      LINK_ARGS="-C link-arg=-Wl,-soname,libaurix_client.so" ;;
  x86_64-linux-android)
                      GODOT_PLATFORM="android"; GODOT_ARCH="x86_64"; ARTIFACTS=("libaurix_client.so"); NDK_ABI="x86_64";
                      LINK_ARGS="-C link-arg=-Wl,-soname,libaurix_client.so" ;;
  aarch64-apple-ios)
                      GODOT_PLATFORM="ios";     GODOT_ARCH="arm64";  ARTIFACTS=("libaurix_client.a");
                      LINK_ARGS="" ;;
  x86_64-apple-darwin|aarch64-apple-darwin)
                      GODOT_PLATFORM="macos";   GODOT_ARCH="universal"; ARTIFACTS=("libaurix_client.dylib");
                      LINK_ARGS="-C link-arg=-Wl,-install_name,@loader_path/libaurix_client.dylib" ;;
  x86_64-pc-windows-msvc)
                      GODOT_PLATFORM="windows"; GODOT_ARCH="x86_64"; ARTIFACTS=("aurix_client.dll" "aurix_client.dll.lib");
                      LINK_ARGS="" ;;
  *) echo "unsupported target for the Godot addon: $TARGET" >&2; exit 2 ;;
esac

OUT="$ROOT/target/$TARGET/$PROFILE"
DEST="$SDK/addons/aurix_voice/bin/$GODOT_PLATFORM.$GODOT_ARCH"
mkdir -p "$DEST"

# libopus 1.6 (DRED/OSCE) is compiled from the sources bundled with opusic-sys (needs cmake) and
# linked statically, so the shipped library has no dependency on a system libopus.
case "$GODOT_PLATFORM" in
  android)
    # cargo-ndk points cmake/cc at the NDK toolchain (API 21+) for the bundled libopus.
    command -v cargo-ndk >/dev/null || { echo "cargo-ndk is required (cargo install cargo-ndk)" >&2; exit 2; }
    [ -n "${ANDROID_NDK_HOME:-}${ANDROID_NDK_ROOT:-}${ANDROID_HOME:-}" ] || {
      echo "set ANDROID_NDK_HOME (or ANDROID_HOME with an ndk/ directory)" >&2; exit 2; }
    NDK_ARGS=(ndk -t "$NDK_ABI" -p "${ANDROID_API_LEVEL:-21}" build -p aurix-client --lib)
    [ "$PROFILE" = "release" ] && NDK_ARGS+=(--release)
    echo "building aurix-client for $TARGET ($PROFILE) via cargo-ndk"
    ( cd "$ROOT" && RUSTFLAGS="${RUSTFLAGS:-} $LINK_ARGS" cargo "${NDK_ARGS[@]}" )
    ;;
  ios)
    [ "$(uname -s)" = "Darwin" ] || { echo "iOS builds need macOS + Xcode (xcodebuild, cmake)" >&2; exit 2; }
    IOS_SIM_TARGET="aarch64-apple-ios-sim"
    for t in "$TARGET" "$IOS_SIM_TARGET"; do
      CARGO_ARGS=(build -p aurix-client --lib --target "$t")
      [ "$PROFILE" = "release" ] && CARGO_ARGS+=(--release)
      echo "building aurix-client for $t ($PROFILE)"
      ( cd "$ROOT" && IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-12.0}" cargo "${CARGO_ARGS[@]}" )
    done
    rm -rf "$DEST/libaurix_client.xcframework"
    xcodebuild -create-xcframework \
      -library "$OUT/libaurix_client.a" -headers "$ROOT/crates/aurix-client/include" \
      -library "$ROOT/target/$IOS_SIM_TARGET/$PROFILE/libaurix_client.a" -headers "$ROOT/crates/aurix-client/include" \
      -output "$DEST/libaurix_client.xcframework"
    echo "  $DEST/libaurix_client.xcframework"
    ;;
  *)
    CARGO_ARGS=(build -p aurix-client --lib --target "$TARGET")
    [ "$PROFILE" = "release" ] && CARGO_ARGS+=(--release)
    echo "building aurix-client for $TARGET ($PROFILE)"
    ( cd "$ROOT" && RUSTFLAGS="${RUSTFLAGS:-} $LINK_ARGS" cargo "${CARGO_ARGS[@]}" )
    ;;
esac

if [ "$GODOT_PLATFORM" != "ios" ]; then
  for f in "${ARTIFACTS[@]}"; do
    if [ ! -f "$OUT/$f" ]; then
      echo "expected artifact missing: $OUT/$f" >&2
      exit 1
    fi
    cp -f "$OUT/$f" "$DEST/$f"
    echo "  $DEST/$f"
  done
fi

if [ "$BUILD_EXTENSION" = "1" ]; then
  SCONS_TARGET="template_debug"
  [ "$PROFILE" = "release" ] && SCONS_TARGET="template_release"
  SCONS_ARGS=(platform="$GODOT_PLATFORM" target="$SCONS_TARGET" -j"$SCONS_JOBS")
  case "$GODOT_PLATFORM" in macos|ios) ;; *) SCONS_ARGS+=(arch="$GODOT_ARCH") ;; esac
  [ -n "${GODOT_CPP_PATH:-}" ] && SCONS_ARGS+=(godot_cpp="$GODOT_CPP_PATH")
  if [ "$GODOT_PLATFORM" = "ios" ]; then
    echo "building the GDExtension (device + simulator): scons ${SCONS_ARGS[*]}"
    ( cd "$SDK" && scons "${SCONS_ARGS[@]}" arch=arm64 ios_simulator=no && scons "${SCONS_ARGS[@]}" arch=arm64 ios_simulator=yes )
    XC="$DEST/libaurix_voice.ios.$SCONS_TARGET.xcframework"
    rm -rf "$XC"
    xcodebuild -create-xcframework \
      -library "$DEST/libaurix_voice.ios.$SCONS_TARGET.arm64.a" \
      -library "$DEST/libaurix_voice.ios.$SCONS_TARGET.arm64.simulator.a" \
      -output "$XC"
  else
    echo "building the GDExtension: scons ${SCONS_ARGS[*]}"
    ( cd "$SDK" && scons "${SCONS_ARGS[@]}" )
  fi
  ls -1 "$DEST"
fi

if [ "$ADDON_DIR" != "$SDK/addons/aurix_voice" ]; then
  mkdir -p "$ADDON_DIR/bin"
  cp -f "$SDK/addons/aurix_voice/aurix_voice.gdextension" "$ADDON_DIR/"
  cp -Rf "$DEST" "$ADDON_DIR/bin/"
  echo "staged into $ADDON_DIR"
fi
