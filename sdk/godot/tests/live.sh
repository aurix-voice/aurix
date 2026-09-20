#!/usr/bin/env bash
# Run tests/live.gd against a running Aurix node.
#
#   AURIX_E2E_API=http://127.0.0.1:8080 AURIX_E2E_WS=ws://127.0.0.1:8081 \   (the /ws path is appended)
#   AURIX_E2E_API_KEY=... [GODOT=/path/to/godot] sdk/godot/tests/live.sh
#
# Creates a channel, mints two player tokens with the API key and hands them to the headless
# Godot process through AURIX_GODOT_* variables. Requires the extension to be built first
# (scripts/build_native.sh) and the project imported once (`godot --headless --import`).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
api="${AURIX_E2E_API:?AURIX_E2E_API required}"
ws="${AURIX_E2E_WS:?AURIX_E2E_WS required}"
key="${AURIX_E2E_API_KEY:?AURIX_E2E_API_KEY required}"
godot="${GODOT:-godot}"

json_field() { python3 -c 'import sys, json; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }

channel=$(curl -fsS -X POST "$api/v1/channels" -H "x-api-key: $key" -H 'content-type: application/json' \
  -d "{\"name\":\"godot-live-$(date +%s)\",\"config\":{}}" | json_field id)

mint() {
  curl -fsS -X POST "$api/v1/tokens" -H "x-api-key: $key" -H 'content-type: application/json' \
    -d "{\"external_id\":\"godot-$1-$$\",\"display_name\":\"$2\",\"channels\":[{\"channel_id\":\"$channel\",\"join\":true,\"speak\":true,\"receive\":true,\"moderate\":false}]}" \
    | json_field token
}

export AURIX_GODOT_WS="${ws%/}/ws" AURIX_GODOT_CHANNEL="$channel"
AURIX_GODOT_TOKEN_A=$(mint alice Alice)
AURIX_GODOT_TOKEN_B=$(mint bob Bob)
export AURIX_GODOT_TOKEN_A AURIX_GODOT_TOKEN_B

cd "$here/.."
if [ ! -f .godot/extension_list.cfg ]; then
  timeout 120 "$godot" --headless --path . --import >/dev/null 2>&1 || true
fi
exec timeout 120 "$godot" --headless --path . -s tests/live.gd
