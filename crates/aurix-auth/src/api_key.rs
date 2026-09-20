use aurix_common::error::{AurixError, Result};
use aurix_db::models::ApiKeyRow;
use aurix_db::DbPool;
use base64::Engine;
use chrono::Utc;
use rand::Rng;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub struct ApiKeyService {
    pool: DbPool,
}

impl ApiKeyService {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn create_key(
        &self,
        app_id: Uuid,
        name: &str,
        permissions: serde_json::Value,
        rate_limit: i32,
        expires_at: Option<chrono::DateTime<Utc>>,
    ) -> Result<(ApiKeyRow, String)> {
        let raw_key = self.generate_raw_key();
        let prefix = &raw_key[..12];
        let hash = self.hash_key(&raw_key);

        let row = ApiKeyRow {
            id: Uuid::now_v7(),
            app_id,
            name: name.to_string(),
            key_prefix: prefix.to_string(),
            key_hash: hash,
            permissions,
            rate_limit,
            active: true,
            last_used_at: None,
            expires_at,
            created_at: Utc::now(),
            revoked_at: None,
        };

        let created = aurix_db::queries::create_api_key(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to create API key: {e}")))?;

        Ok((created, raw_key))
    }

    pub async fn validate_key(&self, raw_key: &str) -> Result<ApiKeyRow> {
        if raw_key.len() < 12 {
            return Err(AurixError::AuthenticationFailed(
                "Invalid API key format".into(),
            ));
        }

        let prefix = &raw_key[..12];
        let key_row = aurix_db::queries::get_api_key_by_prefix(&self.pool, prefix)
            .await
            .map_err(|e| AurixError::Database(format!("API key lookup failed: {e}")))?
            .ok_or_else(|| AurixError::AuthenticationFailed("API key not found".into()))?;

        let hash = self.hash_key(raw_key);
        if !aurix_common::crypto::constant_time_eq(hash.as_bytes(), key_row.key_hash.as_bytes()) {
            return Err(AurixError::AuthenticationFailed(
                "API key validation failed".into(),
            ));
        }

        if !key_row.active || key_row.revoked_at.is_some() {
            return Err(AurixError::AuthenticationFailed(
                "API key has been revoked".into(),
            ));
        }

        if let Some(expires) = key_row.expires_at {
            if expires <= Utc::now() {
                return Err(AurixError::AuthenticationFailed(
                    "API key has expired".into(),
                ));
            }
        }

        let _ = aurix_db::queries::touch_api_key(&self.pool, key_row.id).await;
        Ok(key_row)
    }

    /// Revoke a key owned by `app_id`. Keys of other tenants are reported as not found.
    pub async fn revoke_key(&self, app_id: Uuid, key_id: Uuid) -> Result<()> {
        let affected = aurix_db::queries::revoke_api_key(&self.pool, app_id, key_id)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to revoke API key: {e}")))?;
        if affected == 0 {
            return Err(AurixError::Validation("API key not found".into()));
        }
        Ok(())
    }

    /// Changes the per-key request budget (requests per minute, `0` = unlimited) of an active
    /// key owned by `app_id`.
    pub async fn set_rate_limit(
        &self,
        app_id: Uuid,
        key_id: Uuid,
        rate_limit: i32,
    ) -> Result<ApiKeyRow> {
        aurix_db::queries::update_api_key_rate_limit(&self.pool, app_id, key_id, rate_limit)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to update API key: {e}")))?
            .ok_or_else(|| AurixError::NotFound("API key not found".into()))
    }

    pub async fn list_keys(&self, app_id: Uuid) -> Result<Vec<ApiKeyRow>> {
        aurix_db::queries::list_api_keys(&self.pool, app_id)
            .await
            .map_err(|e| AurixError::Database(format!("Failed to list API keys: {e}")))
    }

    /// Whether the key grants `permission` (e.g. `"channels:write"`). Permissions are stored as a
    /// JSON array of strings; `"*"` grants everything.
    pub fn has_permission(key: &ApiKeyRow, permission: &str) -> bool {
        match key.permissions.as_array() {
            Some(arr) => arr
                .iter()
                .any(|p| p.as_str() == Some("*") || p.as_str() == Some(permission)),
            None => false,
        }
    }

    fn generate_raw_key(&self) -> String {
        let mut rng = rand::thread_rng();
        let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
        format!(
            "aurx_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
        )
    }

    fn hash_key(&self, key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let result = hasher.finalize();
        result.iter().map(|b| format!("{b:02x}")).collect()
    }
}
