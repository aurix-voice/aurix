# Administrator accounts and SSO

Administrators sign in with a password (`POST /admin/login`) or through your identity provider
with OpenID Connect (`GET /admin/oidc/login`). Both produce the same admin JWT; what an
administrator may do is decided by their [role](../concepts/auth.md#administrators) as stored in
Aurix, on every request. This chapter covers the OIDC setup, how provider groups become roles,
the lifecycle API and how tokens are revoked.

`GET /admin/auth/methods` (no credential) tells a login page which methods this deployment
offers:

```json
{"password_login": true, "oidc": {"issuer": "https://login.example.com/realms/aurix", "login_url": "/admin/oidc/login"}}
```

## OpenID Connect

Aurix is a standard OIDC **relying party**: authorization-code flow with PKCE (S256), `state`
and `nonce`, ID tokens verified against the provider's JWKS. Any provider that publishes
`/.well-known/openid-configuration` works — Keycloak, Microsoft Entra ID, Okta, Google, Auth0,
Authelia/Authentik, Dex, … Register a client with redirect URI `https://<api>/admin/oidc/callback`
and put the details in `[auth.oidc]`:

```toml
[auth]
admin_password_login = true      # false: SSO only — POST /admin/login is refused

[auth.oidc]
enabled = true
issuer = "https://login.example.com/realms/aurix"
client_id = "aurix-admin"
client_secret = ""               # or AURIX__AUTH__OIDC__CLIENT_SECRET; empty = public client (PKCE only)
redirect_url = "https://api.example.com/admin/oidc/callback"
frontend_redirect = "https://panel.example.com/auth/callback"
scopes = ["openid", "email", "profile"]
email_claim = "email"
groups_claim = "groups"          # nested claims work: "resource_access.aurix.roles"
role_mapping = { "aurix-superadmins" = "superadmin", "aurix-admins" = "admin", "aurix-mods" = "moderator" }
default_role = ""                # "" refuses users without a mapped group; or "viewer"
superadmin_emails = ["ops@example.com"]
allowed_domains = ["example.com"]
require_email_verified = true
auto_provision = true
sync_roles = true
state_ttl_secs = 600
timeout_ms = 10000
```

| Key | Meaning |
| --- | --- |
| `issuer` | Discovery is fetched from `{issuer}/.well-known/openid-configuration` at start (a failure is logged and retried on the first login) and must report the same `issuer`. `https` only (see `allow_insecure`). |
| `client_id`, `client_secret` | With a secret the token endpoint is called as a confidential client (`client_secret_basic`); without one Aurix is a public client protected by PKCE alone. |
| `redirect_url` | Exactly what you registered at the provider — the public URL of `GET /admin/oidc/callback`. |
| `frontend_redirect` | Where the browser goes after login. The admin JWT is handed over in the URL **fragment** (`#token=…&expires_in_secs=…[&return_to=/…]`) so it never reaches a server log; failures arrive as `#error=…&error_description=…`. Unset: the callback answers with JSON (`{token, expires_in_secs, admin, return_to}`) — handy for API-only setups and tests. |
| `scopes` | Must contain `openid`; add whatever your provider needs to emit the email and groups claims. |
| `email_claim`, `groups_claim` | Claim names in the ID token. Dotted paths descend into nested objects (Keycloak client roles: `resource_access.aurix.roles`). Groups may be a JSON array or a space/comma separated string. When the ID token lacks the email or groups, `userinfo` is consulted with the access token. |
| `role_mapping` | Group value → `viewer` / `moderator` / `admin` / `superadmin`. Matching is exact and case-sensitive. A user in several mapped groups gets the highest role. Through environment variables (`AURIX__AUTH__OIDC__ROLE_MAPPING__<GROUP>=<role>`) group names are lower-cased by the configuration loader and cannot contain `-` or `.`, so use the TOML file for anything but simple lower-case names. |
| `default_role` | Role for users without any mapped group. Empty (default) refuses them with `403`. |
| `superadmin_emails` | Always `superadmin`, whatever the groups say (compared case-insensitively). This is how the first administrator of an SSO-only deployment gets in — no bootstrap password needed. |
| `allowed_domains` | Only these email domains may administer the deployment (`403` otherwise). Empty: any address the provider vouches for. |
| `require_email_verified` | Requires `email_verified: true` (default). Turn off only for providers that never set the claim. |
| `auto_provision` | Create the account on first login (default). `false`: only accounts that already exist (created with `POST /admin/admins` or provisioned earlier) can sign in. |
| `sync_roles` | Re-apply the mapped role on every login (default). `false`: the role set in Aurix (`PATCH /admin/admins/{id}`) sticks and the provider only authenticates. |
| `state_ttl_secs` | A login must complete within this window (30–3600, default 600). |
| `timeout_ms` | Per-request timeout for discovery, JWKS, token and userinfo calls (500–60 000). |
| `allow_insecure` | Accept `http` issuers and private/loopback provider addresses. Development and CI only. |

Role resolution order: `superadmin_emails` → highest role among mapped groups → `default_role`
→ refuse.

### Login flow

1. The dashboard sends the browser to `GET /admin/oidc/login[?return_to=/apps/…]`. Aurix sets the
   `aurix_oidc_login` cookie (HttpOnly, SameSite=Lax, `Secure` when the callback is https, path
   `/admin/oidc`) and answers `302` to the provider's authorization endpoint with PKCE challenge,
   `state` and `nonce`. `return_to` must be a relative path (`/…`, not `//…`, ≤ 512 chars) and
   is handed back untouched — the dashboard decides what to do with it.
2. The user authenticates at the provider, which redirects to `redirect_url` with `code` and
   `state`.
3. `GET /admin/oidc/callback` decrypts and consumes `state` (single use, bound to the cookie,
   expired after `state_ttl_secs`), exchanges the code, verifies the ID token — signature via
   JWKS (asymmetric algorithms only), `iss`, `aud`, `azp`, `exp`/`iat`, `nonce` — applies the
   email, domain and role rules above, creates or binds the account and issues the admin JWT.

Signing-key rotation at the provider needs no restart: an unknown `kid` triggers a JWKS refetch
(at most once every 30 s, so a flood of forged tokens cannot hammer the provider). The provider
is only ever contacted at addresses that discovery advertised, never at private/loopback ranges
unless `allow_insecure` is on. Login attempts are rate-limited per IP like `POST /admin/login`
(`rate_limiting.admin_login_per_minute`).

The flow works across a load-balanced fleet: `state` is sealed with a key derived from
`auth.jwt_secret`, which every node shares, so the callback may land on a different node than
the login. The single-use record of consumed states is per node; a replay that reaches another
node is still stopped by the provider, which accepts each authorization code once.

### Accounts, binding and provisioning

An SSO identity is `issuer + subject`. On a successful login:

* **Known identity** → that account signs in; with `sync_roles` its role follows the provider.
  Its email in Aurix does not change when the provider renames the user.
* **Unknown identity, email matches an existing account without an SSO binding** → the account is
  bound to this identity (password stays usable) and, with `sync_roles`, takes the provider's
  role. Emails are compared lower-cased.
* **Unknown identity, email already bound to a different identity** → refused (`401`). Nobody
  can take over an administrator by presenting the same email from another provider account.
* **Unknown identity and email** → provisioned when `auto_provision` is on (`auth_source:
  "oidc"`, no password — `has_password: false`), refused otherwise.

Deactivated accounts cannot sign in through SSO either. To let an SSO-provisioned administrator
also use a password, a superadmin sets one with `POST /admin/admins/{id}/password` (or the
administrator sets it themself with `POST /admin/me/password`, any `current_password`).

### SSO-only deployments

Set `auth.admin_password_login = false` once SSO works. `POST /admin/login` then answers
`401 Password login is disabled`, `POST /admin/setup` is still available for the bootstrap
flow, and existing password accounts keep their tokens until they expire. The setting is
refused at start-up unless `auth.oidc.enabled` is on, so a typo cannot lock everybody out. Keep
at least one `superadmin_emails` entry (or a mapped superadmin group) so a superadmin can always
sign in.

## Lifecycle API

All routes below need `admins:manage` (the `superadmin` role) except where noted.

| Route | Does |
| --- | --- |
| `GET /admin/admins` | Every account: `id`, `email`, `display_name`, `role`, `permissions`, `active`, `auth_source`, `sso_bound`, `has_password`, `last_login_at`, timestamps. |
| `GET /admin/admins/{id}` | One account. |
| `POST /admin/admins` | Password account (`email`, `password` ≥ 12 chars, `display_name`, `role` default `admin`). SSO accounts are provisioned on their first login instead. |
| `PATCH /admin/admins/{id}` | `role`, `display_name`, `active`. A role change or `active: false` revokes the target's tokens; `active: true` reactivates (an account created for the same email in the meantime blocks it with `409`). |
| `POST /admin/admins/{id}/password` | Sets a new password (also for SSO-only accounts) and revokes the target's tokens. |
| `POST /admin/admins/{id}/logout-all` | Revokes the target's tokens; the account stays active. |
| `GET /admin/me` | *any active administrator* — the account as stored now; `auth_source` says how the presented token was obtained. |
| `POST /admin/me/password` | *any active administrator* — `current_password` + `new_password`; revokes all of the caller's tokens including the one used. |
| `POST /admin/logout-all` | *any active administrator* — revokes all of the caller's tokens. |

Rules enforced by the server: nobody changes their own role or deactivates themself (`400`); the
last active `superadmin` cannot be demoted or deactivated (`409`); creating an account for an
email that is active already is `409`; a deactivated account's email may be reused.

### Token revocation

Admin JWTs are validated against the account row: current `role`, `active`, and a **token
generation** that has to match. Every action listed above as "revokes tokens" increments the
generation, so tokens issued earlier are refused (`401 TOKEN_INVALID`) immediately, on every
node, without shared state beyond PostgreSQL. Provider-driven role changes (`sync_roles`)
revoke the same way — the token minted by that login is newer than the bump and stays valid.
Deactivation answers `401 AUTH_FAILED`; a demotion that leaves the token valid shows up as `403`
on the routes the new role lacks.

Audit entries (`GET /admin/audit-log`) record logins with their source (`password` / `oidc`),
account changes, password resets and revocations.

## Testing against the mock provider

`crates/aurix-server/examples/mock_oidc.rs` is a small OIDC provider for CI and local work:
discovery, JWKS (with an EdDSA → ES256 rotation switch), authorization, token and userinfo
endpoints, plus `/_mock/*` controls for the user that signs in next (groups, unverified email,
wrong nonce/audience, denied consent, missing claims).

```bash
cargo run -p aurix-server --example mock_oidc -- 127.0.0.1:18791 aurix-admin ci-only-oidc-secret
AURIX__AUTH__OIDC__ENABLED=true AURIX__AUTH__OIDC__ISSUER=http://127.0.0.1:18791 \
AURIX__AUTH__OIDC__CLIENT_ID=aurix-admin AURIX__AUTH__OIDC__CLIENT_SECRET=ci-only-oidc-secret \
AURIX__AUTH__OIDC__REDIRECT_URL=http://127.0.0.1:8080/admin/oidc/callback \
AURIX__AUTH__OIDC__ALLOW_INSECURE=true AURIX__AUTH__OIDC__ROLE_MAPPING__AURIX_ADMINS=admin \
AURIX__AUTH__OIDC__ROLE_MAPPING__AURIX_OPS=moderator AURIX__AUTH__OIDC__SUPERADMIN_EMAILS=root.sso@example.com \
AURIX__AUTH__OIDC__ALLOWED_DOMAINS=example.com ./target/debug/aurix-server
AURIX_E2E_OIDC=1 AURIX_E2E_API=http://127.0.0.1:8080 AURIX_E2E_WS=ws://127.0.0.1:8081 \
AURIX_E2E_API_KEY=aurx_… AURIX_E2E_ADMIN_TOKEN=<superadmin jwt> \
cargo test -p aurix-server --test e2e_live -- --ignored admin_sso
```

The E2E drives the whole flow over HTTP (redirect parsing, cookie binding, PKCE, state replay,
wrong nonce/audience, unverified email, foreign domain, key rotation, userinfo fallback) and the
lifecycle API; CI runs it in the *admin SSO* step. The PostgreSQL integration test
`crates/aurix-auth/tests/pg_live.rs` covers the same rules at the service level.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| Node refuses to start: `auth.oidc.issuer` / `redirect_url` must be https | Use `https`, or `allow_insecure = true` for local providers only. |
| `Admin OIDC discovery failed` in the log, login answers `500`/`502` | The discovery URL is unreachable from the node, or the document's `issuer` differs from the configured one (realm path, trailing slash). |
| `403 Your account is not mapped to an administrator role` | No group matched `role_mapping` and `default_role` is empty. Check the claim name (`groups_claim`) and that the provider puts groups into the **ID token** or `userinfo` for this client. |
| `403 Email domain … is not allowed` | `allowed_domains`. |
| `401 Email is not verified` | The provider does not set `email_verified`; verify emails there or set `require_email_verified = false`. |
| `401 Invalid OIDC state` / `OIDC login attempt expired` / `OIDC state already used` | The callback was opened without the `aurix_oidc_login` cookie (different host than `redirect_url`, cookies blocked), more than `state_ttl_secs` passed, or the same callback URL was loaded twice. Start again from `/admin/oidc/login`. |
| `401 ID token signed with an unknown key` after rotation | Wait 30 s (JWKS refetch cooldown) and retry. |
| `401 This email is already bound to another SSO identity` | The email belongs to an account bound to a different provider subject — intended; a superadmin can deactivate the old account, after which the email is free again. |
| `502` from the callback | The provider's token or userinfo endpoint was unreachable or answered with garbage; see the node log. |
