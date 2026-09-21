# Security policy

Aurix is a self-hosted voice stack; a vulnerability in it is a vulnerability in every game that
runs it. Please report privately and give us time to ship a fix before disclosing.

## Reporting a vulnerability

* Preferred: **GitHub private vulnerability reporting** on this repository
  ("Security" tab → "Report a vulnerability"). It reaches the maintainers only.
* If private reporting is unavailable on your fork or mirror, contact a maintainer listed in the
  repository directly and ask for an encrypted channel before sending details.
* Do **not** open a public issue, discussion or pull request for a security problem.

Include what you can: affected version/commit, component (node, TURN, a client SDK, a server SDK,
the CLI, deployment charts), a reproduction or a fuzz artifact, and the impact you believe it has.
A crashing input for one of the `fuzz/` targets is a complete report.

We acknowledge within **3 business days**, give an initial assessment within **10**, and aim to
release a fix for confirmed high/critical issues within **30 days** (longer with your agreement if
coordination with downstream projects is needed). We credit reporters in the release notes unless
you prefer otherwise. There is no bug bounty.

## Scope

In scope — anything that breaks a row of the "Defended" table in the
[threat model](docs/src/concepts/threat-model.md):

* cross-tenant or cross-user access to media, chat, recordings, transcripts or metadata;
* accepting media, control messages or TURN allocations without valid credentials;
* forging or replaying tokens, resume tokens, action tokens, webhook signatures, cascade relays;
* plaintext leaks from `e2ee: true` channels through the node;
* memory-safety issues, panics or unbounded resource use triggered by network input
  (including the bundled libopus);
* SSRF through webhook / live-stream / provider URLs;
* privilege escalation between admin roles or from player to admin;
* vulnerabilities in the client SDKs (native core, Web, Unity, Unreal, Godot) or server SDKs
  that are exploitable by a malicious node or peer.

Out of scope: findings that require a compromised operator or game backend (they are the trust
root by design), volumetric DDoS, issues in third-party providers you configure, and reports
against the deliberately insecure `dev` defaults (`environment = "development"`, `/dev/login`
in token-server examples) when the same configuration is refused in production mode.

## Supported versions

Aurix follows [semantic versioning](docs/src/operations/releases.md). Security fixes are released
for:

| Series | Status |
|---|---|
| latest minor (`1.x` current) | fixes for all confirmed issues |
| previous minor | fixes for high/critical issues for 6 months after the next minor ships |
| older | not supported — upgrade |

Pre-releases (`-rc.N`) and `main` receive fixes as part of normal development only.

## Verifying what you run

Every release ships SHA-256 checksums and a CycloneDX SBOM for each artifact, and — when the
repository has signing enabled — Sigstore/cosign keyless signatures for archives and container
images. See [Releases](docs/src/operations/releases.md) for the verification commands.

## Hardening checklist for operators

* Run with `AURIX__SERVER__ENVIRONMENT=production`; the node refuses weak secrets and open CORS.
* Keep the API key on the game backend only ([token servers](docs/src/backend/server-sdks.md)).
* Terminate TLS in front of the API/WS port; expose only the media UDP range and TURN publicly.
* Rotate API keys (`aurix app rotate-key` / `POST /v1/apps/:id/rotate-key`) and the
  `media.cascade_secret`, JWT and TURN secrets on a schedule (rolling restart).
* Watch the audit log and `aurix_packets_dropped_total`, `aurix_rate_limit_hits_total`,
  `aurix_ws_takeovers_refused_total` for probing.
* Subscribe to this repository's security advisories.
