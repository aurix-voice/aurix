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

/// Payload of a one-time action token. Distinguished from session tokens by `typ`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionClaims {
    pub typ: String,
    pub act: ActionKind,
    pub sub: String,
    pub app_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_user_id: Option<String>,
    #[serde(default)]
    pub speak: bool,
    #[serde(default)]
    pub receive: bool,
    #[serde(default)]
    pub moderate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
}

pub const ACTION_TOKEN_TYP: &str = "aurix/action";

/// What to mint with [`JwtService::generate_action_token`].
#[derive(Debug, Clone)]
pub struct ActionTokenSpec {
    pub action: ActionKind,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub channel_id: Option<ChannelId>,
    pub target_user_id: Option<UserId>,
    pub speak: bool,
    pub receive: bool,
    pub moderate: bool,
    pub metadata: Option<serde_json::Value>,
    pub ttl_secs: i64,
}

#[derive(Debug, Clone)]
pub struct ValidatedActionToken {
    pub action: ActionKind,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub channel_id: Option<ChannelId>,
    pub target_user_id: Option<UserId>,
    pub speak: bool,
    pub receive: bool,
    pub moderate: bool,
    pub metadata: Option<serde_json::Value>,
    pub jti: String,
    pub exp: i64,
}

impl ValidatedActionToken {
    /// Channel permission a `join` token grants; `None` for other actions.
    pub fn channel_permission(&self) -> Option<ChannelPermission> {
        if self.action != ActionKind::Join {
            return None;
        }
        Some(ChannelPermission {
            channel_id: self.channel_id?,
            join: true,
            speak: self.speak,
            receive: self.receive,
            moderate: self.moderate,
        })
    }

    /// Session-shaped view of a `login` token (no channel rights of its own).
    pub fn as_session(&self) -> ValidatedToken {
        ValidatedToken {
            user_id: self.user_id,
            app_id: self.app_id,
            display_name: self.display_name.clone(),
            channels: Vec::new(),
            metadata: self.metadata.clone(),
            jti: self.jti.clone(),
        }
    }
}

/// Either kind of end-user token the server accepts.
#[derive(Debug, Clone)]
pub enum AnyToken {
    Session(ValidatedToken),
    Action(ValidatedActionToken),
}

/// Only the discriminator; used to route a token to the right validator.
#[derive(Deserialize)]
struct TypClaim {
    #[serde(default)]
    typ: Option<String>,
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
                let pub_key_path = config.jwt_public_key_path.as_ref().ok_or_else(|| {
                    AurixError::InvalidConfiguration("RSA requires jwt_public_key_path".into())
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
                let pub_key_path = config.jwt_public_key_path.as_ref().ok_or_else(|| {
                    AurixError::InvalidConfiguration("ECDSA requires jwt_public_key_path".into())
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
        let encoding_key = self.encoding_key.as_ref().ok_or_else(|| {
            AurixError::AuthenticationFailed(
                "No encoding key configured for token generation".into(),
            )
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
        if Self::is_action_token(token) {
            return Err(AurixError::TokenInvalid(
                "Action token presented where a session token is required".into(),
            ));
        }
        let token_data = self.decode_verified::<Claims>(token)?;

        let claims = token_data.claims;
        let user_uuid = Uuid::parse_str(&claims.user_id)
            .map_err(|_| AurixError::TokenInvalid("Invalid user_id in token".into()))?;
        let app_uuid = Uuid::parse_str(&claims.app_id)
            .map_err(|_| AurixError::TokenInvalid("Invalid app_id in token".into()))?;

        let channels = claims
            .channels
            .iter()
            .filter_map(|c| {
                Uuid::parse_str(&c.channel_id)
                    .ok()
                    .map(|ch_uuid| ChannelPermission {
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

    /// Verifies signature and expiry of either token kind and tells them apart.
    pub fn validate_any(&self, token: &str) -> Result<AnyToken> {
        if Self::is_action_token(token) {
            self.validate_action_token(token).map(AnyToken::Action)
        } else {
            self.validate_token(token).map(AnyToken::Session)
        }
    }

    /// Cheap, unverified peek at the payload discriminator; every caller verifies afterwards.
    pub fn is_action_token(token: &str) -> bool {
        use base64::Engine;
        let Some(payload) = token.split('.').nth(1) else {
            return false;
        };
        let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
            return false;
        };
        serde_json::from_slice::<TypClaim>(&bytes)
            .ok()
            .and_then(|c| c.typ)
            .is_some_and(|t| t == ACTION_TOKEN_TYP)
    }

    pub fn generate_action_token(&self, spec: &ActionTokenSpec) -> Result<(String, String, i64)> {
        let encoding_key = self.encoding_key.as_ref().ok_or_else(|| {
            AurixError::AuthenticationFailed(
                "No encoding key configured for token generation".into(),
            )
        })?;
        if spec.action.needs_channel() && spec.channel_id.is_none() {
            return Err(AurixError::Validation(format!(
                "action '{}' requires channel_id",
                spec.action
            )));
        }
        if spec.action.needs_target() && spec.target_user_id.is_none() {
            return Err(AurixError::Validation(format!(
                "action '{}' requires target_user_id",
                spec.action
            )));
        }
        let now = Utc::now().timestamp();
        let jti = Uuid::now_v7().to_string();
        let exp = now + spec.ttl_secs.max(1);
        let claims = ActionClaims {
            typ: ACTION_TOKEN_TYP.into(),
            act: spec.action,
            sub: spec.user_id.0.to_string(),
            app_id: spec.app_id.0.to_string(),
            display_name: spec.display_name.clone(),
            channel_id: spec.channel_id.map(|c| c.0.to_string()),
            target_user_id: spec.target_user_id.map(|u| u.0.to_string()),
            speak: spec.speak,
            receive: spec.receive,
            moderate: spec.moderate,
            metadata: spec.metadata.clone(),
            exp,
            iat: now,
            jti: jti.clone(),
        };
        let token = encode(&Header::new(self.algorithm), &claims, encoding_key)
            .map_err(|e| AurixError::Internal(format!("Token encoding failed: {e}")))?;
        Ok((token, jti, exp))
    }

    /// Signature/expiry check of an action token. Single-use enforcement (`jti`) is the
    /// caller's job (see `aurix-control::ActionTokenService`).
    pub fn validate_action_token(&self, token: &str) -> Result<ValidatedActionToken> {
        let claims = self.decode_verified::<ActionClaims>(token)?.claims;
        if claims.typ != ACTION_TOKEN_TYP {
            return Err(AurixError::TokenInvalid("Not an action token".into()));
        }
        let parse = |raw: &str, what: &str| {
            Uuid::parse_str(raw)
                .map_err(|_| AurixError::TokenInvalid(format!("Invalid {what} in token")))
        };
        let user_id = UserId::from_uuid(parse(&claims.sub, "sub")?);
        let app_id = AppId::from_uuid(parse(&claims.app_id, "app_id")?);
        let channel_id = match &claims.channel_id {
            Some(c) => Some(ChannelId::from_uuid(parse(c, "channel_id")?)),
            None => None,
        };
        let target_user_id = match &claims.target_user_id {
            Some(t) => Some(UserId::from_uuid(parse(t, "target_user_id")?)),
            None => None,
        };
        if claims.act.needs_channel() && channel_id.is_none() {
            return Err(AurixError::TokenInvalid(
                "Action token lacks channel_id".into(),
            ));
        }
        if claims.act.needs_target() && target_user_id.is_none() {
            return Err(AurixError::TokenInvalid(
                "Action token lacks target_user_id".into(),
            ));
        }
        if claims.jti.is_empty() {
            return Err(AurixError::TokenInvalid("Action token lacks jti".into()));
        }
        Ok(ValidatedActionToken {
            action: claims.act,
            user_id,
            app_id,
            display_name: claims.display_name,
            channel_id,
            target_user_id,
            speak: claims.speak,
            receive: claims.receive,
            moderate: claims.moderate,
            metadata: claims.metadata,
            jti: claims.jti,
            exp: claims.exp,
        })
    }

    fn decode_verified<T: serde::de::DeserializeOwned>(
        &self,
        token: &str,
    ) -> Result<jsonwebtoken::TokenData<T>> {
        let mut validation = Validation::new(self.algorithm);
        validation.set_required_spec_claims(&["exp", "sub", "iat"]);
        validation.validate_exp = true;
        validation.leeway = 30;
        decode::<T>(token, &self.decoding_key, &validation).map_err(|e| match e.kind() {
            jsonwebtoken::errors::ErrorKind::ExpiredSignature => AurixError::TokenExpired,
            jsonwebtoken::errors::ErrorKind::InvalidSignature => {
                AurixError::TokenInvalid("Invalid signature".into())
            }
            jsonwebtoken::errors::ErrorKind::InvalidToken => {
                AurixError::TokenInvalid("Malformed token".into())
            }
            _ => {
                tracing::debug!("token rejected: {e}");
                AurixError::TokenInvalid("Token validation failed".into())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> JwtService {
        let cfg = AuthConfig {
            jwt_secret: "unit-test-secret-0123456789abcdef0123456789".into(),
            ..Default::default()
        };
        JwtService::new(&cfg).unwrap()
    }

    fn spec(action: ActionKind) -> ActionTokenSpec {
        ActionTokenSpec {
            action,
            user_id: UserId::new(),
            app_id: AppId::new(),
            display_name: "Alice".into(),
            channel_id: Some(ChannelId::new()),
            target_user_id: Some(UserId::new()),
            speak: true,
            receive: true,
            moderate: false,
            metadata: None,
            ttl_secs: 90,
        }
    }

    #[test]
    fn action_tokens_round_trip_and_are_told_apart_from_session_tokens() {
        let s = svc();
        let spec = spec(ActionKind::Join);
        let (token, jti, exp) = s.generate_action_token(&spec).unwrap();
        assert!(JwtService::is_action_token(&token));
        let v = s.validate_action_token(&token).unwrap();
        assert_eq!(v.action, ActionKind::Join);
        assert_eq!(v.user_id, spec.user_id);
        assert_eq!(v.app_id, spec.app_id);
        assert_eq!(v.channel_id, spec.channel_id);
        assert_eq!(v.jti, jti);
        assert_eq!(v.exp, exp);
        let perm = v.channel_permission().unwrap();
        assert!(perm.join && perm.speak && perm.receive && !perm.moderate);
        // An action token is not a session token and vice versa.
        assert!(matches!(
            s.validate_token(&token),
            Err(AurixError::TokenInvalid(_))
        ));
        let session = s
            .generate_token(spec.user_id, spec.app_id, "Alice", vec![], None)
            .unwrap();
        assert!(!JwtService::is_action_token(&session));
        assert!(matches!(
            s.validate_action_token(&session),
            Err(AurixError::TokenInvalid(_))
        ));
        assert!(matches!(s.validate_any(&session), Ok(AnyToken::Session(_))));
        assert!(matches!(s.validate_any(&token), Ok(AnyToken::Action(_))));
    }

    #[test]
    fn action_tokens_require_the_fields_their_action_needs() {
        let s = svc();
        let mut sp = spec(ActionKind::Kick);
        sp.target_user_id = None;
        assert!(matches!(
            s.generate_action_token(&sp),
            Err(AurixError::Validation(_))
        ));
        let mut sp = spec(ActionKind::Join);
        sp.channel_id = None;
        assert!(matches!(
            s.generate_action_token(&sp),
            Err(AurixError::Validation(_))
        ));
        let mut sp = spec(ActionKind::Login);
        sp.channel_id = None;
        sp.target_user_id = None;
        let (tok, _, _) = s.generate_action_token(&sp).unwrap();
        let v = s.validate_action_token(&tok).unwrap();
        assert!(v.channel_permission().is_none());
        assert!(v.as_session().channels.is_empty());
    }

    #[test]
    fn tampered_or_foreign_key_action_tokens_are_rejected() {
        let s = svc();
        let (tok, _, _) = s.generate_action_token(&spec(ActionKind::Mute)).unwrap();
        let other = AuthConfig {
            jwt_secret: "another-secret-0123456789abcdef0123456789".into(),
            ..Default::default()
        };
        let other = JwtService::new(&other).unwrap();
        assert!(matches!(
            other.validate_action_token(&tok),
            Err(AurixError::TokenInvalid(_))
        ));
        let mut parts: Vec<&str> = tok.split('.').collect();
        let sig = parts[2].to_string();
        let flipped = if sig.ends_with('A') {
            format!("{}B", &sig[..sig.len() - 1])
        } else {
            format!("{}A", &sig[..sig.len() - 1])
        };
        parts[2] = &flipped;
        assert!(s.validate_action_token(&parts.join(".")).is_err());
    }
}
