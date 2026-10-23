# Aurix Rooms demo (backend + static frontend). Build from the repository root:
#   docker build -f examples/rooms/deploy/Dockerfile -t aurix-rooms:dev .
FROM node:22-alpine AS build
WORKDIR /src
COPY sdk/web ./sdk/web
COPY sdk/server/node ./sdk/server/node
COPY examples/rooms/server ./examples/rooms/server
COPY examples/rooms/web ./examples/rooms/web
RUN cd sdk/web && npm ci --no-audit --no-fund && npm run build \
 && cd /src/sdk/server/node && npm ci --no-audit --no-fund && npm run build \
 && cd /src/examples/rooms/server && npm ci --no-audit --no-fund --omit=dev \
 && cd /src/examples/rooms/web && npm ci --no-audit --no-fund && npm run build

FROM node:22-alpine
WORKDIR /app
COPY --from=build /src/sdk/server/node /app/sdk/server/node
COPY --from=build /src/examples/rooms/server /app/examples/rooms/server
COPY --from=build /src/examples/rooms/web/dist /app/examples/rooms/web/dist
ENV PORT=3000 HOST=0.0.0.0 ROOMS_STATIC_DIR=/app/examples/rooms/web/dist ROOMS_STATE_FILE=/data/rooms.json
VOLUME ["/data"]
EXPOSE 3000
WORKDIR /app/examples/rooms/server
CMD ["node", "server.mjs"]
