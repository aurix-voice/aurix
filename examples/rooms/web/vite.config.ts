import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// In development Vite stands in for the reverse proxy: `/api` → rooms backend, `/ws` + `/v1` →
// Aurix node. In production Caddy does the same (see ../deploy/Caddyfile).
const roomsUrl = process.env.ROOMS_URL ?? "http://127.0.0.1:3000";
const aurixApi = process.env.AURIX_API_URL ?? "http://127.0.0.1:8080";
const aurixWs = process.env.AURIX_WS_URL ?? "ws://127.0.0.1:8081";

export default defineConfig({
  plugins: [react()],
  build: { target: "es2022", sourcemap: false },
  server: {
    host: true,
    proxy: {
      "/api": roomsUrl,
      "/healthz": roomsUrl,
      "/v1": aurixApi,
      "/ws": { target: aurixWs, ws: true },
    },
  },
});
