//! WebSocket control plane: authenticated upgrade (`Authorization: Bearer`, optional
//! `X-Aurix-Resume`), first-message `SessionInitAck` handshake and typed
//! [`ControlMessage`] send/receive. Connection policy (reconnects, re-joins) lives in
//! [`crate::client`].

use aurix_common::protocol::ControlMessage;
use aurix_common::types::{SessionId, UserId};
use base64::Engine;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::{ClientError, Result};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const RESUME_HEADER: &str = "x-aurix-resume";

/// `SessionInitAck` with the media key already decoded.
#[derive(Debug, Clone)]
pub struct SessionAck {
    pub session_id: SessionId,
    pub ssrc: u32,
    pub media_addr: String,
    pub media_key: Vec<u8>,
    pub resume_token: String,
    pub resume_grace: Duration,
    pub resumed: bool,
}

/// Informational (unverified) claims of the session JWT; lets the client recognise itself in
/// rosters. The server is the only party that validates the token.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenIdentity {
    pub user_id: Option<UserId>,
    pub display_name: Option<String>,
    pub expires_at_unix: Option<i64>,
}

#[derive(Deserialize)]
struct LooseClaims {
    #[serde(default)]
    user_id: Option<UserId>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    exp: Option<i64>,
}

/// Decode the payload segment of a JWT without verifying it.
pub fn token_identity(token: &str) -> TokenIdentity {
    let mut parts = token.split('.');
    let (Some(_), Some(payload)) = (parts.next(), parts.next()) else {
        return TokenIdentity::default();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return TokenIdentity::default();
    };
    match serde_json::from_slice::<LooseClaims>(&bytes) {
        Ok(c) => TokenIdentity {
            user_id: c.user_id,
            display_name: c.display_name,
            expires_at_unix: c.exp,
        },
        Err(_) => TokenIdentity::default(),
    }
}

/// Host part of the control URL, used when the server advertises an unspecified media host.
pub fn ws_host(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split(']').next().map(str::to_string);
    }
    Some(authority.split(':').next()?.to_string())
}

fn validate_ws_url(url: &str) -> Result<()> {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        Ok(())
    } else {
        Err(ClientError::InvalidArgument(format!(
            "ws_url must start with ws:// or wss:// (got `{url}`)"
        )))
    }
}

fn map_ws_error(e: WsError) -> ClientError {
    match e {
        WsError::Http(resp) => {
            let status = resp.status();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                ClientError::Unauthorized(format!("websocket upgrade rejected: {status}"))
            } else {
                ClientError::Transport(format!("websocket upgrade failed: {status}"))
            }
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => ClientError::NotConnected,
        other => ClientError::Transport(other.to_string()),
    }
}

/// One open control connection.
pub struct ControlConnection {
    sink: SplitSink<WsStream, Message>,
    stream: SplitStream<WsStream>,
}

impl ControlConnection {
    /// Open the WebSocket, authenticate and wait for `SessionInitAck`.
    ///
    /// `resume` is `(session_id, resume_token)` of a detached session; the server falls back to
    /// a fresh session when the claim fails (reported as `resumed == false`).
    pub async fn connect(
        ws_url: &str,
        token: &str,
        resume: Option<(SessionId, &str)>,
        timeout: Duration,
    ) -> Result<(Self, SessionAck)> {
        validate_ws_url(ws_url)?;
        if token.is_empty() {
            return Err(ClientError::InvalidArgument("token is empty".into()));
        }
        let mut request = ws_url
            .into_client_request()
            .map_err(|e| ClientError::InvalidArgument(format!("bad ws_url: {e}")))?;
        let headers = request.headers_mut();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| ClientError::InvalidArgument("token has invalid characters".into()))?,
        );
        headers.insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("aurix"));
        if let Some((sid, tok)) = resume {
            headers.insert(
                RESUME_HEADER,
                HeaderValue::from_str(&format!("{}.{tok}", sid.0))
                    .map_err(|_| ClientError::InvalidArgument("resume token invalid".into()))?,
            );
        }
        let connect = tokio_tungstenite::connect_async(request);
        let (ws, _) = tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| ClientError::Timeout("websocket connect".into()))?
            .map_err(map_ws_error)?;
        let (sink, stream) = ws.split();
        let mut conn = Self { sink, stream };
        let ack = tokio::time::timeout(timeout, conn.wait_for_ack())
            .await
            .map_err(|_| ClientError::Timeout("SessionInitAck".into()))??;
        Ok((conn, ack))
    }

    async fn wait_for_ack(&mut self) -> Result<SessionAck> {
        loop {
            match self.recv().await? {
                Some(ControlMessage::SessionInitAck {
                    session_id,
                    ssrc,
                    media_addr,
                    media_key,
                    resume_token,
                    resume_grace_ms,
                    resumed,
                }) => {
                    let media_key = base64::engine::general_purpose::STANDARD
                        .decode(media_key.as_bytes())
                        .map_err(|_| ClientError::Protocol("media_key is not base64".into()))?;
                    if media_key.len() < 16 {
                        return Err(ClientError::Protocol("media_key too short".into()));
                    }
                    return Ok(SessionAck {
                        session_id,
                        ssrc,
                        media_addr,
                        media_key,
                        resume_token,
                        resume_grace: Duration::from_millis(resume_grace_ms),
                        resumed,
                    });
                }
                Some(ControlMessage::Error { code, message, .. }) => {
                    return Err(ClientError::Server { code, message });
                }
                Some(ControlMessage::SessionClose { reason, .. }) => {
                    return Err(ClientError::Server {
                        code: "SESSION_CLOSED".into(),
                        message: reason,
                    });
                }
                Some(ControlMessage::Ping { nonce }) => {
                    self.send(&ControlMessage::Pong { nonce }).await?;
                }
                Some(_) => {}
                None => return Err(ClientError::NotConnected),
            }
        }
    }

    pub async fn send(&mut self, msg: &ControlMessage) -> Result<()> {
        let json = serde_json::to_string(msg)?;
        self.sink
            .send(Message::Text(json))
            .await
            .map_err(map_ws_error)
    }

    /// Next control message; `Ok(None)` once the peer closed the socket. Transport-level pings
    /// are answered by the WebSocket layer; binary frames are ignored.
    pub async fn recv(&mut self) -> Result<Option<ControlMessage>> {
        loop {
            match self.stream.next().await {
                None => return Ok(None),
                Some(Err(WsError::ConnectionClosed)) | Some(Err(WsError::AlreadyClosed)) => {
                    return Ok(None)
                }
                Some(Err(e)) => return Err(map_ws_error(e)),
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str::<ControlMessage>(&text)
                        .map(Some)
                        .map_err(Into::into);
                }
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(_)) => {}
            }
        }
    }

    /// Graceful close: `SessionClose` (so the server destroys the session instead of keeping
    /// it resumable) followed by a WebSocket close frame.
    pub async fn close(mut self, session_id: SessionId, reason: &str) {
        let _ = self
            .send(&ControlMessage::SessionClose {
                session_id,
                reason: reason.into(),
            })
            .await;
        let _ = self
            .sink
            .send(Message::Close(Some(CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: reason.to_string().into(),
            })))
            .await;
        let _ = self.sink.close().await;
    }

    /// Drop the socket without telling the server (used in tests to simulate a network cut).
    pub fn abort(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ws_host() {
        assert_eq!(
            ws_host("ws://127.0.0.1:8081/ws").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            ws_host("wss://voice.example.com/ws?x=1").as_deref(),
            Some("voice.example.com")
        );
        assert_eq!(ws_host("ws://[::1]:8081/ws").as_deref(), Some("::1"));
        assert_eq!(ws_host("garbage"), None);
    }

    #[test]
    fn decodes_identity_from_unverified_jwt() {
        let uid = uuid::Uuid::new_v4();
        let payload = serde_json::json!({
            "sub": "x", "user_id": uid, "display_name": "Alice", "exp": 1_900_000_000
        });
        let seg = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let id = token_identity(&format!("eyJhbGciOiJIUzI1NiJ9.{seg}.sig"));
        assert_eq!(id.user_id, Some(UserId(uid)));
        assert_eq!(id.display_name.as_deref(), Some("Alice"));
        assert_eq!(id.expires_at_unix, Some(1_900_000_000));
        assert_eq!(token_identity("not-a-jwt"), TokenIdentity::default());
    }

    #[test]
    fn rejects_non_ws_urls() {
        assert!(matches!(
            validate_ws_url("http://x"),
            Err(ClientError::InvalidArgument(_))
        ));
    }
}
