import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// The dashboard talks to a node through the same origin: Caddy proxies these prefixes in the
// image (see Caddyfile); in development Vite does the same against AURIX_API_URL.
const API_PREFIXES = ["/v1", "/admin", "/health", "/ready", "/openapi.json"];
const apiUrl = process.env.AURIX_API_URL ?? "http://127.0.0.1:8080";
const pkg = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as { version: string };

export default defineConfig({
  plugins: [react(), tailwindcss()],
  define: {
    __DASHBOARD_VERSION__: JSON.stringify(`v${pkg.version}`),
  },
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
      // Generated REST client + hand-written fetch transport of the Node server SDK; browser-safe
      // (fetch, URL, TextDecoder). The SDK's webhook helpers (node:crypto) are not imported.
      "@aurix/server-sdk-src": fileURLToPath(new URL("../sdk/server/node/src", import.meta.url)),
    },
  },
  server: {
    port: 5173,
    proxy: Object.fromEntries(
      API_PREFIXES.map((p) => [p, { target: apiUrl, changeOrigin: true, ws: false }]),
    ),
  },
  build: {
    sourcemap: false,
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [
            { name: "react", test: /node_modules[/\\](react|react-dom|scheduler)[/\\]/ },
            { name: "charts", test: /node_modules[/\\](recharts|d3-[a-z]+|victory-vendor)[/\\]/ },
          ],
        },
      },
    },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
