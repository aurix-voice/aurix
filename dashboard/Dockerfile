# syntax=docker/dockerfile:1.6
# Build from the repository root (the SPA compiles the Node server SDK sources in-tree):
#   docker build -f dashboard/Dockerfile -t aurix-dashboard:local .
# ---------- build ----------
FROM --platform=$BUILDPLATFORM node:22-alpine AS builder
WORKDIR /build
COPY dashboard/package.json dashboard/package-lock.json dashboard/
RUN --mount=type=cache,target=/root/.npm \
    cd dashboard && npm ci --no-audit --no-fund
COPY sdk/server/node/src sdk/server/node/src
COPY dashboard dashboard
# Repository of the source this image is built from (shown in the sidebar; AGPL-3.0 §13).
ARG AURIX_SOURCE_URL=https://github.com/aurix-voice/aurix
RUN cd dashboard && AURIX_SOURCE_URL="$AURIX_SOURCE_URL" npm run build

# ---------- runtime ----------
FROM caddy:2.11-alpine AS runtime
# The stock binary carries cap_net_bind_service as a file capability; a container started with
# `cap_drop: [ALL]` then refuses to exec it. Strip it - port 8080 needs no capability.
RUN apk add --no-cache libcap \
    && setcap -r /usr/bin/caddy \
    && apk del libcap \
    && addgroup -S -g 10001 aurix \
    && adduser -S -u 10001 -G aurix -H -h /nonexistent -s /sbin/nologin aurix \
    && mkdir -p /data /config \
    && chown -R aurix:aurix /data /config
COPY dashboard/Caddyfile /etc/caddy/Caddyfile
COPY --from=builder /build/dashboard/dist /srv
RUN caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile

USER aurix:aurix
ENV AURIX_API_URL=http://aurix:8080 \
    DASHBOARD_ADDRESS=:8080 \
    DASHBOARD_TRUSTED_PROXIES=private_ranges \
    XDG_DATA_HOME=/data \
    XDG_CONFIG_HOME=/config
# /data keeps ACME certificates when DASHBOARD_ADDRESS is a hostname (automatic HTTPS); binding
# 80/443 as uid 10001 then needs `--sysctl net.ipv4.ip_unprivileged_port_start=0` or a port mapping.
VOLUME ["/data"]
EXPOSE 8080/tcp
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD ["sh", "-c", "wget -qO- \"${DASHBOARD_HEALTHCHECK_URL:-http://127.0.0.1:${DASHBOARD_ADDRESS##*:}/-/healthz}\" >/dev/null || exit 1"]
CMD ["caddy", "run", "--config", "/etc/caddy/Caddyfile", "--adapter", "caddyfile"]
