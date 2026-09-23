#!/usr/bin/env bash
# Aurix backup / restore / restore-drill tool.
#
# Everything durable lives in PostgreSQL and, when recording is on, the recording store; Redis
# is rebuilt by the nodes. This script wraps pg_dump / pg_restore so that a backup is
# self-describing (manifest with schema version, per-table row counts taken in the dump's own
# snapshot, SHA-256 of every file) and so that restoring — or proving that a backup *can* be
# restored — is one command that fails loudly when anything does not add up.
#
#   backup  --out DIR [--database-url URL] [--recordings PATH] [--label NAME]
#           pg_dump (custom format) + optional tar of a local recording directory + manifest.json
#           into DIR/<timestamp>[-label]/ ; prints the backup directory.
#   verify  BACKUP_DIR [--scratch-url URL]
#           restore drill: the dump is restored into a throw-away database created on the
#           scratch server (default: the `postgres` maintenance database of the backup's source),
#           row counts and migration rows are compared with the manifest, the database is dropped.
#           Exit 0 = this backup restores completely. Run it from cron; a backup nobody has
#           restored is a hope, not a backup.
#   restore BACKUP_DIR --database-url URL [--force] [--recordings PATH]
#           restore into URL, which must be an empty database (no tables in `public`) unless
#           --force drops and recreates the schema; the same comparison as `verify` runs
#           afterwards. Recordings are unpacked into PATH (empty unless --force). Nodes must be
#           stopped; start one afterwards and run `aurix doctor` first.
#
# Environment:
#   AURIX_DATABASE_URL / AURIX__DATABASE__URL   default for --database-url (backup) — the same
#                                                URL the node uses
#   AURIX_BACKUP_SCRATCH_URL                     default for --scratch-url (verify)
#   AURIX_BACKUP_PG_TOOLS                        command prefix for psql/pg_dump/pg_restore,
#                                                e.g. "docker compose exec -T db" when the
#                                                client tools on this host are older than the
#                                                server (they must be >= the server's major)
#
# Credentials never reach the manifest or the log: URLs are printed with the password masked.
# Requires bash 4+, jq, tar, sha256sum (or shasum), and PostgreSQL client tools reachable via
# AURIX_BACKUP_PG_TOOLS or PATH.
set -euo pipefail

log()  { printf '\033[1;36m[backup]\033[0m %s\n' "$*" >&2; }
fail() { printf '\033[1;31m[backup] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }
usage() { sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }

PG_TOOLS=()
if [ -n "${AURIX_BACKUP_PG_TOOLS:-}" ]; then
  # shellcheck disable=SC2206
  PG_TOOLS=(${AURIX_BACKUP_PG_TOOLS})
fi
pg() { "${PG_TOOLS[@]}" "$@"; }

need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is required"; }

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# postgres://user:pass@host/db → postgres://user:***@host/db
mask_url() { printf '%s' "$1" | sed -E 's#(://[^:/@]+):[^@]*@#\1:***@#'; }

# postgres://user:pass@host:port/db?x=y → postgres://user:pass@host:port/NEWDB?x=y
with_db() {
  local url=$1 db=$2 base query=""
  case $url in *\?*) query="?${url#*\?}"; url=${url%%\?*} ;; esac
  base=${url%/*}
  printf '%s/%s%s' "$base" "$db" "$query"
}

db_name() {
  local url=$1
  url=${url%%\?*}
  printf '%s' "${url##*/}"
}

psql_q() { pg psql "$1" -qAtX -v ON_ERROR_STOP=1 -c "$2"; }

# Row counts of every ordinary table in `public`, as "table<TAB>count" lines, run by the given
# psql invocation (a string of SQL is returned so it can run inside the snapshot transaction).
COUNT_SQL="SELECT string_agg(format('SELECT %L AS t, count(*) AS n FROM %I', c.relname, c.relname), ' UNION ALL ' ORDER BY c.relname)
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = 'public' AND c.relkind = 'r'"
MIGRATIONS_SQL="SELECT json_agg(json_build_object('version', version, 'description', description, 'checksum', encode(checksum, 'hex'), 'success', success) ORDER BY version)
                FROM _sqlx_migrations"

# Counts + migration rows of a database as JSON: {"tables": {...}, "migrations": [...]}.
# When a snapshot id is given the queries run inside a REPEATABLE READ transaction on that
# snapshot, i.e. they see exactly what pg_dump --snapshot dumps.
describe_db() {
  local url=$1 snapshot=${2:-} prelude=""
  if [ -n "$snapshot" ]; then
    prelude="BEGIN ISOLATION LEVEL REPEATABLE READ; SET TRANSACTION SNAPSHOT '$snapshot';"
  fi
  local count_query
  count_query=$(psql_q "$url" "$COUNT_SQL")
  [ -n "$count_query" ] || fail "no tables in $(mask_url "$url")"
  pg psql "$url" -qAtX -v ON_ERROR_STOP=1 -F $'\t' <<SQL | build_description
$prelude
SELECT 'T', t, n FROM ($count_query) counts ORDER BY t;
SELECT 'M', coalesce(($MIGRATIONS_SQL), '[]'), '';
SQL
}

build_description() {
  jq -R -s '
    split("\n") | map(select(length > 0) | split("\t"))
    | { tables: (map(select(.[0] == "T") | {key: .[1], value: (.[2] | tonumber)}) | from_entries),
        migrations: (map(select(.[0] == "M"))[0][1] | fromjson) }'
}

# Compares the description of a restored database with the manifest; prints every difference.
compare_with_manifest() {
  local manifest=$1 actual=$2
  local diff
  diff=$(jq -rn --slurpfile m "$manifest" --slurpfile a <(printf '%s' "$actual") '
    ($m[0].tables) as $mt | ($a[0].tables) as $at |
    ($m[0].schema.migrations // []) as $mm | ($a[0].migrations // []) as $am |
    [ ( ($mt | keys) + ($at | keys) | unique | .[] |
        select(($mt[.] // -1) != ($at[.] // -1)) |
        "table \(.): manifest \($mt[.] // "missing") rows, restored \($at[.] // "missing")" ),
      ( ($mm + $am | map(.version) | unique | .[]) as $v |
        ($mm | map(select(.version == $v)) | .[0]) as $x |
        ($am | map(select(.version == $v)) | .[0]) as $y |
        select($x != $y) |
        "migration \($v): manifest \($x | tojson), restored \($y | tojson)" )
    ] | .[]') || fail "could not compare the restored database with the manifest"
  if [ -n "$diff" ]; then
    printf '%s\n' "$diff" >&2
    return 1
  fi
}

# ── backup ───────────────────────────────────────────────────────────────────────────────────

cmd_backup() {
  local out="" url="${AURIX_DATABASE_URL:-${AURIX__DATABASE__URL:-}}" recordings="" label=""
  while [ $# -gt 0 ]; do
    case $1 in
      --out) out=$2; shift 2 ;;
      --database-url) url=$2; shift 2 ;;
      --recordings) recordings=$2; shift 2 ;;
      --label) label=$2; shift 2 ;;
      *) usage ;;
    esac
  done
  [ -n "$out" ] || fail "--out DIR is required"
  [ -n "$url" ] || fail "--database-url URL (or AURIX_DATABASE_URL) is required"
  if [ -n "$recordings" ]; then [ -d "$recordings" ] || fail "recordings directory $recordings does not exist"; fi
  need jq; need tar

  local stamp dir
  stamp=$(date -u +%Y%m%dT%H%M%SZ)
  dir="$out/$stamp${label:+-$label}"
  mkdir -p "$dir"
  log "backing up $(mask_url "$url") → $dir"

  # One REPEATABLE READ transaction exports a snapshot; pg_dump dumps that snapshot while the
  # transaction is still open, and the row counts come from the same transaction — so the
  # manifest describes exactly the data in the dump even with nodes writing concurrently.
  local snapshot
  coproc SNAP { pg psql "$url" -qAtX -v ON_ERROR_STOP=1 2>&1; }
  printf 'BEGIN ISOLATION LEVEL REPEATABLE READ;\nSELECT pg_export_snapshot();\n' >&"${SNAP[1]}"
  IFS= read -r snapshot <&"${SNAP[0]}" || fail "could not open a snapshot on $(mask_url "$url")"
  case $snapshot in
    [0-9A-F]*-[0-9A-F]*) ;;
    *) fail "snapshot export failed: $snapshot" ;;
  esac
  local description
  description=$(describe_db "$url" "$snapshot")
  if ! pg pg_dump "$url" --format=custom --no-owner --no-privileges --snapshot="$snapshot" >"$dir/db.dump"; then
    printf 'ROLLBACK;\n\\q\n' >&"${SNAP[1]}" 2>/dev/null || true
    fail "pg_dump failed"
  fi
  printf 'COMMIT;\n\\q\n' >&"${SNAP[1]}"
  wait "$SNAP_PID" 2>/dev/null || true

  local pg_version
  pg_version=$(psql_q "$url" "SHOW server_version")

  local rec_json=null
  if [ -n "$recordings" ]; then
    local files
    files=$(find "$recordings" -type f | wc -l | tr -d ' ')
    log "archiving $files recording file(s) from $recordings"
    tar -C "$recordings" -czf "$dir/recordings.tar.gz" .
    rec_json=$(jq -n --arg f recordings.tar.gz --arg s "$(sha256 "$dir/recordings.tar.gz")" \
      --argjson b "$(stat -c %s "$dir/recordings.tar.gz" 2>/dev/null || stat -f %z "$dir/recordings.tar.gz")" \
      --argjson n "$files" --arg src "$recordings" \
      '{file: $f, sha256: $s, bytes: $b, files: $n, source: $src}')
  fi

  jq -n \
    --arg created "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg lbl "$label" \
    --arg source "$(mask_url "$url")" \
    --arg pgv "$pg_version" \
    --arg dump_sha "$(sha256 "$dir/db.dump")" \
    --argjson dump_bytes "$(stat -c %s "$dir/db.dump" 2>/dev/null || stat -f %z "$dir/db.dump")" \
    --argjson desc "$description" \
    --argjson rec "$rec_json" \
    '{
      format: 1,
      created_at: $created,
      "label": (if $lbl == "" then null else $lbl end),
      source: $source,
      postgres_version: $pgv,
      schema: {
        latest: ($desc.migrations | map(select(.success)) | map(.version) | max),
        migrations: $desc.migrations
      },
      tables: $desc.tables,
      dump: {file: "db.dump", format: "pg_dump custom", sha256: $dump_sha, bytes: $dump_bytes},
      recordings: $rec
    }' >"$dir/manifest.json"

  local latest tables rows
  latest=$(jq -r .schema.latest "$dir/manifest.json")
  tables=$(jq -r '.tables | length' "$dir/manifest.json")
  rows=$(jq -r '.tables | add' "$dir/manifest.json")
  log "done: schema $latest, $tables tables / $rows rows, $(jq -r .dump.bytes "$dir/manifest.json") bytes"
  log "remember: the database does not contain auth.jwt_secret / signing keys, recording.encryption_key, TLS keys or media.cascade_secret — back those up separately"
  printf '%s\n' "$dir"
}

# ── verify / restore ─────────────────────────────────────────────────────────────────────────

check_backup_dir() {
  local dir=$1
  [ -f "$dir/manifest.json" ] || fail "$dir/manifest.json not found"
  [ "$(jq -r .format "$dir/manifest.json")" = "1" ] || fail "unsupported manifest format"
  local file want have
  file=$(jq -r .dump.file "$dir/manifest.json")
  [ -f "$dir/$file" ] || fail "$dir/$file not found"
  want=$(jq -r .dump.sha256 "$dir/manifest.json")
  have=$(sha256 "$dir/$file")
  [ "$want" = "$have" ] || fail "$file is corrupt: sha256 $have, manifest says $want"
  if [ "$(jq -r .recordings "$dir/manifest.json")" != null ]; then
    file=$(jq -r .recordings.file "$dir/manifest.json")
    [ -f "$dir/$file" ] || fail "$dir/$file not found"
    want=$(jq -r .recordings.sha256 "$dir/manifest.json")
    have=$(sha256 "$dir/$file")
    [ "$want" = "$have" ] || fail "$file is corrupt: sha256 $have, manifest says $want"
  fi
}

restore_into() {
  local dir=$1 url=$2
  log "restoring $(jq -r .dump.file "$dir/manifest.json") into $(mask_url "$url")"
  pg pg_restore --dbname="$url" --no-owner --no-privileges --single-transaction --exit-on-error \
    <"$dir/$(jq -r .dump.file "$dir/manifest.json")" || fail "pg_restore failed"
  local actual
  actual=$(describe_db "$url")
  if ! compare_with_manifest "$dir/manifest.json" "$actual"; then
    fail "restored database differs from the manifest"
  fi
  log "restored database matches the manifest: $(jq -r '.tables | length' "$dir/manifest.json") tables, $(jq -r '.tables | add' "$dir/manifest.json") rows, schema $(jq -r .schema.latest "$dir/manifest.json")"
}

cmd_verify() {
  local dir="" scratch="${AURIX_BACKUP_SCRATCH_URL:-}"
  while [ $# -gt 0 ]; do
    case $1 in
      --scratch-url) scratch=$2; shift 2 ;;
      -*) usage ;;
      *) dir=$1; shift ;;
    esac
  done
  [ -n "$dir" ] || usage
  need jq
  check_backup_dir "$dir"
  if [ -z "$scratch" ]; then
    local src="${AURIX_DATABASE_URL:-${AURIX__DATABASE__URL:-}}"
    [ -n "$src" ] || fail "--scratch-url URL (or AURIX_BACKUP_SCRATCH_URL / AURIX_DATABASE_URL) is required"
    scratch=$(with_db "$src" postgres)
  fi
  local name="aurix_restore_drill_$(date -u +%Y%m%d%H%M%S)_$$" target
  target=$(with_db "$scratch" "$name")
  log "restore drill: creating $name on $(mask_url "$scratch")"
  psql_q "$scratch" "CREATE DATABASE \"$name\"" >/dev/null || fail "cannot create the drill database (CREATEDB privilege on the scratch server?)"
  trap 'psql_q "$scratch" "DROP DATABASE IF EXISTS \"$name\"" >/dev/null 2>&1 || true' EXIT
  restore_into "$dir" "$target"
  psql_q "$scratch" "DROP DATABASE \"$name\"" >/dev/null
  trap - EXIT
  log "OK: $dir restores completely (drill database dropped)"
}

cmd_restore() {
  local dir="" url="" force=0 recordings=""
  while [ $# -gt 0 ]; do
    case $1 in
      --database-url) url=$2; shift 2 ;;
      --recordings) recordings=$2; shift 2 ;;
      --force) force=1; shift ;;
      -*) usage ;;
      *) dir=$1; shift ;;
    esac
  done
  [ -n "$dir" ] && [ -n "$url" ] || usage
  need jq
  check_backup_dir "$dir"
  if [ -n "$recordings" ] && [ "$(jq -r .recordings "$dir/manifest.json")" = null ]; then
    fail "this backup has no recordings archive"
  fi

  local existing
  existing=$(psql_q "$url" "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'public' AND c.relkind = 'r'")
  if [ "$existing" != 0 ]; then
    if [ "$force" = 1 ]; then
      log "--force: dropping schema public of $(mask_url "$url") ($existing tables)"
      psql_q "$url" "SET client_min_messages = warning; DROP SCHEMA public CASCADE; CREATE SCHEMA public" >/dev/null
    else
      fail "$(mask_url "$url") already has $existing tables; restore into an empty database or pass --force to drop them"
    fi
  fi
  if [ -n "$recordings" ]; then
    mkdir -p "$recordings"
    if [ -n "$(ls -A "$recordings")" ] && [ "$force" != 1 ]; then
      fail "$recordings is not empty; pass --force to unpack over it"
    fi
  fi

  restore_into "$dir" "$url"
  if [ -n "$recordings" ]; then
    log "unpacking recordings into $recordings"
    tar -C "$recordings" -xzf "$dir/$(jq -r .recordings.file "$dir/manifest.json")"
  fi
  log "next: with the node's configuration pointing at this database run \`aurix doctor\` — it"
  log "      reports the restored schema against the binary (pending migrations if the binary is"
  log "      newer, versions from a newer build if it is older) — then start one node."
}

case ${1:-} in
  backup) shift; cmd_backup "$@" ;;
  verify) shift; cmd_verify "$@" ;;
  restore) shift; cmd_restore "$@" ;;
  *) usage ;;
esac
