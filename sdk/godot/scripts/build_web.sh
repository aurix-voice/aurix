#!/usr/bin/env bash
# Export the Godot project for the Web (AurixWebVoiceClient over the Aurix Web SDK) and place the
# SDK browser bundle next to index.html. Needs the Godot editor binary (GODOT, default `godot`),
# the matching Web export templates installed, and node/npm for the Web SDK bundle.
#
#   sdk/godot/scripts/build_web.sh                 # release export → sdk/godot/build/web/
#   sdk/godot/scripts/build_web.sh --debug         # debug template (console logging)
#   OUT_DIR=/srv/lobby sdk/godot/scripts/build_web.sh
#
# The output is a static site; serve it over HTTP(S). Threads are disabled in the preset so no
# COOP/COEP headers are required, and the SDK is loaded from `aurix-web-sdk.js` beside index.html
# (override with `AurixWebVoiceClient.sdk_url`).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SDK="$ROOT/sdk/godot"
WEB_SDK="$ROOT/sdk/web"
OUT_DIR="${OUT_DIR:-$SDK/build/web}"
GODOT="${GODOT:-godot}"
MODE="--export-release"

while [ $# -gt 0 ]; do
  case "$1" in
    --debug) MODE="--export-debug"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ ! -f "$WEB_SDK/dist/aurix-web-sdk.js" ]; then
  echo "==> building the Web SDK bundle"
  (cd "$WEB_SDK" && npm ci --no-audit --no-fund && npm run build)
fi

mkdir -p "$OUT_DIR"
echo "==> importing project"
"$GODOT" --headless --path "$SDK" --import >/dev/null 2>&1 || true
echo "==> exporting Web preset ($MODE) → $OUT_DIR"
"$GODOT" --headless --path "$SDK" "$MODE" Web "$OUT_DIR/index.html"
cp "$WEB_SDK/dist/aurix-web-sdk.js" "$OUT_DIR/aurix-web-sdk.js"
test -s "$OUT_DIR/index.html" -a -s "$OUT_DIR/index.wasm" -a -s "$OUT_DIR/index.pck"
echo "==> done: $(du -sh "$OUT_DIR" | cut -f1) in $OUT_DIR"
