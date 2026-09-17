use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use aurix_common::error::{AurixError, Result};
use aurix_common::types::AdminContext;
use aurix_db::models::AdminUserRow;
use aurix_db::DbPool;
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const ADMIN_TOKEN_TYPE: &str = "aurix-admin";
const MIN_PASSWORD_LEN: usize = 12;
pub const ADMIN_ROLES: &[&str] = &["superadmin", "admin", "moderator", "viewer"];

#[derive(Debug, Serialize, Deserialize)]
struct AdminClaims {
    sub: String,
    admin_id: String,
    email: String,
    role: String,
    /// Distinguishes admin tokens from end-user tokens that may share the signing secret.
    typ: String,
    exp: i64,
    iat: i64,
}

/// Valid Argon2id hash of a random password, used to equalise timing for unknown emails.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Zm9vYmFyYmF6cXV4cXV1eGZvb2JhcmJhenF1eHF1dXg";

pub struct AdminAuthService {
    pool: DbPool,
    jwt_secret: String,
    token_ttl_secs: i64,
    bootstrap_token: Option<String>,
}

impl AdminAuthService {
    pub fn new(pool: DbPool, jwt_secret: String) -> Self {
        Self {
            pool,
            jwt_secret,
            token_ttl_secs: 8 * 3600,
            bootstrap_token: None,
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
        self.create_admin(email, password, display_name, "superadmin")
            .await
    }

    pub async fn create_admin(
        &self,
        email: &str,
        password: &str,
        display_name: &str,
        role: &str,
    ) -> Result<AdminUserRow> {
        let email = email.trim().to_ascii_lowercase();
        if email.len() < 3 || !email.contains('@') || email.len() > 254 {
            return Err(AurixError::Validation("Invalid email address".into()));
        }
        if password.len() < MIN_PASSWORD_LEN {
            return Err(AurixError::Validation(format!(
                "Password must be at least {MIN_PASSWORD_LEN} characters"
            )));
        }
        if !ADMIN_ROLES.contains(&role) {
            return Err(AurixError::Validation(format!(
                "Unknown admin role '{role}'"
            )));
        }
        if aurix_db::queries::get_admin_by_email(&self.pool, &email)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?
            .is_some()
        {
            return Err(AurixError::Conflict(
                "An admin with this email already exists".into(),
            ));
        }
        let password_hash = self.hash_password(password)?;
        let admin = AdminUserRow {
            id: Uuid::now_v7(),
            email,
            password_hash,
            display_name: display_name.to_string(),
            role: role.to_string(),
            active: true,
            last_login_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        aurix_db::queries::create_admin_user(&self.pool, &admin)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to create admin: {e}")))
    }

    pub async fn authenticate(
        &self,
        email: &str,
        password: &str,
    ) -> Result<(AdminUserRow, String)> {
        let email = email.trim().to_ascii_lowercase();
        let admin = aurix_db::queries::get_admin_by_email(&self.pool, &email)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?;
        // Always run a password verification so response timing does not reveal whether
        // the email exists.
        let Some(admin) = admin else {
            let _ = self.verify_password(password, DUMMY_HASH);
            return Err(AurixError::AuthenticationFailed(
                "Invalid credentials".into(),
            ));
        };
        self.verify_password(password, &admin.password_hash)?;
        let _ = aurix_db::queries::update_admin_login(&self.pool, admin.id).await;
        let token = self.generate_admin_token(&admin)?;
        Ok((admin, token))
    }

    /// Validate an admin JWT and return the admin context.
    pub fn validate_admin_token(&self, token: &str) -> Result<AdminContext> {
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

        let c = data.claims;
        if c.typ != ADMIN_TOKEN_TYPE {
            return Err(AurixError::TokenInvalid("Not an admin token".into()));
        }
        let admin_id = Uuid::parse_str(&c.admin_id)
            .map_err(|_| AurixError::TokenInvalid("Bad admin_id".into()))?;
        Ok(AdminContext {
            admin_id,
            email: c.email,
            role: c.role,
        })
    }

    fn generate_admin_token(&self, admin: &AdminUserRow) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = AdminClaims {
            sub: admin.id.to_string(),
            admin_id: admin.id.to_string(),
            email: admin.email.clone(),
            role: admin.role.clone(),
            typ: ADMIN_TOKEN_TYPE.to_string(),
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

    /// Verify that the admin referenced by a token is still active (tokens outlive revocation).
    pub async fn ensure_active(&self, admin_id: Uuid) -> Result<AdminUserRow> {
        aurix_db::queries::get_admin_by_id(&self.pool, admin_id)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?
            .ok_or_else(|| AurixError::AuthenticationFailed("Admin account is disabled".into()))
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
