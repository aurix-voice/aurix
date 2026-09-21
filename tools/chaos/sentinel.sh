#!/bin/sh
# Entrypoint of the Sentinel containers: Sentinel rewrites its configuration file at runtime,
# so each instance gets a fresh writable copy generated here instead of a read-only bind mount.
set -eu
port="${SENTINEL_PORT:-26379}"
conf="/data/sentinel-${port}.conf"
mkdir -p /data
cat >"$conf" <<EOF
port ${port}
dir /data
sentinel monitor aurix 127.0.0.1 6380 2
sentinel down-after-milliseconds aurix 2000
sentinel failover-timeout aurix 10000
sentinel parallel-syncs aurix 1
sentinel resolve-hostnames no
EOF
exec redis-sentinel "$conf"
