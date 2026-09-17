FROM rust:1.94-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --bin aurix-server --bin aurix -j 3

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/aurix-server /app/aurix-server
COPY --from=builder /build/target/release/aurix /app/aurix
COPY --from=builder /build/configs /etc/aurix/configs
COPY --from=builder /build/migrations /etc/aurix/migrations
ENV AURIX__DATABASE__URL=postgres://aurix:aurix@db:5432/aurix
ENV AURIX__REDIS__URL=redis://redis:6379
COPY configs /app/configs
EXPOSE 8080 8081 10000/udp 3478/udp 4040
ENTRYPOINT ["/app/aurix-server"]
CMD ["--config", "/etc/aurix/configs/default"]