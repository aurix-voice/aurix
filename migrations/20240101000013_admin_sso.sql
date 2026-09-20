-- Administrator accounts: SSO identity, role/permission bookkeeping and token revocation.
--
-- * `auth_source`      'password' | 'oidc' — how the account signs in.
-- * `sso_issuer/subject` the OpenID Connect identity the account is bound to (unique per issuer).
-- * `token_generation` every admin JWT carries the generation it was issued under; a password
--                      change, role change, deactivation or "sign out everywhere" bumps it and
--                      earlier tokens are refused on every node without relying on clocks.
-- * `tokens_revoked_at` when the generation was last bumped (informational).
ALTER TABLE admin_users
    ADD COLUMN auth_source TEXT NOT NULL DEFAULT 'password',
    ADD COLUMN sso_issuer TEXT,
    ADD COLUMN sso_subject TEXT,
    ADD COLUMN token_generation BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN tokens_revoked_at TIMESTAMPTZ;

CREATE UNIQUE INDEX idx_admin_sso_identity
    ON admin_users(sso_issuer, sso_subject)
    WHERE sso_issuer IS NOT NULL AND sso_subject IS NOT NULL;

-- The original schema made `email` globally unique, which blocked re-inviting an address whose
-- old account was deactivated; the partial index below keeps active emails unique instead.
ALTER TABLE admin_users DROP CONSTRAINT IF EXISTS admin_users_email_key;
