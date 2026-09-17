//! WebSocket signalling for players (`/ws`) and the tenant event stream (`/events`).
//!
//! Every player connection owns exactly one media session. The connection lifecycle is persisted
//! (sessions, channel memberships) and all fan-out (participant joins, speaking, mutes, kicks,
//! recording notices) runs through two node-wide tasks so cost is per event, not per connection.

use aurix_auth::ValidatedToken;
use aurix_common::error::AurixError;
use aurix_common::protocol::{ControlMessage, ParticipantBrief};
use aurix_common::types::*;
use aurix_control::{ControlPlane, ServerEvent};
use aurix_media::{MediaEvent, SfuNode};
use aurix_recording::RecordingService;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use dashmap::{DashMap, DashSet};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const OUTBOUND_QUEUE: usize = 256;
const PING_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_TEXT_FRAME: usize = 64 * 1024;
const MAX_POSITIONS_PER_UPDATE: usize = 64;
/// Sub-protocol prefix browsers use to pass the JWT (`Sec-WebSocket-Protocol: aurix, bearer.<jwt>`).
const BEARER_SUBPROTOCOL_PREFIX: &str = "bearer.";
const AURIX_SUBPROTOCOL: &str = "aurix";

#[derive(Clone)]
pub struct WsState {
    pub control: Arc<ControlPlane>,
    pub sfu: Arc<RwLock<SfuNode>>,
    pub recording: Option<Arc<RecordingService>>,
    pub connections: Arc<DashMap<SessionId, ConnectionInfo>>,
    pub channel_members: Arc<DashMap<ChannelId, DashSet<SessionId>>>,
    pub trusted_proxies: Arc<Vec<ipnetwork::IpNetwork>>,
    fanout_started: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct ConnectionInfo {
    pub tx: mpsc::Sender<String>,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub ssrc: u32,
    pub channels: Vec<ChannelId>,
}

impl WsState {
    pub fn new(
        control: Arc<ControlPlane>,
        sfu: Arc<RwLock<SfuNode>>,
        recording: Option<Arc<RecordingService>>,
    ) -> Self {
        let trusted_proxies = Arc::new(aurix_common::net::parse_trusted_proxies(
            &control.config.server.trusted_proxies,
        ));
        Self {
            control,
            sfu,
            recording,
            connections: Arc::new(DashMap::new()),
            channel_members: Arc::new(DashMap::new()),
            trusted_proxies,
            fanout_started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Starts the node-wide fan-out tasks (idempotent).
    pub fn start_fanout(&self) {
        if self.fanout_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let st = self.clone();
        tokio::spawn(async move { st.run_server_event_fanout().await });
        let st = self.clone();
        tokio::spawn(async move { st.run_media_event_fanout().await });
    }

    /// Sends `SessionClose` to every player and drops their senders so the connection tasks end.
    pub fn close_all(&self, reason: &str) {
        let ids: Vec<SessionId> = self.connections.iter().map(|e| *e.key()).collect();
        for sid in ids {
            self.send_to_session(
                &sid,
                &ControlMessage::SessionClose {
                    session_id: sid,
                    reason: reason.into(),
                },
            );
            self.connections.remove(&sid);
        }
    }

    fn send_to_session(&self, session_id: &SessionId, msg: &ControlMessage) {
        if let Some(conn) = self.connections.get(session_id) {
            if let Ok(json) = serde_json::to_string(msg) {
                let _ = conn.tx.try_send(json);
            }
        }
    }

    fn broadcast_channel(
        &self,
        channel_id: &ChannelId,
        msg: &ControlMessage,
        exclude: Option<&SessionId>,
    ) {
        let Ok(json) = serde_json::to_string(msg) else {
            return;
        };
        let Some(members) = self.channel_members.get(channel_id) else {
            return;
        };
        for sid in members.iter() {
            if Some(&*sid) == exclude {
                continue;
            }
            if let Some(conn) = self.connections.get(&sid) {
                // Slow consumers drop broadcast frames instead of stalling the node.
                let _ = conn.tx.try_send(json.clone());
            }
        }
    }

    fn sessions_of_user(&self, app_id: AppId, user_id: UserId) -> Vec<SessionId> {
        self.connections
            .iter()
            .filter(|e| e.value().app_id == app_id && e.value().user_id == user_id)
            .map(|e| *e.key())
            .collect()
    }

    fn index_join(&self, channel_id: ChannelId, session_id: SessionId) {
        self.channel_members
            .entry(channel_id)
            .or_default()
            .insert(session_id);
        if let Some(mut conn) = self.connections.get_mut(&session_id) {
            if !conn.channels.contains(&channel_id) {
                conn.channels.push(channel_id);
            }
        }
    }

    fn index_leave(&self, channel_id: &ChannelId, session_id: &SessionId) {
        if let Some(members) = self.channel_members.get(channel_id) {
            members.remove(session_id);
            if members.is_empty() {
                drop(members);
                self.channel_members
                    .remove_if(channel_id, |_, m| m.is_empty());
            }
        }
        if let Some(mut conn) = self.connections.get_mut(session_id) {
            conn.channels.retain(|c| c != channel_id);
        }
    }

    async fn run_server_event_fanout(self) {
        let mut rx = self.control.events.subscribe();
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("WS event fan-out lagged by {n} events");
                    continue;
                }
                Err(_) => return,
            };
            match event {
                ServerEvent::ParticipantJoined {
                    channel_id,
                    user_id,
                    display_name,
                    session_id,
                    ..
                } => {
                    let ssrc = self
                        .connections
                        .get(&session_id)
                        .map(|c| c.ssrc)
                        .unwrap_or(0);
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::ParticipantJoined {
                            channel_id,
                            user_id,
                            display_name,
                            ssrc,
                        },
                        Some(&session_id),
                    );
                }
                ServerEvent::ParticipantLeft {
                    channel_id,
                    user_id,
                    session_id,
                    ..
                } => {
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::ParticipantLeft {
                            channel_id,
                            user_id,
                        },
                        Some(&session_id),
                    );
                }
                ServerEvent::UserMuted {
                    app_id,
                    channel_id,
                    user_id,
                    ..
                } => {
                    self.apply_server_mute(app_id, user_id, true);
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::MuteStateChanged {
                            channel_id,
                            user_id,
                            muted: true,
                            server_muted: true,
                        },
                        None,
                    );
                }
                ServerEvent::UserUnmuted {
                    app_id,
                    channel_id,
                    user_id,
                    ..
                } => {
                    self.apply_server_mute(app_id, user_id, false);
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::MuteStateChanged {
                            channel_id,
                            user_id,
                            muted: false,
                            server_muted: false,
                        },
                        None,
                    );
                }
                ServerEvent::UserKicked {
                    app_id,
                    channel_id,
                    user_id,
                    reason,
                    ..
                } => {
                    for sid in self.sessions_of_user(app_id, user_id) {
                        self.send_to_session(
                            &sid,
                            &ControlMessage::Kick {
                                channel_id,
                                user_id,
                                reason: reason.clone(),
                            },
                        );
                        self.leave_channel_full(sid, channel_id, "kicked").await;
                    }
                }
                ServerEvent::UserBanned {
                    app_id,
                    user_id,
                    reason,
                    ..
                } => {
                    for sid in self.sessions_of_user(app_id, user_id) {
                        self.send_to_session(
                            &sid,
                            &ControlMessage::SessionClose {
                                session_id: sid,
                                reason: format!("banned: {reason}"),
                            },
                        );
                        // Dropping the sender closes the connection's send loop.
                        self.connections.remove(&sid);
                    }
                }
                ServerEvent::RecordingStarted {
                    channel_id,
                    recording_id,
                    ..
                }
                | ServerEvent::RecordingConsentRequired {
                    channel_id,
                    recording_id,
                    ..
                } => {
                    let initiated_by = self
                        .recording
                        .as_ref()
                        .and_then(|r| {
                            r.active_in_channel(&channel_id)
                                .into_iter()
                                .find(|(id, _)| *id == recording_id)
                        })
                        .map(|(_, u)| u)
                        .unwrap_or(UserId(uuid::Uuid::nil()));
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::RecordingNotification {
                            channel_id,
                            recording_id,
                            active: true,
                            initiated_by,
                        },
                        None,
                    );
                }
                ServerEvent::RecordingStopped {
                    channel_id,
                    recording_id,
                    ..
                } => {
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::RecordingNotification {
                            channel_id,
                            recording_id,
                            active: false,
                            initiated_by: UserId(uuid::Uuid::nil()),
                        },
                        None,
                    );
                }
                ServerEvent::ChannelDestroyed { channel_id, .. } => {
                    let members: Vec<SessionId> = self
                        .channel_members
                        .get(&channel_id)
                        .map(|m| m.iter().map(|s| *s).collect())
                        .unwrap_or_default();
                    for sid in members {
                        self.leave_channel_full(sid, channel_id, "channel_destroyed")
                            .await;
                    }
                }
                _ => {}
            }
        }
    }

    fn apply_server_mute(&self, app_id: AppId, user_id: UserId, muted: bool) {
        let sfu = self.sfu.read();
        for s in sfu.sessions_for_user(&user_id) {
            if s.app_id == app_id {
                s.is_server_muted.store(muted, Ordering::Relaxed);
            }
        }
    }

    async fn run_media_event_fanout(self) {
        let mut rx = self.sfu.read().subscribe_events();
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("WS media fan-out lagged by {n} events");
                    continue;
                }
                Err(_) => return,
            };
            match event {
                MediaEvent::SessionBound { session_id, .. } => {
                    self.send_to_session(&session_id, &ControlMessage::MediaBound { session_id });
                }
                MediaEvent::SpeakingChanged {
                    user_id,
                    channels,
                    speaking,
                    ..
                } => {
                    for channel_id in channels {
                        self.broadcast_channel(
                            &channel_id,
                            &ControlMessage::SpeakingStateChanged {
                                channel_id,
                                user_id,
                                speaking,
                            },
                            None,
                        );
                    }
                }
                MediaEvent::MuteChanged {
                    session_id,
                    user_id,
                    channels,
                    muted,
                } => {
                    let server_muted = self
                        .sfu
                        .read()
                        .get_session(&session_id)
                        .map(|s| s.is_server_muted.load(Ordering::Relaxed))
                        .unwrap_or(false);
                    for channel_id in channels {
                        self.broadcast_channel(
                            &channel_id,
                            &ControlMessage::MuteStateChanged {
                                channel_id,
                                user_id,
                                muted,
                                server_muted,
                            },
                            None,
                        );
                    }
                }
            }
        }
    }

    /// Leaves a channel everywhere: SFU, WS index, DB membership, Redis, events.
    async fn leave_channel_full(&self, session_id: SessionId, channel_id: ChannelId, reason: &str) {
        let Some((user_id, app_id)) = self
            .connections
            .get(&session_id)
            .map(|c| (c.user_id, c.app_id))
        else {
            return;
        };
        let was_member = self
            .channel_members
            .get(&channel_id)
            .map(|m| m.contains(&session_id))
            .unwrap_or(false);
        {
            let sfu = self.sfu.read();
            let _ = sfu.leave_channel(&session_id, &channel_id);
        }
        self.index_leave(&channel_id, &session_id);
        if !was_member {
            return;
        }
        if let Err(e) = self
            .control
            .sessions
            .remove_channel_membership(channel_id, session_id)
            .await
        {
            warn!("membership close failed: {e}");
        }
        let _ = self
            .control
            .channels
            .update_participant_count(channel_id, -1)
            .await;
        if let Some(ref redis) = self.control.redis {
            let _ = redis.remove_user_channel(user_id, channel_id).await;
            let _ = redis.decr_channel_participants(channel_id).await;
        }
        if let Some(rec) = &self.recording {
            rec.on_user_left(channel_id, user_id).await;
        }
        self.control.events.publish(ServerEvent::ParticipantLeft {
            app_id,
            channel_id,
            user_id,
            session_id,
            reason: reason.into(),
            timestamp: chrono::Utc::now(),
        });
    }
}

#[derive(Deserialize, Default)]
pub struct WsQuery {
    /// Deprecated fallback for clients that cannot set headers or sub-protocols. Tokens in URLs
    /// end up in proxy/access logs; prefer the `bearer.<jwt>` sub-protocol.
    pub token: Option<String>,
}

/// Extracts the JWT from (in order) `Authorization: Bearer`, the `bearer.<jwt>` sub-protocol,
/// or the `token` query parameter. Returns the token and whether the `aurix` sub-protocol
/// must be echoed back.
fn extract_ws_token(headers: &HeaderMap, query: &WsQuery) -> Option<(String, bool)> {
    if let Some(t) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some((t.to_string(), false));
    }
    if let Some(protocols) = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
    {
        let mut token = None;
        let mut has_aurix = false;
        for p in protocols.split(',').map(str::trim) {
            if p == AURIX_SUBPROTOCOL {
                has_aurix = true;
            } else if let Some(t) = p.strip_prefix(BEARER_SUBPROTOCOL_PREFIX) {
                token = Some(t.to_string());
            }
        }
        if let Some(t) = token {
            return Some((t, has_aurix));
        }
    }
    query.token.clone().map(|t| (t, false))
}

fn peer_ip(state: &WsState, headers: &HeaderMap, peer: Option<SocketAddr>) -> IpAddr {
    let peer = peer.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    aurix_common::net::client_ip(
        peer,
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        headers.get("x-real-ip").and_then(|v| v.to_str().ok()),
        &state.trusted_proxies,
    )
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
    headers: HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
    Query(query): Query<WsQuery>,
) -> Response {
    let Some((token, echo_subprotocol)) = extract_ws_token(&headers, &query) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let ip = peer_ip(&state, &headers, peer.map(|p| p.0));
    let validated = match state
        .control
        .authenticate_session(&token, &ip.to_string())
        .await
    {
        Ok((v, _node)) => v,
        Err(AurixError::UserBanned(_)) => return StatusCode::FORBIDDEN.into_response(),
        Err(AurixError::RateLimitExceeded(_)) => {
            return StatusCode::TOO_MANY_REQUESTS.into_response()
        }
        Err(AurixError::MediaNodeUnavailable(_)) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(255).collect::<String>());
    let ws = ws.max_message_size(MAX_TEXT_FRAME);
    let ws = if echo_subprotocol {
        ws.protocols([AURIX_SUBPROTOCOL])
    } else {
        ws
    };
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state, validated, ip, user_agent))
}

async fn handle_ws_connection(
    socket: WebSocket,
    state: WsState,
    token: ValidatedToken,
    ip: IpAddr,
    user_agent: Option<String>,
) {
    state.start_fanout();
    let session_id = SessionId::new();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(OUTBOUND_QUEUE);

    // Media session first: node capacity is the hard limit.
    let created = {
        let sfu = state.sfu.read();
        sfu.create_session(
            session_id,
            token.user_id,
            token.app_id,
            token.display_name.clone(),
        )
    };
    let media_session = match created {
        Ok(s) => s,
        Err(e) => {
            let err = ControlMessage::Error {
                code: e.error_code().into(),
                message: e.public_message(),
            };
            let _ = ws_sender
                .send(Message::Text(
                    serde_json::to_string(&err).unwrap_or_default(),
                ))
                .await;
            let _ = ws_sender.close().await;
            return;
        }
    };

    if let Err(e) = state
        .control
        .sessions
        .create_session(
            session_id,
            token.user_id,
            token.app_id,
            state.control.node_id,
            &ip.to_string(),
            user_agent.as_deref(),
        )
        .await
    {
        warn!("session persistence failed: {e}");
        {
            let sfu = state.sfu.read();
            let _ = sfu.destroy_session(&session_id);
        }
        let err = ControlMessage::Error {
            code: "INTERNAL_ERROR".into(),
            message: "Session could not be created".into(),
        };
        let _ = ws_sender
            .send(Message::Text(
                serde_json::to_string(&err).unwrap_or_default(),
            ))
            .await;
        let _ = ws_sender.close().await;
        return;
    }

    state.connections.insert(
        session_id,
        ConnectionInfo {
            tx: tx.clone(),
            user_id: token.user_id,
            app_id: token.app_id,
            display_name: token.display_name.clone(),
            ssrc: media_session.ssrc,
            channels: Vec::new(),
        },
    );

    if let Some(ref redis) = state.control.redis {
        let _ = redis
            .set_session_node(session_id, state.control.node_id)
            .await;
    }

    let media_addr = format!(
        "{}:{}",
        state
            .control
            .config
            .media
            .external_ip
            .as_deref()
            .unwrap_or(&state.control.config.server.host),
        state.control.config.media.port
    );
    let init_ack = ControlMessage::SessionInitAck {
        session_id,
        ssrc: media_session.ssrc,
        media_addr,
        media_key: base64::engine::general_purpose::STANDARD.encode(media_session.media_key),
    };
    if tx
        .send(serde_json::to_string(&init_ack).unwrap_or_default())
        .await
        .is_err()
    {
        cleanup_connection(&state, session_id, &token, "send_failed").await;
        return;
    }
    aurix_metrics::WS_CONNECTIONS.inc();
    info!(
        "WS session {} opened for user {} ({})",
        session_id, token.user_id, ip
    );

    let send_task = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => { if ws_sender.send(Message::Text(m)).await.is_err() { break; } }
                    None => { let _ = ws_sender.close().await; break; }
                },
                _ = ping.tick() => { if ws_sender.send(Message::Ping(Vec::new())).await.is_err() { break; } }
            }
        }
    });

    let recv_state = state.clone();
    let recv_token = token.clone();
    let recv_tx = tx.clone();
    let recv_task = tokio::spawn(async move {
        loop {
            let frame = tokio::time::timeout(IDLE_TIMEOUT, ws_receiver.next()).await;
            let msg = match frame {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => {
                    debug!("WS session {session_id} idle timeout");
                    break;
                }
            };
            match msg {
                Message::Text(text) => match serde_json::from_str::<ControlMessage>(&text) {
                    Ok(cm) => {
                        handle_control_message(&recv_state, session_id, &recv_token, cm, &recv_tx)
                            .await
                    }
                    Err(_) => {
                        send_error(&recv_tx, "VALIDATION_ERROR", "Malformed control message").await
                    }
                },
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Keep the session→node mapping alive in Redis while connected.
    let redis_state = state.clone();
    let redis_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            if redis_state.connections.get(&session_id).is_none() {
                return;
            }
            if let Some(ref redis) = redis_state.control.redis {
                let _ = redis
                    .set_session_node(session_id, redis_state.control.node_id)
                    .await;
            }
        }
    });

    tokio::select! { _ = send_task => {}, _ = recv_task => {} }
    redis_task.abort();
    cleanup_connection(&state, session_id, &token, "disconnected").await;
}

async fn cleanup_connection(
    state: &WsState,
    session_id: SessionId,
    token: &ValidatedToken,
    reason: &str,
) {
    let channels: Vec<ChannelId> = state
        .connections
        .get(&session_id)
        .map(|c| c.channels.clone())
        .unwrap_or_default();
    for ch in &channels {
        state.leave_channel_full(session_id, *ch, reason).await;
    }
    let quality = {
        let sfu = state.sfu.read();
        let q = sfu
            .get_session(&session_id)
            .map(|s| serde_json::to_value(&*s.quality.read()).unwrap_or_default());
        let _ = sfu.destroy_session(&session_id);
        q
    };
    state.connections.remove(&session_id);
    let _ = state
        .control
        .sessions
        .close_session_memberships(session_id)
        .await;
    if let Err(e) = state
        .control
        .sessions
        .close_session(session_id, reason, quality)
        .await
    {
        warn!("session close persistence failed: {e}");
    }
    aurix_metrics::WS_CONNECTIONS.dec();
    info!(
        "WS session {} closed for user {} ({})",
        session_id, token.user_id, reason
    );
}

async fn send_error(tx: &mpsc::Sender<String>, code: &str, message: &str) {
    let err = ControlMessage::Error {
        code: code.into(),
        message: message.into(),
    };
    let _ = tx
        .send(serde_json::to_string(&err).unwrap_or_default())
        .await;
}

async fn send_msg(tx: &mpsc::Sender<String>, msg: &ControlMessage) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = tx.send(json).await;
    }
}

fn channel_role(
    state: &WsState,
    session_id: &SessionId,
    channel_id: &ChannelId,
) -> Option<ChannelRole> {
    let sfu = state.sfu.read();
    let session = sfu.get_session(session_id)?;
    let channel = sfu.get_channel(channel_id)?;
    if !channel.has_participant(&session.user_id) {
        return None;
    }
    Some(channel.get_role(&session.user_id))
}

async fn handle_control_message(
    state: &WsState,
    session_id: SessionId,
    token: &ValidatedToken,
    msg: ControlMessage,
    tx: &mpsc::Sender<String>,
) {
    match msg {
        ControlMessage::ChannelJoin { channel_id, .. } => {
            let key = format!("join:{}", token.user_id);
            let per_minute = state
                .control
                .config
                .rate_limiting
                .channel_joins_per_minute
                .max(1) as f64;
            if state.control.config.rate_limiting.enabled
                && !state
                    .control
                    .rate_limiter
                    .check_with_cost(&key, 60.0 / per_minute)
            {
                return send_error(tx, "RATE_LIMIT_EXCEEDED", "Too many channel joins").await;
            }
            if let Err(e) = state
                .control
                .rbac
                .check_channel_join(&channel_id, &token.channels)
            {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            // Tenant check + persisted configuration (limits, spatial settings, codec).
            let config = match state
                .control
                .channels
                .load_channel_config(token.app_id, channel_id)
                .await
            {
                Ok(c) => c,
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            };
            let role = state
                .control
                .rbac
                .role_from_permissions(&channel_id, &token.channels);
            let join_result = {
                let sfu = state.sfu.read();
                sfu.join_channel(&session_id, channel_id, config, role)
            };
            let existing = match join_result {
                Ok(existing) => existing,
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            };
            let ssrc = state
                .connections
                .get(&session_id)
                .map(|c| c.ssrc)
                .unwrap_or(0);
            if let Err(e) = state
                .control
                .sessions
                .add_channel_membership(channel_id, token.user_id, session_id, role, ssrc)
                .await
            {
                warn!("membership persistence failed: {e}");
                {
                    let sfu = state.sfu.read();
                    let _ = sfu.leave_channel(&session_id, &channel_id);
                }
                return send_error(tx, "INTERNAL_ERROR", "Channel join could not be persisted")
                    .await;
            }
            state.index_join(channel_id, session_id);
            let _ = state
                .control
                .channels
                .update_participant_count(channel_id, 1)
                .await;
            if let Some(ref redis) = state.control.redis {
                let _ = redis.add_user_channel(token.user_id, channel_id).await;
                let _ = redis.incr_channel_participants(channel_id).await;
            }
            let participants: Vec<ParticipantBrief> = {
                let sfu = state.sfu.read();
                let channel = sfu.get_channel(&channel_id);
                existing
                    .iter()
                    .map(|s| ParticipantBrief {
                        user_id: s.user_id,
                        display_name: s.display_name.clone(),
                        ssrc: s.ssrc,
                        role: channel
                            .as_ref()
                            .map(|c| c.get_role(&s.user_id))
                            .unwrap_or(ChannelRole::Speaker),
                        is_muted: s.is_muted.load(Ordering::Relaxed)
                            || s.is_server_muted.load(Ordering::Relaxed),
                        is_speaking: s.is_speaking.load(Ordering::Relaxed),
                    })
                    .collect()
            };
            send_msg(
                tx,
                &ControlMessage::ChannelJoinAck {
                    channel_id,
                    participants,
                },
            )
            .await;
            // Active recordings in the channel must be disclosed to the newcomer.
            if let Some(rec) = &state.recording {
                for (recording_id, initiated_by) in rec.active_in_channel(&channel_id) {
                    send_msg(
                        tx,
                        &ControlMessage::RecordingNotification {
                            channel_id,
                            recording_id,
                            active: true,
                            initiated_by,
                        },
                    )
                    .await;
                }
            }
            state
                .control
                .events
                .publish(ServerEvent::ParticipantJoined {
                    app_id: token.app_id,
                    channel_id,
                    user_id: token.user_id,
                    display_name: token.display_name.clone(),
                    session_id,
                    timestamp: chrono::Utc::now(),
                });
        }

        ControlMessage::ChannelLeave { channel_id } => {
            state
                .leave_channel_full(session_id, channel_id, "voluntary")
                .await;
        }

        ControlMessage::PositionUpdate {
            channel_id,
            positions,
        } => {
            let Some(role) = channel_role(state, &session_id, &channel_id) else {
                return send_error(tx, "AUTH_DENIED", "Not a member of this channel").await;
            };
            if positions.len() > MAX_POSITIONS_PER_UPDATE {
                return send_error(tx, "VALIDATION_ERROR", "Too many positions in one update")
                    .await;
            }
            // Players may only move themselves; moderators (game-server proxies) may move anyone.
            let authoritative = matches!(role, ChannelRole::Moderator | ChannelRole::Administrator);
            let accepted: Vec<_> = positions
                .into_iter()
                .filter(|p| authoritative || p.user_id == token.user_id)
                .filter(|p| p.position.is_finite() && p.orientation.is_finite())
                .collect();
            if accepted.is_empty() {
                return;
            }
            {
                let sfu = state.sfu.read();
                for up in &accepted {
                    sfu.update_position(&up.user_id, &channel_id, up.position.clone());
                }
            }
            state.broadcast_channel(
                &channel_id,
                &ControlMessage::PositionUpdate {
                    channel_id,
                    positions: accepted,
                },
                Some(&session_id),
            );
        }

        ControlMessage::OcclusionUpdate {
            channel_id,
            source_user_id,
            occlusion_factor,
        } => {
            let Some(role) = channel_role(state, &session_id, &channel_id) else {
                return send_error(tx, "AUTH_DENIED", "Not a member of this channel").await;
            };
            if !occlusion_factor.is_finite() || !(0.0..=1.0).contains(&occlusion_factor) {
                return send_error(
                    tx,
                    "VALIDATION_ERROR",
                    "occlusion_factor must be within 0..=1",
                )
                .await;
            }
            // Occlusion is a listener-side attenuation; only the listener (or a moderator) may set it.
            if source_user_id == token.user_id
                && !matches!(role, ChannelRole::Moderator | ChannelRole::Administrator)
            {
                return;
            }
            state.send_to_session(
                &session_id,
                &ControlMessage::OcclusionUpdate {
                    channel_id,
                    source_user_id,
                    occlusion_factor,
                },
            );
        }

        ControlMessage::ReverbZoneUpdate { channel_id, reverb } => {
            let Some(role) = channel_role(state, &session_id, &channel_id) else {
                return send_error(tx, "AUTH_DENIED", "Not a member of this channel").await;
            };
            if !matches!(role, ChannelRole::Moderator | ChannelRole::Administrator) {
                return send_error(
                    tx,
                    "AUTH_DENIED",
                    "Only channel moderators may change reverb zones",
                )
                .await;
            }
            state.broadcast_channel(
                &channel_id,
                &ControlMessage::ReverbZoneUpdate { channel_id, reverb },
                None,
            );
        }

        ControlMessage::RecordingConsentResponse {
            recording_id,
            consent,
        } => {
            let Some(rec) = &state.recording else {
                return send_error(tx, "INVALID_CONFIG", "Recording is not enabled").await;
            };
            if let Err(e) = rec
                .set_consent(token.app_id, recording_id, token.user_id, consent)
                .await
            {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
        }

        ControlMessage::MuteStateChanged { user_id, muted, .. } => {
            if user_id != token.user_id {
                return send_error(tx, "AUTH_DENIED", "Cannot change another user's mute state")
                    .await;
            }
            let channels = {
                let sfu = state.sfu.read();
                match sfu.get_session(&session_id) {
                    Some(s) => {
                        s.is_muted.store(muted, Ordering::Relaxed);
                        (s.get_channels(), s.is_server_muted.load(Ordering::Relaxed))
                    }
                    None => return,
                }
            };
            for channel_id in channels.0 {
                state.broadcast_channel(
                    &channel_id,
                    &ControlMessage::MuteStateChanged {
                        channel_id,
                        user_id,
                        muted,
                        server_muted: channels.1,
                    },
                    None,
                );
            }
        }

        ControlMessage::QualityReport {
            rtt_ms,
            jitter_ms,
            packet_loss,
        } => {
            if !(rtt_ms.is_finite() && jitter_ms.is_finite() && packet_loss.is_finite()) {
                return;
            }
            aurix_metrics::RTT_MS.observe(rtt_ms.clamp(0.0, 10_000.0) as f64);
            aurix_metrics::JITTER_MS.observe(jitter_ms.clamp(0.0, 10_000.0) as f64);
            aurix_metrics::PACKET_LOSS_RATE.set(packet_loss.clamp(0.0, 100.0) as f64);
            {
                let sfu = state.sfu.read();
                if let Some(s) = sfu.get_session(&session_id) {
                    let mut q = s.quality.write();
                    q.rtt_ms = rtt_ms;
                    q.jitter_ms = jitter_ms;
                    q.packet_loss_percent = packet_loss;
                    q.mos_score = q.calculate_mos();
                }
            }
            if packet_loss > 10.0 || jitter_ms > 50.0 {
                let target = if packet_loss > 20.0 { 16 } else { 32 };
                send_msg(
                    tx,
                    &ControlMessage::BitrateCommand {
                        target_bitrate_kbps: target,
                        reason: format!("loss={packet_loss:.1}% jitter={jitter_ms:.1}ms"),
                    },
                )
                .await;
                if packet_loss > 20.0 {
                    state.control.events.publish(ServerEvent::QualityAlert {
                        app_id: token.app_id,
                        session_id,
                        user_id: token.user_id,
                        metric: "packet_loss".into(),
                        value: packet_loss as f64,
                        threshold: 20.0,
                        timestamp: chrono::Utc::now(),
                    });
                }
            }
        }

        ControlMessage::WebRtcOffer { sdp } => {
            if sdp.len() > MAX_TEXT_FRAME {
                return send_error(tx, "VALIDATION_ERROR", "SDP too large").await;
            }
            let answer = {
                let sfu = state.sfu.read();
                sfu.attach_webrtc(&session_id, &sdp)
            };
            match answer {
                Ok(sdp) => send_msg(tx, &ControlMessage::WebRtcAnswer { sdp }).await,
                Err(e) => send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::Ping { nonce } => send_msg(tx, &ControlMessage::Pong { nonce }).await,

        ControlMessage::SessionClose { .. } => {
            // The receive loop ends when the client closes the socket; nothing to do here.
        }

        // Server→client only.
        ControlMessage::SessionInit { .. }
        | ControlMessage::SessionInitAck { .. }
        | ControlMessage::MediaBound { .. }
        | ControlMessage::ChannelJoinAck { .. }
        | ControlMessage::ParticipantJoined { .. }
        | ControlMessage::ParticipantLeft { .. }
        | ControlMessage::SpeakingStateChanged { .. }
        | ControlMessage::BitrateCommand { .. }
        | ControlMessage::RecordingNotification { .. }
        | ControlMessage::Error { .. }
        | ControlMessage::Kick { .. }
        | ControlMessage::WebRtcAnswer { .. }
        | ControlMessage::Pong { .. } => {
            send_error(
                tx,
                "VALIDATION_ERROR",
                "Message type is server-to-client only",
            )
            .await
        }
    }
}

// ── Tenant event stream (game servers / dashboards) ──

#[derive(Deserialize, Default)]
pub struct EventStreamQuery {
    pub api_key: Option<String>,
}

pub async fn event_stream_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
    headers: HeaderMap,
    Query(query): Query<EventStreamQuery>,
) -> Response {
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string)
        })
        .or(query.api_key);
    let Some(key) = key else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let key_row = match state.control.api_keys.validate_key(&key).await {
        Ok(k) => k,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !aurix_auth::ApiKeyService::has_permission(&key_row, "events:read")
        && !aurix_auth::ApiKeyService::has_permission(&key_row, "*")
        && !key_row
            .permissions
            .get("all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let app_id = AppId::from_uuid(key_row.app_id);
    ws.on_upgrade(move |socket| handle_event_stream(socket, state, app_id))
}

async fn handle_event_stream(socket: WebSocket, state: WsState, app_id: AppId) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut event_rx = state.control.events.subscribe();

    let send_task = tokio::spawn(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            // Node health is operational (not tenant) data and is only exposed to admins.
            if event.app_id() != Some(app_id) {
                continue;
            }
            let json = serde_json::to_string(&event).unwrap_or_default();
            if ws_sender.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    let recv_task = tokio::spawn(async move {
        loop {
            match tokio::time::timeout(IDLE_TIMEOUT * 10, ws_receiver.next()).await {
                Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
                Ok(Some(Ok(_))) => {}
            }
        }
    });

    tokio::select! { _ = send_task => {}, _ = recv_task => {} }
}
