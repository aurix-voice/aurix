# Aurix operator dashboard

Standalone operator UI for an Aurix fleet: Vite + React + TypeScript SPA served by Caddy from its
own image (`ghcr.io/aurix-voice/aurix-dashboard`). The node never serves it; the SPA talks to the
node's `/v1` and `/admin` routes same-origin through Caddy's reverse proxy with an administrator
JWT, and can do nothing the API does not allow.

Full documentation: [docs/src/operations/dashboard.md](../docs/src/operations/dashboard.md)
(deployment with Compose / Helm / Traefik, sign-in, roles, compatibility, limits).

```bash
npm ci
AURIX_API_URL=http://127.0.0.1:8080 npm run dev                # http://127.0.0.1:5173
npm run check && npm run lint && npm test && npm run build     # tsc, eslint, vitest, dist/
AURIX_E2E_ADMIN_EMAIL=… AURIX_E2E_ADMIN_PASSWORD=… npm run e2e  # Playwright against a live node
docker build -f dashboard/Dockerfile -t aurix-dashboard:local ..  # from the repository root
```

Layout: `src/api` (typed client over `sdk/server/node/src`, TanStack Query hooks, SSE), `src/auth`
(password / OIDC / first-administrator setup, permission gating), `src/pages/*` (one directory per
section), `src/ui` (design system), `src/i18n` (RU / EN), `e2e/` (Playwright, `DASHBOARD_URL` to
target a deployed container), `Caddyfile` + `Dockerfile` (runtime image).
