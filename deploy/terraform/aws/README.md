# Terraform example — one Aurix region on AWS

Creates a **regional node pool**: `node_count` EC2 instances with Elastic IPs, each running
the Aurix container on the host network behind Caddy (automatic Let's Encrypt), plus the
shared data stores the first region needs — RDS PostgreSQL (Multi-AZ, encrypted) and
ElastiCache Redis (TLS, automatic failover). Secrets are generated with the `random`
provider, stored in Secrets Manager and pulled by the instances at boot through an IAM
role — nothing secret is written into user-data or tags. (They do live in the Terraform
state: keep it in an encrypted, access-controlled backend.)

```
                     Route 53
   api-eu-central.voice.example.com  ──►  round robin over the pool (backend REST)
   aurix-eu-central-0.voice.example.com ► EIP ► EC2 ┐ Caddy :443 ─► aurix :8080 (REST) / :8081 (WS)
   aurix-eu-central-1.voice.example.com ► EIP ► EC2 ┘ UDP :10000 media, :10001 cascade, :3478 + relay TURN
                                                       │
                                     RDS PostgreSQL ◄──┴──► ElastiCache Redis (rediss://)
```

Every node has its own hostname, so `GET /v1/regions` and the `endpoint` in
`POST /v1/tokens` can hand a player a direct `wss://` URL and a reconnect resumes on the node
that still holds the session.

## Use

```bash
cd deploy/terraform/aws
cat > eu-central.tfvars <<'TFVARS'
aws_region      = "eu-central-1"
aurix_region    = "eu_central"
location        = { latitude = 50.11, longitude = 8.68 }
node_count      = 2
image           = "ghcr.io/aurix-voice/aurix-server:1.5.0"
domain          = "voice.example.com"
route53_zone_id = "Z0123456789ABCDEFGHIJ"
cors_origins    = ["https://game.example.com"]
acme_email      = "ops@example.com"
TFVARS
terraform init
terraform apply -var-file=eu-central.tfvars
```

Then bootstrap the admin account once (the token is in the fleet secret, see output
`admin_bootstrap_hint`) and remove `admin_bootstrap_token` from the secret afterwards.

### Second and further regions

All regions share **one** database, one Redis and one cascade secret. Apply the same module
again — in a separate state (workspace or directory) — with the outputs of the first region:

```bash
terraform apply -var-file=us-east.tfvars \
  -var secrets_arn="$(terraform -chdir=../eu-central output -raw secrets_arn)" \
  -var database_url="$(terraform -chdir=../eu-central output -raw database_url)" \
  -var redis_url="$(terraform -chdir=../eu-central output -raw redis_url)"
```

Cross-region access to RDS/ElastiCache (VPC peering or Transit Gateway plus the security
group rules on the data stores) is **not** created by this example — wire it the way your
network is organised, or run PostgreSQL/Redis in every region with your own replication.
Control-plane latency to a remote database is acceptable; media never crosses it.

## Variables worth knowing

| variable | default | notes |
|---|---|---|
| `node_count` | 2 | one EIP + one instance each; `aurix-<region>-<n>.<domain>` |
| `instance_type` | `c6i.large` | Graviton types pick the arm64 Debian AMI automatically |
| `vpc_id`, `subnet_ids` | create a VPC | pass existing public subnets instead |
| `turn_enabled`, `turn_port_range` | on, 49152–65535 | security-group rules follow |
| `ssh_cidrs`, `key_name` | none | Session Manager is enabled via IAM anyway |
| `database_url`, `redis_url`, `secrets_arn` | create | set all three when attaching to an existing fleet |
| `extra_env` | `{}` | any other `AURIX__*` setting, e.g. `AURIX__RECORDING__S3_BUCKET` |

## What the instance does at boot

`templates/user-data.sh.tftpl` installs Docker, tunes UDP buffers, fetches the fleet and pool
secrets, writes `/etc/aurix/aurix.env` (mode 0600), `docker-compose.yml`, the `Caddyfile`,
and starts a `systemd` unit. Node `0` runs migrations (`database.run_migrations=true`); the
others start with migrations off. Node ids are `uuidv5(dns, hostname)`, so a replaced
instance re-registers under the same `media_nodes.id`. Metrics listen on `127.0.0.1:4040`
for a node-local Prometheus agent.

## Not covered

Backups beyond RDS automated snapshots, WAF/DDoS protection, recordings storage (set the S3
settings through `extra_env` and give the node role `s3:PutObject`/`GetObject`), autoscaling —
nodes are stateful (UDP sessions), so scale by changing `node_count`; a removed node closes
its sessions gracefully on `SIGTERM` and clients resume elsewhere as fresh sessions.

## Validation

```bash
terraform fmt -check -recursive deploy/terraform
terraform -chdir=deploy/terraform/aws init -backend=false
terraform -chdir=deploy/terraform/aws validate
```
