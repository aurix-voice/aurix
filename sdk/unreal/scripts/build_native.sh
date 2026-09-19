#!/usr/bin/env bash
# Build the aurix-client native library and stage it (plus headers) into the Unreal plugin's
# ThirdParty module. Run from anywhere inside the Aurix repository.
#
#   sdk/unreal/scripts/build_native.sh                # host platform, release
#   sdk/unreal/scripts/build_native.sh --target x86_64-unknown-linux-gnu
#   sdk/unreal/scripts/build_native.sh --target aarch64-apple-darwin
#   PLUGIN_DIR=/path/to/MyGame/Plugins/AurixVoice sdk/unreal/scripts/build_native.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
PLUGIN_DIR="${PLUGIN_DIR:-$ROOT/sdk/unreal/AurixVoice}"
THIRD_PARTY="$PLUGIN_DIR/Source/ThirdParty/AurixClientLibrary"
PROFILE="release"
TARGET=""

while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --debug) PROFILE="debug"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$TARGET" ]; then
  TARGET="$(rustc -vV | sed -n 's/^host: //p')"
fi

case "$TARGET" in
  *-linux-*)
    UE_PLATFORM="Linux"
    ARTIFACTS=("libaurix_client.so")
    # Rust sets no SONAME; without it the packaged game would DT_NEED the absolute build path.
    LINK_ARGS="-C link-arg=-Wl,-soname,libaurix_client.so"
    ;;
  *-apple-darwin)
    UE_PLATFORM="Mac"
    ARTIFACTS=("libaurix_client.dylib")
    LINK_ARGS="-C link-arg=-Wl,-install_name,@rpath/libaurix_client.dylib"
    ;;
  *-pc-windows-msvc)
    UE_PLATFORM="Win64"
    ARTIFACTS=("aurix_client.dll" "aurix_client.dll.lib")
    LINK_ARGS=""
    ;;
  *)
    echo "unsupported target for the Unreal plugin: $TARGET" >&2
    exit 2
    ;;
esac

CARGO_ARGS=(build -p aurix-client --lib --target "$TARGET")
[ "$PROFILE" = "release" ] && CARGO_ARGS+=(--release)

echo "building aurix-client for $TARGET ($PROFILE)"
# libopus is compiled from the bundled source and linked statically (needs cmake) so the shipped
# library has no dependency on a system libopus. audiopus_sys does not re-run its build script
# when these variables change, so its output is cleaned first.
CLEAN_ARGS=(clean -p audiopus_sys --target "$TARGET")
[ "$PROFILE" = "release" ] && CLEAN_ARGS+=(--release)
( cd "$ROOT" && cargo "${CLEAN_ARGS[@]}" >/dev/null 2>&1 || true )
( cd "$ROOT" && LIBOPUS_STATIC=1 LIBOPUS_NO_PKG=1 RUSTFLAGS="${RUSTFLAGS:-} $LINK_ARGS" cargo "${CARGO_ARGS[@]}" )

OUT="$ROOT/target/$TARGET/$PROFILE"
DEST="$THIRD_PARTY/lib/$UE_PLATFORM"
mkdir -p "$DEST" "$THIRD_PARTY/include"

for f in "${ARTIFACTS[@]}"; do
  if [ ! -f "$OUT/$f" ]; then
    echo "expected artifact missing: $OUT/$f" >&2
    exit 1
  fi
  cp -f "$OUT/$f" "$DEST/$f"
  echo "  $DEST/$f"
done

cp -f "$ROOT/crates/aurix-client/include/aurix_client.h" "$THIRD_PARTY/include/"
cp -f "$ROOT/crates/aurix-client/include/aurix_client.hpp" "$THIRD_PARTY/include/"
echo "  $THIRD_PARTY/include/aurix_client.h"
echo "  $THIRD_PARTY/include/aurix_client.hpp"
echo "done: $(cd "$THIRD_PARTY" && pwd)"
