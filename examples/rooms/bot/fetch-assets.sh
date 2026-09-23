#!/usr/bin/env bash
# Downloads the demo playlist (public-domain / CC BY recordings from Wikimedia Commons), converts
# everything to 48 kHz 16-bit WAV (stereo for music, mono for speech and ambience) and writes
# `playlist.json` for `aurix-rooms-bot --assets <dir>`. Needs curl and ffmpeg. Nothing here is
# committed to the repository; rerun to refresh.
#
#   ./fetch-assets.sh [target-dir]        (default ./assets)
#   MAX_SECONDS=150 ./fetch-assets.sh     cap per track (long readings are trimmed)
set -euo pipefail

dir=${1:-$(dirname "$0")/assets}
max=${MAX_SECONDS:-150}
ua='aurix-rooms-demo/1.0 (+https://github.com/aurix-voice/aurix)'
base='https://upload.wikimedia.org/wikipedia/commons'

command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v ffmpeg >/dev/null || { echo "ffmpeg is required" >&2; exit 1; }
mkdir -p "$dir"

# id|kind|channels|title|artist|license|commons path
tracks=(
  'carefree|music|2|Carefree|Kevin MacLeod|CC BY 3.0|5/58/Kevin_MacLeod_-_Carefree.ogg'
  'o-captain|speech|1|O Captain! My Captain!|Walt Whitman, read by Annie Coleman (LibriVox)|Public domain|f/f8/LibriVox_-_O_Captain%21_My_Captain%21_-_Annie_Coleman.ogg'
  'monkeys|music|2|Monkeys Spinning Monkeys|Kevin MacLeod|CC BY 3.0|3/37/Kevin_MacLeod_-_Monkeys_Spinning_Monkeys.ogg'
  'ambience|ambience|1|Field recording|nille (Wikimedia Commons)|Public domain|0/0a/20090610_0_ambience.ogg'
  'ice-giants|music|2|The Ice Giants|Kevin MacLeod|CC BY 4.0|a/a6/Kevin_MacLeod_-_The_Ice_Giants.ogg'
  'the-raven|speech|1|The Raven|Edgar Allan Poe, read by Chris Goringe (LibriVox)|Public domain|b/b5/LibriVox_-_The_Raven_-_Chris_Goringe.ogg'
  'rhapsody|music|1|Rhapsody in Blue (1924 recording)|George Gershwin, Paul Whiteman Orchestra|Public domain|b/bb/Rhapsody_in_Blue_-_Original_1924_Recording.opus'
  'fluffing|music|2|Fluffing a Duck|Kevin MacLeod|CC BY 3.0|c/c3/Kevin_MacLeod_-_Fluffing_a_Duck.ogg'
)

json='{"tracks":['
first=1
for spec in "${tracks[@]}"; do
  IFS='|' read -r id kind channels title artist license path <<<"$spec"
  src="$dir/.src-$id.${path##*.}"
  wav="$dir/$id.wav"
  if [ ! -s "$wav" ]; then
    echo "→ $title"
    curl -fsSL --retry 3 -A "$ua" -o "$src" "$base/$path"
    # loudnorm keeps tracks at a comparable level so the room does not jump between them.
    ffmpeg -nostdin -loglevel error -y -i "$src" -t "$max" -vn \
      -af "loudnorm=I=-18:LRA=9:TP=-1.5" -ar 48000 -ac "$channels" -c:a pcm_s16le "$wav"
    rm -f "$src"
  fi
  [ $first = 1 ] || json+=','
  first=0
  json+=$(printf '{"title":"%s","kind":"%s","artist":"%s","license":"%s","file":"%s"}' \
    "$title" "$kind" "$artist" "$license" "$id.wav")
done
json+=']}'
printf '%s\n' "$json" | python3 -m json.tool >"$dir/playlist.json" 2>/dev/null || printf '%s\n' "$json" >"$dir/playlist.json"
echo "playlist written to $dir/playlist.json ($(du -sh "$dir" | cut -f1))"
