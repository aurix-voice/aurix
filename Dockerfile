# syntax=docker/dockerfile:1.6
# ---------- build ----------
FROM rust:1.90-bookworm AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake clang \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY api ./api
COPY migrations ./migrations
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin aurix-server --bin aurix \
    && mkdir -p /out && cp target/release/aurix-server target/release/aurix /out/

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 curl tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 aurix \
    && useradd --system --uid 10001 --gid aurix --home /var/lib/aurix --shell /usr/sbin/nologin aurix \
    && mkdir -p /var/lib/aurix/recordings /etc/aurix \
    && chown -R aurix:aurix /var/lib/aurix
COPY --from=builder /out/aurix-server /out/aurix /usr/local/bin/
COPY --chown=aurix:aurix configs /etc/aurix/configs
COPY --chown=aurix:aurix migrations /etc/aurix/migrations

WORKDIR /etc/aurix
USER aurix:aurix
ENV AURIX__SERVER__ENVIRONMENT=production \
    AURIX__RECORDING__STORAGE_PATH=/var/lib/aurix/recordings \
    AURIX__TRACING__LOG_FORMAT=json
VOLUME ["/var/lib/aurix/recordings"]
# REST, WebSocket, native media (UDP), TURN (UDP+TCP), metrics. TURN relay ports are
# configured via turn.min_port/max_port and must be published separately (see docker-compose).
EXPOSE 8080/tcp 8081/tcp 10000/udp 3478/udp 3478/tcp 4040/tcp
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${AURIX__SERVER__API_PORT:-8080}/ready" || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/aurix-server"]
