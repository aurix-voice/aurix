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

    #[error("Token already used")]
    TokenReused,

    #[error("Action token required: {0}")]
    ActionTokenRequired(String),

    #[error("Channel not found: {0}")]
    ChannelNotFound(String),

    #[error("Channel full: {0}")]
    ChannelFull(String),

    /// The session is already joined to as many channels as the node allows.
    #[error("Channel limit exceeded: {0}")]
    ChannelLimitExceeded(String),

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

    /// A per-application usage quota (concurrent sessions, monthly minutes) is exhausted.
    #[error("Quota exceeded: {0}")]
    QuotaExceeded(String),

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

    #[error("Not found: {0}")]
    NotFound(String),

    /// Text chat is disabled on this node.
    #[error("Chat disabled")]
    ChatDisabled,

    /// Message rejected by the content filter.
    #[error("Message blocked: {0}")]
    MessageBlocked(String),

    #[error("User is not online")]
    UserOffline,

    /// Text-to-speech is not configured on this node.
    #[error("Text-to-speech disabled")]
    TtsDisabled,

    /// The TTS provider failed or returned unusable audio.
    #[error("Text-to-speech failed: {0}")]
    Tts(String),

    /// The STT provider failed.
    #[error("Speech-to-text failed: {0}")]
    Stt(String),

    /// Live translation is not configured on this node.
    #[error("Translation disabled")]
    TranslationDisabled,

    /// The machine-translation provider failed.
    #[error("Translation failed: {0}")]
    Translation(String),

    /// The content-safety classifier failed.
    #[error("Safety classifier failed: {0}")]
    Safety(String),
}

impl AurixError {
    /// True for errors whose details describe server internals (DB, Redis, I/O…)
    /// and must never be echoed to API clients.
    pub fn is_internal(&self) -> bool {
        matches!(
            self,
            Self::Database(_)
                | Self::Redis(_)
                | Self::Transport(_)
                | Self::Codec(_)
                | Self::StunTurn(_)
                | Self::Encryption(_)
                | Self::Recording(_)
                | Self::Moderation(_)
                | Self::Internal(_)
                | Self::MediaNodeUnavailable(_)
                | Self::Tts(_)
                | Self::Stt(_)
                | Self::Translation(_)
                | Self::Safety(_)
        )
    }

    /// Message safe to return to clients.
    pub fn public_message(&self) -> String {
        if self.is_internal() {
            match self {
                Self::MediaNodeUnavailable(_) => "No media node is currently available".to_string(),
                Self::Recording(_) => "Recording operation failed".to_string(),
                Self::Moderation(_) => "Moderation operation failed".to_string(),
                Self::Tts(_) => "Text-to-speech failed".to_string(),
                Self::Stt(_) => "Speech-to-text failed".to_string(),
                Self::Translation(_) => "Translation failed".to_string(),
                Self::Safety(_) => "Safety classifier failed".to_string(),
                _ => "Internal server error".to_string(),
            }
        } else {
            self.to_string()
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            Self::AuthenticationFailed(_) => 401,
            Self::AuthorizationDenied(_) => 403,
            Self::TokenExpired => 401,
            Self::TokenInvalid(_) => 401,
            Self::TokenReused => 401,
            Self::ActionTokenRequired(_) => 403,
            Self::ChannelNotFound(_) => 404,
            Self::UserNotFound(_) => 404,
            Self::SessionNotFound(_) => 404,
            Self::ChannelFull(_) => 409,
            Self::ChannelLimitExceeded(_) => 409,
            Self::UserBanned(_) => 403,
            Self::UserMuted(_) => 403,
            Self::RateLimitExceeded(_) => 429,
            Self::QuotaExceeded(_) => 429,
            Self::InvalidConfiguration(_) => 400,
            Self::Validation(_) => 400,
            Self::Conflict(_) => 409,
            Self::Timeout(_) => 504,
            Self::NotFound(_) => 404,
            Self::NotImplemented(_) => 501,
            Self::MediaNodeUnavailable(_) => 503,
            Self::ChatDisabled => 404,
            Self::MessageBlocked(_) => 422,
            Self::UserOffline => 404,
            Self::TtsDisabled => 404,
            Self::Tts(_) => 502,
            Self::Stt(_) => 502,
            Self::TranslationDisabled => 404,
            Self::Translation(_) => 502,
            Self::Safety(_) => 502,
            _ => 500,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::AuthenticationFailed(_) => "AUTH_FAILED",
            Self::AuthorizationDenied(_) => "AUTH_DENIED",
            Self::TokenExpired => "TOKEN_EXPIRED",
            Self::TokenInvalid(_) => "TOKEN_INVALID",
            Self::TokenReused => "TOKEN_REUSED",
            Self::ActionTokenRequired(_) => "ACTION_TOKEN_REQUIRED",
            Self::ChannelNotFound(_) => "CHANNEL_NOT_FOUND",
            Self::ChannelFull(_) => "CHANNEL_FULL",
            Self::ChannelLimitExceeded(_) => "CHANNEL_LIMIT_EXCEEDED",
            Self::UserNotFound(_) => "USER_NOT_FOUND",
            Self::UserBanned(_) => "USER_BANNED",
            Self::UserMuted(_) => "USER_MUTED",
            Self::SessionNotFound(_) => "SESSION_NOT_FOUND",
            Self::MediaNodeUnavailable(_) => "MEDIA_NODE_UNAVAILABLE",
            Self::RateLimitExceeded(_) => "RATE_LIMIT_EXCEEDED",
            Self::QuotaExceeded(_) => "QUOTA_EXCEEDED",
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
            Self::NotFound(_) => "NOT_FOUND",
            Self::ChatDisabled => "CHAT_DISABLED",
            Self::MessageBlocked(_) => "MESSAGE_BLOCKED",
            Self::UserOffline => "USER_OFFLINE",
            Self::TtsDisabled => "TTS_DISABLED",
            Self::Tts(_) => "TTS_ERROR",
            Self::Stt(_) => "STT_ERROR",
            Self::TranslationDisabled => "TRANSLATION_DISABLED",
            Self::Translation(_) => "TRANSLATION_ERROR",
            Self::Safety(_) => "SAFETY_ERROR",
        }
    }
}

pub type Result<T> = std::result::Result<T, AurixError>;

#[cfg(feature = "server")]
impl From<sqlx::Error> for AurixError {
    fn from(e: sqlx::Error) -> Self {
        AurixError::Database(e.to_string())
    }
}
