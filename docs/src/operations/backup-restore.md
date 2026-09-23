# Backup and restore

Aurix keeps every durable byte in two places — **PostgreSQL** and, when recording is on, the
**recording store** (local directory or S3) — plus a handful of secrets that live in the node's
configuration, not in the database. Redis is a cache that the fleet rebuilds. This chapter is the
procedure: what to copy, how often, how to prove a copy is restorable, and how to bring a fleet
back on it.

## What to back up

| what | where | how |
|---|---|---|
| Apps, API keys (hashed), users, channels, bans, blocks, moderation events, chat, transcripts, recording catalogue, webhooks (with their signing secrets), admins, audit log, usage analytics, tombstones, migration table | PostgreSQL | `tools/backup/run.sh backup` (pg_dump) — or base backup + WAL archiving (PITR) for anything beyond a small deployment |
| Recording files (Ogg/Opus tracks, mixdowns) | `recording.storage_path` volume or the S3 bucket | `--recordings` in the same backup, or volume snapshots / S3 versioning + replication |
| `auth.jwt_secret` (or the signing key pair), `recording.encryption_key`, `media.cascade_secret`, `turn.auth_secret`, `auth.oidc.client_secret`, TLS certificates and keys (`server.tls_*`, `media.quic_cert_path`), S3 credentials | node configuration / secret store | your secret manager — these are **not** in the dump, and a restored database is useless for encrypted recordings without the recording key |

What is deliberately **not** backed up:

* **Redis** — events, one-time token claims, session→node mapping and session mirrors, node
  liveness beacons, rate-limit counters, global mutes. After a loss clients reconnect and the
  state rebuilds; see [High availability](high-availability.md#redis).
* **Runtime rows** in PostgreSQL that are stale by definition in any backup: open `sessions`,
  `channel_memberships`, `media_nodes`, `media_node_links`, `live_streams`. They are dumped with
  everything else (the dump is a consistent snapshot) and cleaned up by the nodes: a node closes
  its own rows on start (`recover_node_state`), the fleet reaper closes those of node ids that
  never come back (`cluster.node_lost_after_secs`). No manual cleanup.

The dump is sensitive: it holds webhook signing secrets in clear, password and API-key hashes,
chat and transcripts. Encrypt it at rest (age, GPG, SSE-KMS) and keep its retention in line with
the privacy commitments below.

## `tools/backup/run.sh`

One script, three commands, no dependencies beyond `bash`, `jq`, `tar` and the PostgreSQL
client tools (`psql`, `pg_dump`, `pg_restore` of the server's major version or newer). Where the
host's tools are too old, run them inside the database container:
`AURIX_BACKUP_PG_TOOLS="docker compose exec -T db"`.

```sh
export AURIX_DATABASE_URL=postgres://aurix:…@db:5432/aurix      # the node's database.url

# 1. backup → /backups/<UTC timestamp>-nightly/{db.dump, recordings.tar.gz, manifest.json}
tools/backup/run.sh backup --out /backups --recordings /var/lib/aurix/recordings --label nightly

# 2. restore drill: restore into a throw-away database, compare, drop (exit 0 = restorable)
tools/backup/run.sh verify /backups/20260923T020000Z-nightly

# 3. real restore into an empty database (nodes stopped), recordings back onto the volume
tools/backup/run.sh restore /backups/20260923T020000Z-nightly \
  --database-url postgres://aurix:…@db:5432/aurix --recordings /var/lib/aurix/recordings
```

**`backup`** opens one `REPEATABLE READ` transaction, exports its snapshot and hands it to
`pg_dump --snapshot` (custom format, no owners or privileges, so it restores under any role) while
counting the rows of every table *in the same snapshot*. Nodes may keep writing during the
backup; the manifest still describes exactly what the dump contains. `manifest.json`:

```json
{
  "format": 1,
  "created_at": "2026-09-23T02:00:04Z",
  "label": "nightly",
  "source": "postgres://aurix:***@db:5432/aurix",
  "postgres_version": "16.15",
  "schema": { "latest": 20240101000024, "migrations": [ { "version": …, "checksum": "…", "success": true }, … ] },
  "tables": { "apps": 12, "users": 7793, "user_tombstones": 90, "recordings": 195, … },
  "dump": { "file": "db.dump", "format": "pg_dump custom", "sha256": "…", "bytes": 3750359 },
  "recordings": { "file": "recordings.tar.gz", "sha256": "…", "bytes": …, "files": 195, "source": "/var/lib/aurix/recordings" }
}
```

Passwords never reach the manifest or the log. The script ends by naming the secrets the dump
does not contain.

**`verify`** is the restore drill. It checks both files against their SHA-256, creates
`aurix_restore_drill_<time>_<pid>` on the scratch server (`--scratch-url`,
`AURIX_BACKUP_SCRATCH_URL`, or the `postgres` maintenance database of the backup's own server —
the role needs `CREATEDB`), restores the dump into it in a single transaction, compares every
table's row count and every `_sqlx_migrations` row (version, description, checksum, success)
with the manifest, and drops the database — also on failure. Any difference is printed and exits
non-zero. Schedule it: a backup nobody has restored is a hope, not a backup. CI runs it against
the database the live E2E suites fill while both nodes are still writing.

**`restore`** refuses a target that already has tables (`--force` drops and recreates schema
`public`) and a non-empty recordings directory (`--force` unpacks over it), restores in a single
transaction with `--exit-on-error`, runs the same comparison as `verify`, and unpacks the
recordings. It does **not** touch the nodes, Redis, or the schema version.

## Restoring a fleet

1. **Stop every node** (`docker compose stop aurix`, scale the `StatefulSet` to 0). A node still
   running would write into the old database and, worse, run migrations against it.
2. Put the **secrets** in place first — the same `auth.jwt_secret` / signing key if issued
   tokens should keep working, the same `recording.encryption_key` (a node refuses to process a
   recording whose `encryption_key_id` is not its own; it never produces garbage audio), the
   same `media.cascade_secret` on every node.
3. **Restore** with the script into an empty database (a fresh `CREATE DATABASE aurix` on the
   new server, or `--force` on the old one), recordings onto the volume or back into the bucket.
4. **`aurix doctor`** from a node's working directory with the node's environment
   ([CLI](../backend/cli.md#preflight-on-the-node-host-aurix-doctor)). Its `migrations` check
   tells you what the restored schema is against the binary you are about to start:
   * *current* — start.
   * *pending* (the backup is older than the binary) — `database.run_migrations = true` applies
     them at start-up, or run `aurix-server --migrate-only` first; take a copy of the restored
     database before either if the gap is more than a patch release.
   * *versions from a newer build* (the backup is newer than the binary) — do **not** start this
     binary: with `run_migrations = true` it fails on the unknown version, with `false` it would
     serve a schema it does not understand. Deploy the version that wrote the backup, or newer.
   * *checksum mismatch / failed* — the database was not produced by an Aurix migrator, or a
     migration was edited; investigate before starting anything.
5. **Start one node**, wait for `/ready`, join a channel, check `GET /v1/nodes` shows only the
   live node (stale rows disappear as the reaper runs), then start the rest. Redis needs
   nothing: it can be empty, new, or the old one — clients resume or reconnect either way.

There is no rollback of migrations. Going back to an older binary means restoring a backup taken
before the upgrade — one more reason to back up before upgrading, which `aurix doctor` reminds
you of whenever migrations are pending.

## Point-in-time recovery

`pg_dump` is a snapshot at one instant: the recovery point is the last backup. For anything
beyond a small deployment run a daily base backup plus continuous WAL archiving (pgBackRest,
WAL-G, or the managed provider's PITR) and use `tools/backup/run.sh` on top for the manifest and
the drill, or run the drill against a PITR restore and compare with `verify`'s output by hand.
Recordings and the database are backed up independently, so after a PITR the catalogue may
reference files that were written after the recovery point (harmless — a download 404s) or lack
rows for files that exist (orphans that the retention sweep never sees; delete them by
`created_at`). S3 versioning with a lifecycle rule covers the file side.

## Privacy

Backups outlive erasure: a dump taken before `DELETE /v1/users/{id}` still contains the user's
rows, and a recording archive still holds their tracks. Set backup retention to match what you
promise players ([User erasure](../features/moderation.md#user-erasure-delete-v1usersuser_id)
and the [retention sweep](../features/moderation.md#retention-sweep)). What does survive a
restore correctly: tombstones (`user_tombstones`) are ordinary rows, so a restored
database keeps rejecting tokens issued before an erasure that happened before the backup; an
erasure that happened *after* the backup is lost with everything else after the recovery point
and has to be repeated.

## Schedule

| deployment | database | recordings | drill |
|---|---|---|---|
| Compose, one node | nightly `backup --recordings …`, keep 14 | in the same backup | weekly `verify` of the newest backup |
| several nodes, own PostgreSQL | pgBackRest / WAL-G base + WAL, plus nightly `backup` for the manifest | volume snapshot or S3 versioning | nightly `verify`, quarterly full-fleet restore into staging |
| managed PostgreSQL + S3 | provider PITR, plus nightly `backup` | S3 versioning + cross-region replication | nightly `verify` on a scratch instance |
