use aurix_common::config::AuthConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub user_id: String,
    pub app_id: String,
    pub display_name: String,
    #[serde(default)]
    pub channels: Vec<ChannelPermClaim>,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelPermClaim {
    pub channel_id: String,
    pub join: bool,
    pub speak: bool,
    pub receive: bool,
    pub moderate: bool,
}

#[derive(Debug, Clone)]
pub struct ValidatedToken {
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub channels: Vec<ChannelPermission>,
    pub metadata: Option<serde_json::Value>,
    pub jti: String,
}

pub struct JwtService {
    encoding_key: Option<EncodingKey>,
    decoding_key: DecodingKey,
    algorithm: Algorithm,
    token_ttl: i64,
}

impl JwtService {
    pub fn new(config: &AuthConfig) -> Result<Self> {
        let algorithm = match config.jwt_algorithm.as_str() {
            "HS256" => Algorithm::HS256,
            "HS384" => Algorithm::HS384,
            "HS512" => Algorithm::HS512,
            "RS256" => Algorithm::RS256,
            "RS384" => Algorithm::RS384,
            "RS512" => Algorithm::RS512,
            "ES256" => Algorithm::ES256,
            "ES384" => Algorithm::ES384,
            other => {
                return Err(AurixError::InvalidConfiguration(format!(
                    "Unsupported JWT algorithm: {other}"
                )))
            }
        };

        let (encoding_key, decoding_key) = match algorithm {
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
                let secret = config.jwt_secret.as_bytes();
                (
                    Some(EncodingKey::from_secret(secret)),
                    DecodingKey::from_secret(secret),
                )
            }
            Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
                let pub_key_path = config
                    .jwt_public_key_path
                    .as_ref()
                    .ok_or_else(|| {
                        AurixError::InvalidConfiguration(
                            "RSA requires jwt_public_key_path".into(),
                        )
                    })?;
                let pub_pem = std::fs::read(pub_key_path).map_err(|e| {
                    AurixError::InvalidConfiguration(format!("Cannot read public key: {e}"))
                })?;
                let decoding = DecodingKey::from_rsa_pem(&pub_pem).map_err(|e| {
                    AurixError::InvalidConfiguration(format!("Invalid RSA public key: {e}"))
                })?;
                let encoding = config.jwt_private_key_path.as_ref().map(|path| {
                    let priv_pem = std::fs::read(path).expect("Cannot read RSA private key");
                    EncodingKey::from_rsa_pem(&priv_pem).expect("Invalid RSA private key PEM")
                });
                (encoding, decoding)
            }
            Algorithm::ES256 | Algorithm::ES384 => {
                let pub_key_path = config
                    .jwt_public_key_path
                    .as_ref()
                    .ok_or_else(|| {
                        AurixError::InvalidConfiguration(
                            "ECDSA requires jwt_public_key_path".into(),
                        )
                    })?;
                let pub_pem = std::fs::read(pub_key_path).map_err(|e| {
                    AurixError::InvalidConfiguration(format!("Cannot read EC public key: {e}"))
                })?;
                let decoding = DecodingKey::from_ec_pem(&pub_pem).map_err(|e| {
                    AurixError::InvalidConfiguration(format!("Invalid EC public key: {e}"))
                })?;
                let encoding = config.jwt_private_key_path.as_ref().map(|path| {
                    let priv_pem = std::fs::read(path).expect("Cannot read EC private key");
                    EncodingKey::from_ec_pem(&priv_pem).expect("Invalid EC private key PEM")
                });
                (encoding, decoding)
            }
            _ => {
                return Err(AurixError::InvalidConfiguration(
                    "Unsupported algorithm variant".into(),
                ))
            }
        };

        Ok(Self {
            encoding_key,
            decoding_key,
            algorithm,
            token_ttl: config.token_ttl_secs,
        })
    }

    pub fn generate_token(
        &self,
        user_id: UserId,
        app_id: AppId,
        display_name: &str,
        channels: Vec<ChannelPermission>,
        metadata: Option<serde_json::Value>,
    ) -> Result<String> {
        let encoding_key = self
            .encoding_key
            .as_ref()
            .ok_or_else(|| {
                AurixError::AuthenticationFailed("No encoding key configured for token generation".into())
            })?;

        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: user_id.0.to_string(),
            user_id: user_id.0.to_string(),
            app_id: app_id.0.to_string(),
            display_name: display_name.to_string(),
            channels: channels
                .iter()
                .map(|c| ChannelPermClaim {
                    channel_id: c.channel_id.0.to_string(),
                    join: c.join,
                    speak: c.speak,
                    receive: c.receive,
                    moderate: c.moderate,
                })
                .collect(),
            exp: now + self.token_ttl,
            iat: now,
            jti: Uuid::now_v7().to_string(),
            metadata,
        };

        let header = Header::new(self.algorithm);
        encode(&header, &claims, encoding_key)
            .map_err(|e| AurixError::Internal(format!("Token encoding failed: {e}")))
    }

    pub fn validate_token(&self, token: &str) -> Result<ValidatedToken> {
        let mut validation = Validation::new(self.algorithm);
        validation.set_required_spec_claims(&["exp", "sub", "iat"]);
        validation.validate_exp = true;
        validation.leeway = 30;

        let token_data = decode::<Claims>(token, &self.decoding_key, &validation).map_err(
            |e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AurixError::TokenExpired,
                jsonwebtoken::errors::ErrorKind::InvalidSignature => {
                    AurixError::TokenInvalid("Invalid signature".into())
                }
                jsonwebtoken::errors::ErrorKind::InvalidToken => {
                    AurixError::TokenInvalid("Malformed token".into())
                }
                _ => AurixError::TokenInvalid(format!("Token validation failed: {e}")),
            },
        )?;

        let claims = token_data.claims;
        let user_uuid = Uuid::parse_str(&claims.user_id)
            .map_err(|_| AurixError::TokenInvalid("Invalid user_id in token".into()))?;
        let app_uuid = Uuid::parse_str(&claims.app_id)
            .map_err(|_| AurixError::TokenInvalid("Invalid app_id in token".into()))?;

        let channels = claims
            .channels
            .iter()
            .filter_map(|c| {
                Uuid::parse_str(&c.channel_id).ok().map(|ch_uuid| ChannelPermission {
                    channel_id: ChannelId::from_uuid(ch_uuid),
                    join: c.join,
                    speak: c.speak,
                    receive: c.receive,
                    moderate: c.moderate,
                })
            })
            .collect();

        Ok(ValidatedToken {
            user_id: UserId::from_uuid(user_uuid),
            app_id: AppId::from_uuid(app_uuid),
            display_name: claims.display_name,
            channels,
            metadata: claims.metadata,
            jti: claims.jti,
        })
    }
}