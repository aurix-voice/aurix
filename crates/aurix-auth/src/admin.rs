use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use aurix_common::error::{AurixError, Result};
use aurix_common::types::{AdminAuthSource, AdminContext, AdminRole};
use aurix_db::models::AdminUserRow;
use aurix_db::DbPool;
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const ADMIN_TOKEN_TYPE: &str = "aurix-admin";
const MIN_PASSWORD_LEN: usize = 12;
/// Stored in `password_hash` for SSO-only accounts; never verifies.
const NO_PASSWORD: &str = "!";

#[derive(Debug, Serialize, Deserialize)]
struct AdminClaims {
    sub: String,
    admin_id: String,
    email: String,
    role: String,
    /// Distinguishes admin tokens from end-user tokens that may share the signing secret.
    typ: String,
    /// How the token was obtained (`password` / `oidc`). Older tokens carry no `src`.
    #[serde(default)]
    src: Option<AdminAuthSource>,
    /// `admin_users.token_generation` at issue time; a mismatch means the token was revoked.
    /// Tokens minted before the column existed carry none and count as generation 0.
    #[serde(default, rename = "gen")]
    generation: i64,
    exp: i64,
    iat: i64,
}

/// Valid Argon2id hash of a random password, used to equalise timing for unknown emails.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Zm9vYmFyYmF6cXV4cXV1eGZvb2JhcmJhenF1eHF1dXg";

/// A verified identity handed over by the OIDC provider, already mapped to a local role.
#[derive(Debug, Clone)]
pub struct SsoLogin {
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub display_name: String,
    pub role: AdminRole,
    /// Create the account when neither the SSO identity nor the email is known.
    pub auto_provision: bool,
    /// Overwrite the stored role with `role` on every login.
    pub sync_role: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AdminUpdate {
    pub role: Option<AdminRole>,
    pub display_name: Option<String>,
    pub active: Option<bool>,
}

pub struct AdminAuthService {
    pool: DbPool,
    jwt_secret: String,
    token_ttl_secs: i64,
    bootstrap_token: Option<String>,
    password_login: bool,
}

impl AdminAuthService {
    pub fn new(pool: DbPool, jwt_secret: String) -> Self {
        Self {
            pool,
            jwt_secret,
            token_ttl_secs: 8 * 3600,
            bootstrap_token: None,
            password_login: true,
        }
    }

    pub fn with_token_ttl(mut self, ttl_secs: i64) -> Self {
        self.token_ttl_secs = ttl_secs.max(60);
        self
    }

    pub fn with_bootstrap_token(mut self, token: Option<String>) -> Self {
        self.bootstrap_token = token.filter(|t| !t.is_empty());
        self
    }

    pub fn with_password_login(mut self, enabled: bool) -> Self {
        self.password_login = enabled;
        self
    }

    pub fn password_login_enabled(&self) -> bool {
        self.password_login
    }

    pub fn token_ttl_secs(&self) -> i64 {
        self.token_ttl_secs
    }

    pub async fn admin_count(&self) -> Result<i64> {
        aurix_db::queries::count_admin_users(&self.pool)
            .await
            .map_err(|e| AurixError::Database(format!("Admin count failed: {e}")))
    }

    /// First-run bootstrap: creates the initial superadmin.
    ///
    /// Allowed only when (a) no active admin exists yet, or (b) `auth.admin_bootstrap_token`
    /// is configured and `presented_token` matches it (constant-time). Anything else is denied.
    pub async fn bootstrap_admin(
        &self,
        presented_token: Option<&str>,
        email: &str,
        password: &str,
        display_name: &str,
    ) -> Result<AdminUserRow> {
        let token_ok = match (&self.bootstrap_token, presented_token) {
            (Some(expected), Some(given)) => {
                aurix_common::crypto::constant_time_eq(expected.as_bytes(), given.as_bytes())
            }
            _ => false,
        };
        if !token_ok && self.admin_count().await? > 0 {
            return Err(AurixError::AuthorizationDenied(
                "Admin bootstrap is disabled: an administrator already exists".into(),
            ));
        }
        self.create_admin(email, password, display_name, AdminRole::Superadmin)
            .await
    }

    pub async fn create_admin(
        &self,
        email: &str,
        password: &str,
        display_name: &str,
        role: AdminRole,
    ) -> Result<AdminUserRow> {
        let email = normalize_email(email)?;
        validate_password(password)?;
        let display_name = validate_display_name(display_name)?;
        self.ensure_email_free(&email).await?;
        let password_hash = self.hash_password(password)?;
        let admin = new_row(
            email,
            password_hash,
            display_name,
            role,
            AdminAuthSource::Password,
            None,
        );
        aurix_db::queries::create_admin_user(&self.pool, &admin)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to create admin: {e}")))
    }

    pub async fn authenticate(
        &self,
        email: &str,
        password: &str,
    ) -> Result<(AdminUserRow, String)> {
        if !self.password_login {
            return Err(AurixError::AuthenticationFailed(
                "Password login is disabled; sign in through SSO".into(),
            ));
        }
        let email = email.trim().to_ascii_lowercase();
        let admin = aurix_db::queries::get_admin_by_email(&self.pool, &email)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?;
        // Always run a password verification so response timing does not reveal whether
        // the email exists or whether the account is SSO-only.
        let Some(admin) = admin.filter(|a| a.password_hash != NO_PASSWORD) else {
            let _ = self.verify_password(password, DUMMY_HASH);
            return Err(AurixError::AuthenticationFailed(
                "Invalid credentials".into(),
            ));
        };
        self.verify_password(password, &admin.password_hash)?;
        let _ = aurix_db::queries::update_admin_login(&self.pool, admin.id).await;
        let token = self.issue_token(&admin, AdminAuthSource::Password)?;
        Ok((admin, token))
    }

    /// Completes an SSO login: binds or provisions the local account and issues a token.
    pub async fn login_sso(&self, login: SsoLogin) -> Result<(AdminUserRow, String)> {
        let email = normalize_email(&login.email)?;
        let display_name = validate_display_name(&login.display_name)
            .unwrap_or_else(|_| email.split('@').next().unwrap_or("admin").to_string());
        let by_identity =
            aurix_db::queries::get_admin_by_sso(&self.pool, &login.issuer, &login.subject)
                .await
                .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?;
        let admin = match by_identity {
            Some(existing) => {
                if !existing.active {
                    return Err(AurixError::AuthenticationFailed(
                        "Admin account is disabled".into(),
                    ));
                }
                let _ = aurix_db::queries::update_admin_login(&self.pool, existing.id).await;
                let stored = AdminRole::parse(&existing.role);
                if login.sync_role
                    && stored != Some(login.role)
                    && !self.is_last_superadmin(&existing, login.role).await?
                {
                    aurix_db::queries::update_admin_user(
                        &self.pool,
                        existing.id,
                        Some(login.role.as_str()),
                        None,
                        None,
                        true,
                    )
                    .await
                    .map_err(|e| AurixError::Database(format!("Role sync failed: {e}")))?
                    .unwrap_or(existing)
                } else {
                    existing
                }
            }
            None => {
                let by_email = aurix_db::queries::get_admin_by_email(&self.pool, &email)
                    .await
                    .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?;
                match by_email {
                    Some(existing) if existing.sso_subject.is_some() => {
                        return Err(AurixError::AuthenticationFailed(
                            "This email is already bound to another SSO identity".into(),
                        ));
                    }
                    Some(existing) => {
                        aurix_db::queries::bind_admin_sso(
                            &self.pool,
                            existing.id,
                            &login.issuer,
                            &login.subject,
                        )
                        .await
                        .map_err(|e| AurixError::Database(format!("SSO bind failed: {e}")))?;
                        let role = if login.sync_role
                            && AdminRole::parse(&existing.role) != Some(login.role)
                            && !self.is_last_superadmin(&existing, login.role).await?
                        {
                            Some(login.role.as_str())
                        } else {
                            None
                        };
                        aurix_db::queries::update_admin_user(
                            &self.pool,
                            existing.id,
                            role,
                            None,
                            None,
                            role.is_some(),
                        )
                        .await
                        .map_err(|e| AurixError::Database(format!("Role sync failed: {e}")))?
                        .unwrap_or(existing)
                    }
                    None => {
                        if !login.auto_provision {
                            return Err(AurixError::AuthenticationFailed(
                                "No administrator account for this identity".into(),
                            ));
                        }
                        let row = new_row(
                            email,
                            NO_PASSWORD.to_string(),
                            display_name,
                            login.role,
                            AdminAuthSource::Oidc,
                            Some((login.issuer.clone(), login.subject.clone())),
                        );
                        let created = aurix_db::queries::create_admin_user(&self.pool, &row)
                            .await
                            .map_err(|e| {
                                AurixError::Database(format!("Failed to provision admin: {e}"))
                            })?;
                        let _ = aurix_db::queries::update_admin_login(&self.pool, created.id).await;
                        created
                    }
                }
            }
        };
        let token = self.issue_token(&admin, AdminAuthSource::Oidc)?;
        Ok((admin, token))
    }

    /// Validates an admin JWT against the current account state: the account must be active,
    /// the token's generation must match the account's (a password/role change, deactivation
    /// or logout-all bumps it), and the role is the one stored now — never the claim.
    pub async fn authenticate_token(&self, token: &str) -> Result<AdminContext> {
        let claims = self.decode_token(token)?;
        let admin_id = Uuid::parse_str(&claims.admin_id)
            .map_err(|_| AurixError::TokenInvalid("Bad admin_id".into()))?;
        let admin = aurix_db::queries::get_admin_by_id(&self.pool, admin_id)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?
            .ok_or_else(|| AurixError::AuthenticationFailed("Admin account is disabled".into()))?;
        if claims.generation != admin.token_generation {
            return Err(AurixError::TokenInvalid("Admin token revoked".into()));
        }
        let role = AdminRole::parse(&admin.role).ok_or_else(|| {
            AurixError::Internal(format!(
                "Admin {} has unknown role {:?}",
                admin.id, admin.role
            ))
        })?;
        Ok(AdminContext {
            admin_id,
            email: admin.email,
            role,
            auth_source: claims.src.unwrap_or(AdminAuthSource::Password),
        })
    }

    fn decode_token(&self, token: &str) -> Result<AdminClaims> {
        let key = DecodingKey::from_secret(self.jwt_secret.as_bytes());
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_required_spec_claims(&["exp", "sub"]);
        validation.validate_exp = true;
        validation.leeway = 30;

        let data = decode::<AdminClaims>(token, &key, &validation).map_err(|e| match e.kind() {
            jsonwebtoken::errors::ErrorKind::ExpiredSignature => AurixError::TokenExpired,
            _ => {
                tracing::debug!("admin token rejected: {e}");
                AurixError::TokenInvalid("Admin token invalid".into())
            }
        })?;
        if data.claims.typ != ADMIN_TOKEN_TYPE {
            return Err(AurixError::TokenInvalid("Not an admin token".into()));
        }
        Ok(data.claims)
    }

    fn issue_token(&self, admin: &AdminUserRow, source: AdminAuthSource) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = AdminClaims {
            sub: admin.id.to_string(),
            admin_id: admin.id.to_string(),
            email: admin.email.clone(),
            role: admin.role.clone(),
            typ: ADMIN_TOKEN_TYPE.to_string(),
            src: Some(source),
            generation: admin.token_generation,
            exp: now + self.token_ttl_secs,
            iat: now,
        };
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        )
        .map_err(|e| AurixError::Internal(format!("Admin token generation failed: {e}")))
    }

    pub async fn list_admins(&self) -> Result<Vec<AdminUserRow>> {
        aurix_db::queries::list_admin_users(&self.pool)
            .await
            .map_err(|e| AurixError::Database(format!("Admin list failed: {e}")))
    }

    pub async fn get_admin(&self, admin_id: Uuid) -> Result<AdminUserRow> {
        aurix_db::queries::get_admin_by_id_any(&self.pool, admin_id)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?
            .ok_or_else(|| AurixError::NotFound("Administrator not found".into()))
    }

    /// Role / name / active edits by `actor`. Guards: nobody changes their own role or
    /// deactivates themselves, and the last active superadmin cannot be demoted or disabled.
    /// Provider-driven role sync must never strip the last active superadmin: a group change
    /// at the IdP would otherwise lock everyone out of administrator management.
    async fn is_last_superadmin(&self, admin: &AdminUserRow, new_role: AdminRole) -> Result<bool> {
        if !admin.active
            || admin.role != AdminRole::Superadmin.as_str()
            || new_role == AdminRole::Superadmin
        {
            return Ok(false);
        }
        let superadmins = aurix_db::queries::count_active_admins_with_role(
            &self.pool,
            AdminRole::Superadmin.as_str(),
        )
        .await
        .map_err(|e| AurixError::Database(format!("Admin count failed: {e}")))?;
        if superadmins <= 1 {
            tracing::warn!(
                admin = %admin.email,
                provider_role = new_role.as_str(),
                "SSO role sync would remove the last active superadmin; keeping superadmin"
            );
            return Ok(true);
        }
        Ok(false)
    }

    /// A role change or deactivation revokes the target's existing tokens.
    pub async fn update_admin(
        &self,
        actor: &AdminContext,
        target_id: Uuid,
        update: AdminUpdate,
    ) -> Result<AdminUserRow> {
        let target = self.get_admin(target_id).await?;
        let display_name = match &update.display_name {
            Some(name) => Some(validate_display_name(name)?),
            None => None,
        };
        let role_change = update
            .role
            .filter(|r| Some(*r) != AdminRole::parse(&target.role));
        let active_change = update.active.filter(|a| *a != target.active);
        if target.id == actor.admin_id && (role_change.is_some() || active_change == Some(false)) {
            return Err(AurixError::Validation(
                "You cannot change your own role or deactivate yourself".into(),
            ));
        }
        let loses_superadmin = target.active
            && target.role == AdminRole::Superadmin.as_str()
            && (active_change == Some(false)
                || role_change.is_some_and(|r| r != AdminRole::Superadmin));
        if loses_superadmin {
            let superadmins = aurix_db::queries::count_active_admins_with_role(
                &self.pool,
                AdminRole::Superadmin.as_str(),
            )
            .await
            .map_err(|e| AurixError::Database(format!("Admin count failed: {e}")))?;
            if superadmins <= 1 {
                return Err(AurixError::Conflict(
                    "Cannot remove the last active superadmin".into(),
                ));
            }
        }
        if active_change == Some(true) {
            // Reactivation must not collide with an active account created for the same email.
            self.ensure_email_free(&target.email).await?;
        }
        let revoke = role_change.is_some() || active_change == Some(false);
        aurix_db::queries::update_admin_user(
            &self.pool,
            target_id,
            role_change.map(|r| r.as_str()),
            display_name.as_deref(),
            active_change,
            revoke,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Admin update failed: {e}")))?
        .ok_or_else(|| AurixError::NotFound("Administrator not found".into()))
    }

    /// Own password change: requires the current password; revokes every earlier token.
    pub async fn change_own_password(
        &self,
        admin_id: Uuid,
        current_password: &str,
        new_password: &str,
    ) -> Result<()> {
        let admin = self.get_admin(admin_id).await?;
        if !admin.active {
            return Err(AurixError::AuthenticationFailed(
                "Admin account is disabled".into(),
            ));
        }
        if admin.password_hash == NO_PASSWORD {
            return Err(AurixError::Validation(
                "This account signs in through SSO and has no password".into(),
            ));
        }
        self.verify_password(current_password, &admin.password_hash)
            .map_err(|_| AurixError::AuthenticationFailed("Current password is wrong".into()))?;
        validate_password(new_password)?;
        let hash = self.hash_password(new_password)?;
        aurix_db::queries::set_admin_password(&self.pool, admin_id, &hash)
            .await
            .map_err(|e| AurixError::Database(format!("Password update failed: {e}")))
    }

    /// Password reset by a superadmin; gives an SSO-only account a password too.
    pub async fn reset_password(&self, target_id: Uuid, new_password: &str) -> Result<()> {
        let target = self.get_admin(target_id).await?;
        validate_password(new_password)?;
        let hash = self.hash_password(new_password)?;
        aurix_db::queries::set_admin_password(&self.pool, target.id, &hash)
            .await
            .map_err(|e| AurixError::Database(format!("Password update failed: {e}")))
    }

    /// Invalidates every token issued to `admin_id` so far ("sign out everywhere").
    pub async fn revoke_tokens(&self, admin_id: Uuid) -> Result<()> {
        self.get_admin(admin_id).await?;
        aurix_db::queries::revoke_admin_tokens(&self.pool, admin_id)
            .await
            .map_err(|e| AurixError::Database(format!("Token revocation failed: {e}")))
    }

    async fn ensure_email_free(&self, email: &str) -> Result<()> {
        if aurix_db::queries::get_admin_by_email(&self.pool, email)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?
            .is_some()
        {
            return Err(AurixError::Conflict(
                "An admin with this email already exists".into(),
            ));
        }
        Ok(())
    }

    fn hash_password(&self, password: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| AurixError::Internal(format!("Password hashing failed: {e}")))
    }

    fn verify_password(&self, password: &str, hash: &str) -> Result<()> {
        let parsed = PasswordHash::new(hash)
            .map_err(|e| AurixError::Internal(format!("Invalid stored hash: {e}")))?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AurixError::AuthenticationFailed("Invalid credentials".into()))
    }
}

fn new_row(
    email: String,
    password_hash: String,
    display_name: String,
    role: AdminRole,
    source: AdminAuthSource,
    sso: Option<(String, String)>,
) -> AdminUserRow {
    let (sso_issuer, sso_subject) = match sso {
        Some((issuer, subject)) => (Some(issuer), Some(subject)),
        None => (None, None),
    };
    AdminUserRow {
        id: Uuid::now_v7(),
        email,
        password_hash,
        display_name,
        role: role.as_str().to_string(),
        active: true,
        last_login_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        auth_source: source.as_str().to_string(),
        sso_issuer,
        sso_subject,
        token_generation: 0,
        tokens_revoked_at: None,
    }
}

pub fn normalize_email(email: &str) -> Result<String> {
    let email = email.trim().to_ascii_lowercase();
    let at = email.find('@');
    let valid = email.len() >= 3
        && email.len() <= 254
        && at.is_some_and(|i| i > 0 && i + 1 < email.len())
        && !email.chars().any(|c| c.is_whitespace() || c.is_control());
    if !valid {
        return Err(AurixError::Validation("Invalid email address".into()));
    }
    Ok(email)
}

fn validate_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AurixError::Validation(format!(
            "Password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if password.len() > 1024 {
        return Err(AurixError::Validation("Password too long".into()));
    }
    Ok(())
}

fn validate_display_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err(AurixError::Validation(
            "Display name must be 1..=120 printable characters".into(),
        ));
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_service(password_login: bool) -> AdminAuthService {
        // Never connects: every test here fails or succeeds before touching the database.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://aurix:unused@127.0.0.1:1/aurix")
            .expect("lazy pool");
        AdminAuthService::new(pool, "unit-test-admin-secret-0123456789".into())
            .with_password_login(password_login)
    }

    fn row(generation: i64) -> AdminUserRow {
        let mut row = new_row(
            "ops@example.com".into(),
            NO_PASSWORD.into(),
            "Ops".into(),
            AdminRole::Admin,
            AdminAuthSource::Oidc,
            Some(("https://idp.example.com".into(), "sub-1".into())),
        );
        row.token_generation = generation;
        row
    }

    #[tokio::test]
    async fn disabled_password_login_is_refused_before_any_lookup() {
        let svc = lazy_service(false);
        assert!(!svc.password_login_enabled());
        let err = svc
            .authenticate("ops@example.com", "whatever-password")
            .await
            .unwrap_err();
        assert!(
            matches!(err, AurixError::AuthenticationFailed(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn tokens_carry_the_generation_they_were_issued_under() {
        let svc = lazy_service(true);
        let now = Utc::now().timestamp();
        let fresh = svc.issue_token(&row(0), AdminAuthSource::Password).unwrap();
        let claims = svc.decode_token(&fresh).unwrap();
        assert!((claims.iat - now).abs() <= 1);
        assert_eq!(claims.generation, 0);
        assert_eq!(claims.src, Some(AdminAuthSource::Password));
        assert_eq!(claims.exp - claims.iat, svc.token_ttl_secs());

        let later = svc.issue_token(&row(7), AdminAuthSource::Oidc).unwrap();
        let claims = svc.decode_token(&later).unwrap();
        assert_eq!(claims.generation, 7);
        assert_eq!(claims.src, Some(AdminAuthSource::Oidc));

        // Pre-upgrade tokens have no `gen` claim and are treated as generation 0.
        let mut legacy: serde_json::Value = serde_json::to_value(&claims).unwrap();
        legacy.as_object_mut().unwrap().remove("gen");
        legacy.as_object_mut().unwrap().remove("src");
        let legacy = encode(
            &Header::new(Algorithm::HS256),
            &legacy,
            &EncodingKey::from_secret(svc.jwt_secret.as_bytes()),
        )
        .unwrap();
        let claims = svc.decode_token(&legacy).unwrap();
        assert_eq!(claims.generation, 0);
        assert_eq!(claims.src, None);
    }

    #[tokio::test]
    async fn admin_tokens_reject_foreign_types_and_secrets() {
        let svc = lazy_service(true);
        let token = svc.issue_token(&row(0), AdminAuthSource::Password).unwrap();
        let other = AdminAuthService::new(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://aurix:unused@127.0.0.1:1/aurix")
                .unwrap(),
            "a-different-secret-0123456789abcdef".into(),
        );
        assert!(matches!(
            other.decode_token(&token).unwrap_err(),
            AurixError::TokenInvalid(_)
        ));
        let mut user_claims = svc.decode_token(&token).unwrap();
        user_claims.typ = "aurix-user".into();
        let forged = encode(
            &Header::new(Algorithm::HS256),
            &user_claims,
            &EncodingKey::from_secret(svc.jwt_secret.as_bytes()),
        )
        .unwrap();
        assert!(matches!(
            svc.decode_token(&forged).unwrap_err(),
            AurixError::TokenInvalid(_)
        ));
    }

    #[test]
    fn email_normalisation() {
        assert_eq!(
            normalize_email("  Ops@Example.COM ").unwrap(),
            "ops@example.com"
        );
        assert!(normalize_email("nobody").is_err());
        assert!(normalize_email("@example.com").is_err());
        assert!(normalize_email("ops@").is_err());
        assert!(normalize_email("o ps@example.com").is_err());
    }

    #[test]
    fn password_and_name_rules() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("long-enough-password").is_ok());
        assert!(validate_display_name("  ").is_err());
        assert_eq!(validate_display_name(" Ops ").unwrap(), "Ops");
        assert!(validate_display_name("a\u{7}b").is_err());
    }
}
