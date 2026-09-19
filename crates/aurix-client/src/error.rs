use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ClientError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("client is not connected")]
    NotConnected,
    #[error("client is shutting down")]
    Closed,
    #[error("transport error: {0}")]
    Transport(String),
    #[error("authentication failed: {0}")]
    Unauthorized(String),
    #[error("timed out waiting for {0}")]
    Timeout(String),
    #[error("server rejected the request: {code}: {message}")]
    Server { code: String, message: String },
    #[error("audio codec error: {0}")]
    Codec(String),
    #[error("{0}")]
    Protocol(String),
}

impl From<opus::Error> for ClientError {
    fn from(e: opus::Error) -> Self {
        ClientError::Codec(e.to_string())
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::Protocol(format!("malformed control message: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;
