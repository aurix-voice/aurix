# Aurix Helm chart

Deploys a regional pool of Aurix nodes: one `StatefulSet`, one pod per Kubernetes node, every
pod a complete node (REST + WebSocket + SFU + TURN). PostgreSQL and Redis are **external**
(managed services or your own charts) — the chart only carries their URLs.

```bash
helm install aurix deploy/helm/aurix -n aurix --create-namespace \
  -f my-values.yaml                       # start from ci/single-node-values.yaml
helm test aurix -n aurix                  # curls /ready inside the cluster
```

## Why a StatefulSet on the host network

Players send UDP media **directly to the node that owns their session** — there is no UDP load
balancing and no session migration. Each pod therefore needs:

* a public IP for `media.external_ip` / `turn.external_ip` (`config.externalIpSource`:
  `static` → `config.externalIp` or one `config.externalIps[]` entry per ordinal; `hostIP` →
  the Kubernetes node's address; `script` → an init container runs `config.externalIpScript`,
  e.g. the cloud metadata query);
* UDP `ports.media`, `ports.media + 1` (cascade) and, with TURN, `ports.turn` + the
  `turn.minPort–maxPort` relay range reachable on that IP. `hostNetwork: true` gives all of
  that for free; `hostNetwork: false` publishes the single ports with `hostPort` and cannot
  carry the TURN relay range;
* its **own hostname** for the WebSocket if you want region discovery and session resume to
  address one node: `perNode.enabled` renders `<release>-<ordinal>.<perNode.domain>` — a
  Service pinned to the pod plus an Ingress rule — and sets `server.external_url` /
  `server.external_ws_url` accordingly. Create the DNS records (or run external-dns) pointing
  at each node's public IP. Without `perNode`, discovery advertises the shared
  `config.externalUrl` and a resume that lands on another pod becomes a fresh session.

Required one-pod-per-node anti-affinity is always rendered.

## Values you must set

| value | purpose |
|---|---|
| `config.externalUrl` (or `perNode.*`) | public https URL; `wss://<host>/ws` is derived from it |
| `config.corsOrigins` | allowed browser origins (no wildcard in production) |
| `config.externalIpSource` + `externalIp`/`externalIps`/`externalIpScript` | public media IP per pod |
| `config.externalIpv6`/`externalIpv6s`, `config.dualStack` | optional public IPv6 per pod → media/TURN bind `::` and advertise IPv4 + IPv6 |
| `config.region`, `config.location` | region label and coordinates for [region discovery](../../../docs/src/operations/scaling.md#regions) |
| `existingSecret` **or** `secrets.*` | see below |

### Secrets

The pods read secrets with `envFrom`, so the Secret's keys are the `AURIX__*` variables:

| key | required |
|---|---|
| `AURIX__DATABASE__URL` | yes — `postgres://user:pass@host:5432/aurix` |
| `AURIX__REDIS__URL` | yes — `redis://` / `rediss://` |
| `AURIX__AUTH__JWT_SECRET` | yes — ≥ 32 random bytes |
| `AURIX__TURN__AUTH_SECRET` | when TURN is enabled |
| `AURIX__MEDIA__CASCADE_SECRET` | same value on every node of every region; enables cascade |
| `AURIX__AUTH__ADMIN_BOOTSTRAP_TOKEN` | first `POST /admin/setup` only — remove afterwards |
| `AURIX__RECORDING__ENCRYPTION_KEY`, `AURIX__RECORDING__S3_ACCESS_KEY`, `AURIX__RECORDING__S3_SECRET_KEY` | recordings |

Prefer `existingSecret` fed by an external secret manager; `secrets.*` values are stored in
the Helm release history.

## What else is rendered

* **Migrations**: a `pre-install,pre-upgrade` Job runs `aurix-server --migrate-only`, and the
  pods start with `database.run_migrations=false`, so a rolling upgrade never races on the
  schema. Disable with `migrations.enabled=false` to let the first pod migrate instead.
* `Service` (ClusterIP: api, ws, metrics) + headless Service for the StatefulSet; per-node
  Services with `perNode`.
* `Ingress` with `/ws` → WebSocket port and `/` → API (add proxy read/send timeouts of an
  hour or more for WebSockets — see `ci/single-node-values.yaml`).
* Optional `PodDisruptionBudget`, `ServiceMonitor`, `NetworkPolicy`, recordings PVC per pod
  (`persistence`) — with several nodes use S3 (`config.recording.s3`) instead so recordings are
  reachable from every node.
* Hardened defaults: uid 10001, read-only root, no capabilities, seccomp `RuntimeDefault`, no
  service-account token.
* **Operator dashboard** (`dashboard.enabled=true`, off by default): a Deployment of the
  `aurix-dashboard` Caddy image proxying `/v1`, `/admin`, `/health`, `/ready`, `/openapi.json` to
  the release's API Service (`dashboard.apiUrl`), its own ClusterIP Service, optional PDB, and
  either `dashboard.ingress` (standard Ingress, any controller) or `dashboard.ingressRoute`
  (Traefik CRD: host, entry points, cert resolver or TLS secret, middlewares). The dashboard pod
  is the node's client — add the pod CIDR to `config.trustedProxies`; the NetworkPolicy allows
  its egress to the API. See [Operator dashboard](../../../docs/src/operations/dashboard.md).

## Multi-region

Install the chart once per region (one cluster or one namespace per region) with the same
database, Redis and `AURIX__MEDIA__CASCADE_SECRET`, a different `config.region` /
`config.location` and per-node hostnames. Nodes find each other through the `media_nodes`
table; `GET /v1/regions` and the `endpoint` in `POST /v1/tokens` then hand each player the
nearest region with capacity. See [Scaling out](../../../docs/src/operations/scaling.md).

## Validation

```bash
helm lint deploy/helm/aurix --strict -f deploy/helm/aurix/ci/multi-node-values.yaml
helm template aurix deploy/helm/aurix -f deploy/helm/aurix/ci/multi-node-values.yaml | kubeconform -strict
```
