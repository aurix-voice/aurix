#!/usr/bin/env bash
# Fail on any Godot error in a Web export log except the ones that only say "the desktop
# GDExtension binary is not built here". The Web preset never ships that binary (the browser
# lobby runs on AurixWebVoiceClient), so an export from a checkout without
# `scripts/build_native.sh` legitimately logs: the missing .so, the .gdextension that therefore
# fails to load, "no suitable library" for the wasm32 feature set, and parse errors in the
# desktop-only demo/main.gd whose classes come from that extension.
#
#   sdk/godot/scripts/build_web.sh 2>&1 | tee export.log
#   sdk/godot/scripts/check_web_export_log.sh export.log
set -euo pipefail

LOG="${1:?usage: check_web_export_log.sh <export.log>}"

ALLOWED='GDExtension dynamic library not found: .*/addons/aurix_voice/bin/'
ALLOWED+='|Condition "!FileAccess::exists\(path\)" is true\. Returning: ERR_FILE_NOT_FOUND'
ALLOWED+='|Failed loading resource: res://addons/aurix_voice/aurix_voice\.gdextension'
ALLOWED+='|Error loading extension: res://addons/aurix_voice/aurix_voice\.gdextension'
ALLOWED+='|No suitable library found for GDExtension: res://addons/aurix_voice/aurix_voice\.gdextension'
ALLOWED+='|Parse Error: .*"(AurixVoiceClient|AurixParticipantPlayer)"'
ALLOWED+='|Parse Error: Cannot infer the type of "(r|player)" variable'
ALLOWED+='|at: GDScript::reload \(res://demo/main\.gd:'

if grep -E "ERROR" "$LOG" | grep -Ev "$ALLOWED" | grep -q .; then
  echo "unexpected errors in $LOG:" >&2
  grep -E "ERROR" "$LOG" | grep -Ev "$ALLOWED" >&2
  exit 1
fi
if grep -Eq "$ALLOWED" "$LOG"; then
  echo "note: desktop GDExtension binary not built in this checkout; its load errors were ignored (Web export does not ship it)"
fi
echo "export log clean"
