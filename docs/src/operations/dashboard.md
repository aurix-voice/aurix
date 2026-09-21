# Operator dashboard

`dashboard/` is the operator web UI: a static single-page application (Vite + React + TypeScript)
that talks to a node's REST API and event stream with an administrator's JWT. It is a **separate
service** — the node never serves it, and the SPA has no backend of its own: everything it shows
or changes goes through the same `/v1` and `/admin` routes that the CLI and the server SDKs use,
with the same permission checks, audit entries and rate limits. Nothing is possible from the
dashboard that is not possible from the API.

What it covers: fleet overview (CCU, participant minutes, MOS, node health, regions, versions,
quality alerts); nodes (load, drain / undrain, addresses, relay-only hubs); applications, API
keys, per-app limits and webhooks (subscriptions, deliveries, test, resync, secret rotation);
live channels and sessions with moderation actions, transport and per-session statistics;
moderation (reports, safety incidents with evidence, bans and blocks, users, stored chat with
search); recordings (mixdown, transcripts, downloads); analytics (time series, CSV export, worst
sessions); administrators, roles, SSO and the audit log; and the node's effective configuration
(read-only, secrets masked). English and Russian; light, dark and system themes.

## How the browser reaches the node

The SPA calls the API **same-origin**: the container runs [Caddy](https://caddyserver.com/), which
serves the compiled files and reverse-proxies `/v1/*`, `/admin/*`, `/health`, `/ready` and
`/openapi.json` to the node (`AURIX_API_URL`). Consequences:

* No CORS entry is needed on the node for the dashboard origin, and the admin JWT never crosses
  an origin boundary.
* `GET /v1/events` (SSE) is proxied unbuffered (`flush_interval -1`), so live updates arrive as
  the node emits them. Anything you put *in front* of the dashboard must also stream that path
  and tolerate long-lived responses: with Traefik do not attach a `buffering` middleware and do
  not set a response `writeTimeout` on the entry point; with a cloud load balancer keep the
  idle timeout above the node's SSE keep-alive interval (`webhooks.sse_keepalive_secs`). The SPA reconnects when a stream drops.
* The node sees the dashboard container as the client. Add its network (the Compose network,
  the pod CIDR) to `server.trusted_proxies` so audit entries, rate limiting and bans record the
  administrator's real address from `X-Forwarded-For`.
* Everything else Caddy answers itself: hashed `/assets/*` with a one-year immutable cache,
  `index.html` with `no-cache`, any other path falls back to the SPA shell, a missing asset is a
  real `404`. `/-/healthz` is the container's own liveness endpoint. Security headers
  (`Content-Security-Policy`, `X-Frame-Options: DENY`, `nosniff`, referrer and permissions
  policies) are set on every response; the Caddy admin API is off.

The image ([`dashboard/Dockerfile`](https://github.com/aurix-voice/aurix/blob/main/dashboard/Dockerfile),
[`dashboard/Caddyfile`](https://github.com/aurix-voice/aurix/blob/main/dashboard/Caddyfile)) is
published as `ghcr.io/aurix-voice/aurix-dashboard:<version>` next to `aurix-server`, multi-arch,
signed and attested like the server image ([Releases](releases.md)). It runs as uid 10001 with
no capabilities (the file capability on the stock Caddy binary is stripped) and a read-only root
file system; `/data` (ACME state), `/config` and `/tmp` are the only writable paths.

| Variable | Default | Meaning |
|---|---|---|
| `AURIX_API_URL` | `http://aurix:8080` | node (or node load balancer) the API prefixes are proxied to |
| `DASHBOARD_ADDRESS` | `:8080` | Caddy site address. A hostname (`dash.example.com`) turns on automatic HTTPS — then `/data` must persist and the container must be reachable on 80/443 |
| `DASHBOARD_TRUSTED_PROXIES` | `private_ranges` | proxies whose `X-Forwarded-*` headers are trusted (Caddy syntax: `private_ranges` or CIDRs) |
| `DASHBOARD_HEALTHCHECK_URL` | `http://127.0.0.1:<port>/-/healthz` | overrides the image `HEALTHCHECK` target |

## Docker Compose

`docker-compose.yml` ships a `dashboard` service built from the repository (`aurix-dashboard:local`)
and hardened like the node: read-only root, `tmpfs` for `/tmp` and `/config`, `cap_drop: [ALL]`,
`no-new-privileges`, CPU/memory limits, log rotation. It binds to `127.0.0.1:8090` by default
(`AURIX_DASHBOARD_PORT`); put TLS in front or set `AURIX_DASHBOARD_ADDRESS` to a public hostname
for automatic HTTPS. With Traefik already on the host, drop the port mapping and route by
labels instead:

```yaml
  dashboard:
    # …as shipped, minus `ports:`
    labels:
      traefik.enable: "true"
      traefik.http.routers.aurix-dashboard.rule: Host(`dash.example.com`)
      traefik.http.routers.aurix-dashboard.entrypoints: websecure
      traefik.http.routers.aurix-dashboard.tls.certresolver: letsencrypt
      traefik.http.services.aurix-dashboard.loadbalancer.server.port: "8080"
      # SSE: Traefik must not buffer /v1/events — do not attach a `buffering` middleware.
```

and add the Compose network to the node's `AURIX_TRUSTED_PROXIES` (`.env`).

## Kubernetes (Helm)

The chart renders the dashboard when `dashboard.enabled: true`: a `Deployment` (non-root, no
service-account token, read-only root, probes on `/-/healthz`), a `ClusterIP` `Service`, an
optional `PodDisruptionBudget`, and either a standard `Ingress` (`dashboard.ingress`, any
controller — set `className: traefik` or leave the cluster default) or a Traefik-native
`IngressRoute` (`dashboard.ingressRoute`, needs the Traefik CRDs). `dashboard.apiUrl` defaults to
the release's API `Service`, so nothing else has to be exposed; when the chart's
`NetworkPolicy` is on, the dashboard pods are allowed to reach the node API.

```yaml
dashboard:
  enabled: true
  image:
    repository: ghcr.io/aurix-voice/aurix-dashboard   # tag defaults to the chart appVersion
  ingressRoute:
    enabled: true
    host: dash.example.com
    entryPoints: [websecure]
    certResolver: letsencrypt      # or tls.secretName
    middlewares: [ops-allowlist]   # optional Traefik Middleware objects in the namespace
config:
  trustedProxies: ["10.0.0.0/8"]   # pod CIDR: the dashboard pod is the node's client
```

`helm install … --set dashboard.enabled=true` and `NOTES.txt` prints the URL, replica count,
API target and route type. Any ingress controller works through the standard `Ingress`; the
project's own examples and defaults are Caddy and Traefik.

## Signing in

The first administrator is created through `/setup` with the node's `auth.admin_bootstrap_token`
(the page closes once an administrator exists); afterwards `/login` offers password sign-in
(`auth.admin_password_login`) and/or SSO (`[auth.oidc]`, see [Administrator accounts and
SSO](admin-sso.md)). Two OIDC shapes work: point `auth.oidc.redirect_url` at the node and set
`auth.oidc.frontend_redirect` to `https://dash.example.com/auth/callback` (the JWT returns in the
URL fragment, never in a query string or a log line), or point the IdP's redirect URI at the
dashboard's `/auth/callback` and let the SPA complete the exchange through the same-origin
proxy. Tokens live in `localStorage`, expire with the JWT and are revoked server-side by
"sign out everywhere" / deactivation.

Roles map to what the UI shows (`AdminPermission` minimum roles): a **viewer** reads
applications, nodes and analytics; a **moderator** adds moderation, chat and the audit log; an
**admin** manages applications, keys, webhooks, channels, recordings, node drain and sees the
configuration view; a **superadmin** deletes applications, runs retention and manages
administrators. The gating is cosmetic — the node enforces every permission — and the E2E
suite checks that a viewer is refused by the server, not just hidden from a button.

## Compatibility and upgrades

The dashboard and the node share one version number and are released together. A dashboard
may run against a node of the same minor version; a newer dashboard against an older node
degrades by feature (routes the node lacks show as errors on that page only — the shell keeps
working), an older dashboard against a newer node simply lacks the new sections. Upgrade the
node first, then the dashboard; roll back the dashboard alone by pinning the previous image
tag — it holds no state beyond the browser's `localStorage`.

## Development and tests

```bash
cd dashboard && npm ci
AURIX_API_URL=http://127.0.0.1:8080 npm run dev      # Vite on :5173 with the same API proxy
npm run check && npm run lint && npm test && npm run build
AURIX_E2E_ADMIN_EMAIL=… AURIX_E2E_ADMIN_PASSWORD=… npx playwright test   # against a live node
```

The Playwright suite runs against a real node (never a mocked API): first-administrator setup
or sign-in, the same-origin proxy and SSE, applications and one-time key reveal, live channel
CRUD, administrators / roles / audit with server-side denial, node drain / undrain, and the
read-only configuration view. Set `DASHBOARD_URL` to run it against a deployed container instead
of Vite; CI does exactly that against the freshly built Caddy image
(`.github/workflows/ci.yml`, job `dashboard`).

## What it does not do

* **No configuration mutation.** The configuration view is read-only and there is no API to
  change a node's settings at runtime — configuration stays in files, environment and the Helm
  chart. Drain / undrain is the only node-level action.
* **No listening in.** Operators see who speaks, transport and quality, never the audio; E2EE
  channels are opaque to the node and therefore to the dashboard.
* **No per-application administrators.** Administrators are global to the fleet; application
  scope in the UI is a filter, not a tenancy boundary.
