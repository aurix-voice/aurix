use thiserror::Error;

#[derive(Error, Debug)]
pub enum AurixError {
    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("Authorization denied: {0}")]
    AuthorizationDenied(String),

    #[error("Token expired")]
    TokenExpired,

    #[error("Token invalid: {0}")]
    TokenInvalid(String),

    #[error("Channel not found: {0}")]
    ChannelNotFound(String),

    #[error("Channel full: {0}")]
    ChannelFull(String),

    #[error("User not found: {0}")]
    UserNotFound(String),

    #[error("User banned: {0}")]
    UserBanned(String),

    #[error("User muted: {0}")]
    UserMuted(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Media node unavailable: {0}")]
    MediaNodeUnavailable(String),

    #[error("Rate limit exceeded: {0}")]
    RateLimitExceeded(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Redis error: {0}")]
    Redis(String),

    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Codec error: {0}")]
    Codec(String),

    #[error("STUN/TURN error: {0}")]
    StunTurn(String),

    #[error("Encryption error: {0}")]
    Encryption(String),

    #[error("Recording error: {0}")]
    Recording(String),

    #[error("Moderation error: {0}")]
    Moderation(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Not implemented: {0}")]
    NotImplemented(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Timeout: {0}")]
    Timeout(String),
}

impl AurixError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::AuthenticationFailed(_) => 401,
            Self::AuthorizationDenied(_) => 403,
            Self::TokenExpired => 401,
            Self::TokenInvalid(_) => 401,
            Self::ChannelNotFound(_) => 404,
            Self::UserNotFound(_) => 404,
            Self::SessionNotFound(_) => 404,
            Self::ChannelFull(_) => 409,
            Self::UserBanned(_) => 403,
            Self::UserMuted(_) => 403,
            Self::RateLimitExceeded(_) => 429,
            Self::InvalidConfiguration(_) => 400,
            Self::Validation(_) => 400,
            Self::Conflict(_) => 409,
            Self::Timeout(_) => 504,
            _ => 500,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::AuthenticationFailed(_) => "AUTH_FAILED",
            Self::AuthorizationDenied(_) => "AUTH_DENIED",
            Self::TokenExpired => "TOKEN_EXPIRED",
            Self::TokenInvalid(_) => "TOKEN_INVALID",
            Self::ChannelNotFound(_) => "CHANNEL_NOT_FOUND",
            Self::ChannelFull(_) => "CHANNEL_FULL",
            Self::UserNotFound(_) => "USER_NOT_FOUND",
            Self::UserBanned(_) => "USER_BANNED",
            Self::UserMuted(_) => "USER_MUTED",
            Self::SessionNotFound(_) => "SESSION_NOT_FOUND",
            Self::MediaNodeUnavailable(_) => "MEDIA_NODE_UNAVAILABLE",
            Self::RateLimitExceeded(_) => "RATE_LIMIT_EXCEEDED",
            Self::InvalidConfiguration(_) => "INVALID_CONFIG",
            Self::Database(_) => "DB_ERROR",
            Self::Redis(_) => "REDIS_ERROR",
            Self::Transport(_) => "TRANSPORT_ERROR",
            Self::Codec(_) => "CODEC_ERROR",
            Self::StunTurn(_) => "STUN_TURN_ERROR",
            Self::Encryption(_) => "ENCRYPTION_ERROR",
            Self::Recording(_) => "RECORDING_ERROR",
            Self::Moderation(_) => "MODERATION_ERROR",
            Self::Internal(_) => "INTERNAL_ERROR",
            Self::Validation(_) => "VALIDATION_ERROR",
            Self::NotImplemented(_) => "NOT_IMPLEMENTED",
            Self::Conflict(_) => "CONFLICT",
            Self::Timeout(_) => "TIMEOUT",
        }
    }
}

pub type Result<T> = std::result::Result<T, AurixError>;

impl From<sqlx::Error> for AurixError {
    fn from(e: sqlx::Error) -> Self {
        AurixError::Database(e.to_string())
    }
}