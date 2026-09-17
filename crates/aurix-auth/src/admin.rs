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

#[derive(Debug, Serialize, Deserialize)]
struct AdminClaims {
    sub: String,
    admin_id: String,
    email: String,
    role: String,
    exp: i64,
    iat: i64,
}

pub struct AdminAuthService {
    pool: DbPool,
    jwt_secret: String,
}

impl AdminAuthService {
    pub fn new(pool: DbPool, jwt_secret: String) -> Self {
        Self { pool, jwt_secret }
    }

    pub async fn create_admin(
        &self, email: &str, password: &str, display_name: &str, role: &str,
    ) -> Result<AdminUserRow> {
        let password_hash = self.hash_password(password)?;
        let admin = AdminUserRow {
            id: Uuid::now_v7(), email: email.to_string(), password_hash,
            display_name: display_name.to_string(), role: role.to_string(),
            active: true, last_login_at: None, created_at: Utc::now(), updated_at: Utc::now(),
        };
        aurix_db::queries::create_admin_user(&self.pool, &admin)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to create admin: {e}")))
    }

    pub async fn authenticate(&self, email: &str, password: &str) -> Result<(AdminUserRow, String)> {
        let admin = aurix_db::queries::get_admin_by_email(&self.pool, email)
            .await
            .map_err(|e| AurixError::Database(format!("Admin lookup failed: {e}")))?
            .ok_or_else(|| AurixError::AuthenticationFailed("Invalid credentials".into()))?;
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
            _ => AurixError::TokenInvalid(format!("Admin token invalid: {e}")),
        })?;

        let c = data.claims;
        Ok(AdminContext {
            admin_id: Uuid::parse_str(&c.admin_id)
                .map_err(|_| AurixError::TokenInvalid("Bad admin_id".into()))?,
            email: c.email,
            role: c.role,
        })
    }

    fn generate_admin_token(&self, admin: &AdminUserRow) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = AdminClaims {
            sub: admin.id.to_string(), admin_id: admin.id.to_string(),
            email: admin.email.clone(), role: admin.role.clone(),
            exp: now + 86400, iat: now,
        };
        encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(self.jwt_secret.as_bytes()))
            .map_err(|e| AurixError::Internal(format!("Admin token generation failed: {e}")))
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