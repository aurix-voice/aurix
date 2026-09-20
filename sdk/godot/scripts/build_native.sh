#!/usr/bin/env bash
# Build the aurix-client native library and stage it into the Godot addon's bin/ directory, then
# (unless --no-extension) build the GDExtension itself with SCons. Run from anywhere in the repo.
#
#   sdk/godot/scripts/build_native.sh                              # host, template_debug
#   sdk/godot/scripts/build_native.sh --release                    # template_release
#   sdk/godot/scripts/build_native.sh --target aarch64-unknown-linux-gnu
#   GODOT_CPP_PATH=~/godot-cpp sdk/godot/scripts/build_native.sh
#   ADDON_DIR=/path/to/MyGame/addons/aurix_voice sdk/godot/scripts/build_native.sh
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
  x86_64-apple-darwin|aarch64-apple-darwin)
                      GODOT_PLATFORM="macos";   GODOT_ARCH="universal"; ARTIFACTS=("libaurix_client.dylib");
                      LINK_ARGS="-C link-arg=-Wl,-install_name,@loader_path/libaurix_client.dylib" ;;
  x86_64-pc-windows-msvc)
                      GODOT_PLATFORM="windows"; GODOT_ARCH="x86_64"; ARTIFACTS=("aurix_client.dll" "aurix_client.dll.lib");
                      LINK_ARGS="" ;;
  *) echo "unsupported target for the Godot addon: $TARGET" >&2; exit 2 ;;
esac

CARGO_ARGS=(build -p aurix-client --lib --target "$TARGET")
[ "$PROFILE" = "release" ] && CARGO_ARGS+=(--release)

echo "building aurix-client for $TARGET ($PROFILE)"
# libopus 1.6 (DRED/OSCE) is compiled from the sources bundled with opusic-sys (needs cmake) and
# linked statically, so the shipped library has no dependency on a system libopus.
( cd "$ROOT" && RUSTFLAGS="${RUSTFLAGS:-} $LINK_ARGS" cargo "${CARGO_ARGS[@]}" )

OUT="$ROOT/target/$TARGET/$PROFILE"
DEST="$SDK/addons/aurix_voice/bin/$GODOT_PLATFORM.$GODOT_ARCH"
mkdir -p "$DEST"

for f in "${ARTIFACTS[@]}"; do
  if [ ! -f "$OUT/$f" ]; then
    echo "expected artifact missing: $OUT/$f" >&2
    exit 1
  fi
  cp -f "$OUT/$f" "$DEST/$f"
  echo "  $DEST/$f"
done

if [ "$BUILD_EXTENSION" = "1" ]; then
  SCONS_TARGET="template_debug"
  [ "$PROFILE" = "release" ] && SCONS_TARGET="template_release"
  SCONS_ARGS=(platform="$GODOT_PLATFORM" target="$SCONS_TARGET" -j"$SCONS_JOBS")
  [ "$GODOT_PLATFORM" != "macos" ] && SCONS_ARGS+=(arch="$GODOT_ARCH")
  [ -n "${GODOT_CPP_PATH:-}" ] && SCONS_ARGS+=(godot_cpp="$GODOT_CPP_PATH")
  echo "building the GDExtension: scons ${SCONS_ARGS[*]}"
  ( cd "$SDK" && scons "${SCONS_ARGS[@]}" )
  ls -1 "$DEST"
fi

if [ "$ADDON_DIR" != "$SDK/addons/aurix_voice" ]; then
  mkdir -p "$ADDON_DIR/bin"
  cp -f "$SDK/addons/aurix_voice/aurix_voice.gdextension" "$ADDON_DIR/"
  cp -Rf "$DEST" "$ADDON_DIR/bin/"
  echo "staged into $ADDON_DIR"
fi
