use aurix_common::protocol::ControlMessage;
use aurix_common::types::*;
use aurix_control::{ControlPlane, ServerEvent};
use aurix_media::SfuNode;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::Response;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct WsState {
    pub control: Arc<ControlPlane>,
    pub sfu: Arc<RwLock<SfuNode>>,
    pub connections: Arc<DashMap<SessionId, ConnectionInfo>>,
}

#[derive(Clone)]
pub struct ConnectionInfo {
    pub tx: tokio::sync::mpsc::Sender<String>,
    pub user_id: UserId,
    pub app_id: AppId,
    pub channels: Vec<ChannelId>,
}

#[derive(Deserialize)]
pub struct WsQuery { pub token: String }

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
    Query(query): Query<WsQuery>,
) -> Result<Response, axum::http::StatusCode> {
    let validated = state.control.validate_token(&query.token)
        .map_err(|_| axum::http::StatusCode::UNAUTHORIZED)?;

    // ── Ban check (prevents banned users whose JWT hasn't expired yet) ──
    let bans = aurix_db::queries::get_active_bans_for_user(
        &state.control.pool, validated.app_id.0, validated.user_id.0,
    ).await.unwrap_or_default();
    if !bans.is_empty() {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }

    Ok(ws.on_upgrade(move |socket| handle_ws_connection(socket, state, validated)))
}

async fn handle_ws_connection(
    socket: WebSocket,
    state: WsState,
    token: aurix_auth::ValidatedToken,
) {
    let session_id = SessionId::new();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);

    state.connections.insert(session_id, ConnectionInfo {
        tx: tx.clone(), user_id: token.user_id, app_id: token.app_id, channels: Vec::new(),
    });

    // Create media session — drop the guard before any .await
    let media_session = {
        let sfu = state.sfu.read();
        match sfu.create_session(session_id, token.user_id, token.app_id, token.display_name.clone()) {
            Ok(s) => s,
            Err(e) => { error!("Failed to create session: {}", e); return; }
        }
    };

    // Track session→node in Redis
    if let Some(ref redis) = state.control.redis {
        let node_id = state.sfu.read().node_id;
        let _ = redis.set_session_node(session_id, node_id).await;
    }

    let init_ack = ControlMessage::SessionInitAck {
        session_id,
        ssrc: media_session.ssrc,
        media_addr: format!("{}:{}", state.control.config.media.external_ip.as_deref().unwrap_or("127.0.0.1"), state.control.config.media.port),
    };
    let _ = tx.send(serde_json::to_string(&init_ack).unwrap()).await;

    // ── Event subscription (filtered by this user's app_id) ──
    let mut event_rx = state.control.events.subscribe();
    let event_tx = tx.clone();
    let event_app_id = token.app_id;
    let event_user_id = token.user_id;

    let event_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let should_send = match &event {
                ServerEvent::ParticipantJoined { app_id, .. } => *app_id == event_app_id,
                ServerEvent::ParticipantLeft { app_id, .. } => *app_id == event_app_id,
                ServerEvent::UserMuted { app_id, .. } => *app_id == event_app_id,
                ServerEvent::UserUnmuted { app_id, .. } => *app_id == event_app_id,
                ServerEvent::UserKicked { app_id, user_id, .. } => *app_id == event_app_id && *user_id == event_user_id,
                ServerEvent::UserBanned { app_id, user_id, .. } => *app_id == event_app_id && *user_id == event_user_id,
                ServerEvent::QualityAlert { user_id, .. } => *user_id == event_user_id,
                ServerEvent::RecordingStarted { app_id, .. } => *app_id == event_app_id,
                ServerEvent::RecordingStopped { app_id, .. } => *app_id == event_app_id,
                ServerEvent::RecordingConsentRequired { app_id, .. } => *app_id == event_app_id,
                _ => false,
            };
            if should_send {
                if event_tx.send(serde_json::to_string(&event).unwrap_or_default()).await.is_err() {
                    break;
                }
            }
        }
    });

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(Message::Text(msg)).await.is_err() { break; }
        }
    });

    let state_clone = state.clone();
    let token_clone = token.clone();
    let tx_clone = tx.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(control_msg) = serde_json::from_str::<ControlMessage>(&text) {
                        handle_control_message(&state_clone, session_id, &token_clone, control_msg, &tx_clone).await;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! { _ = send_task => {}, _ = recv_task => {}, _ = event_task => {} }

    state.connections.remove(&session_id);
    {
        let sfu = state.sfu.read();
        let _ = sfu.destroy_session(&session_id);
    }
    if let Some(ref redis) = state.control.redis {
        // Clean up user channels in Redis
        if let Some(conn) = state.connections.get(&session_id) {
            for ch in &conn.channels {
                let _ = redis.remove_user_channel(token.user_id, *ch).await;
                let _ = redis.decr_channel_participants(*ch).await;
            }
        }
    }
    let _ = state.control.sessions.close_session(session_id, "disconnected", None).await;
    info!("WebSocket session {} closed for user {}", session_id, token.user_id);
}

async fn handle_control_message(
    state: &WsState,
    session_id: SessionId,
    token: &aurix_auth::ValidatedToken,
    msg: ControlMessage,
    tx: &tokio::sync::mpsc::Sender<String>,
) {
    match msg {
        ControlMessage::ChannelJoin { channel_id, .. } => {
            if let Err(e) = state.control.rbac.check_channel_join(&channel_id, &token.channels) {
                let error = ControlMessage::Error { code: "AUTH_DENIED".into(), message: e.to_string() };
                let _ = tx.send(serde_json::to_string(&error).unwrap()).await;
                return;
            }
            let role = state.control.rbac.role_from_permissions(&channel_id, &token.channels);
            let config = ChannelConfig::default();
            let join_result = {
                let sfu = state.sfu.read();
                sfu.join_channel(&session_id, channel_id, config, role)
            };
            match join_result {
                Ok(existing) => {
                    if let Some(mut conn) = state.connections.get_mut(&session_id) {
                        conn.channels.push(channel_id);
                    }
                    // Track in Redis
                    if let Some(ref redis) = state.control.redis {
                        let _ = redis.add_user_channel(token.user_id, channel_id).await;
                        let _ = redis.incr_channel_participants(channel_id).await;
                    }
                    let participants: Vec<aurix_common::protocol::ParticipantBrief> = existing.iter().map(|s| {
                        aurix_common::protocol::ParticipantBrief {
                            user_id: s.user_id, display_name: s.display_name.clone(), ssrc: s.ssrc,
                            role: ChannelRole::Speaker,
                            is_muted: s.is_muted.load(std::sync::atomic::Ordering::Relaxed),
                            is_speaking: s.is_speaking.load(std::sync::atomic::Ordering::Relaxed),
                        }
                    }).collect();
                    let ack = ControlMessage::ChannelJoinAck { channel_id, participants };
                    let _ = tx.send(serde_json::to_string(&ack).unwrap()).await;
                    state.control.events.publish(ServerEvent::ParticipantJoined {
                        app_id: token.app_id, channel_id, user_id: token.user_id,
                        display_name: token.display_name.clone(), session_id, timestamp: chrono::Utc::now(),
                    });
                }
                Err(e) => {
                    let error = ControlMessage::Error { code: e.error_code().into(), message: e.to_string() };
                    let _ = tx.send(serde_json::to_string(&error).unwrap()).await;
                }
            }
        }

        ControlMessage::ChannelLeave { channel_id } => {
            {
                let sfu = state.sfu.read();
                let _ = sfu.leave_channel(&session_id, &channel_id);
            }
            if let Some(mut conn) = state.connections.get_mut(&session_id) {
                conn.channels.retain(|c| *c != channel_id);
            }
            if let Some(ref redis) = state.control.redis {
                let _ = redis.remove_user_channel(token.user_id, channel_id).await;
                let _ = redis.decr_channel_participants(channel_id).await;
            }
            state.control.events.publish(ServerEvent::ParticipantLeft {
                app_id: token.app_id, channel_id, user_id: token.user_id,
                session_id, reason: "voluntary".into(), timestamp: chrono::Utc::now(),
            });
        }

        ControlMessage::PositionUpdate { channel_id, positions } => {
            {
                let sfu = state.sfu.read();
                for up in &positions { sfu.update_position(&up.user_id, &channel_id, up.position.clone()); }
            }
            for entry in state.connections.iter() {
                if entry.key() == &session_id { continue; }
                let conn = entry.value();
                if conn.channels.contains(&channel_id) {
                    let msg = ControlMessage::PositionUpdate { channel_id, positions: positions.clone() };
                    let _ = conn.tx.send(serde_json::to_string(&msg).unwrap()).await;
                }
            }
        }

        ControlMessage::OcclusionUpdate { channel_id, source_user_id, occlusion_factor } => {
            for entry in state.connections.iter() {
                if entry.key() == &session_id { continue; }
                let conn = entry.value();
                if conn.channels.contains(&channel_id) {
                    let msg = ControlMessage::OcclusionUpdate { channel_id, source_user_id, occlusion_factor };
                    let _ = conn.tx.send(serde_json::to_string(&msg).unwrap()).await;
                }
            }
        }

        ControlMessage::ReverbZoneUpdate { channel_id, reverb } => {
            for entry in state.connections.iter() {
                let conn = entry.value();
                if conn.channels.contains(&channel_id) {
                    let msg = ControlMessage::ReverbZoneUpdate { channel_id, reverb: reverb.clone() };
                    let _ = conn.tx.send(serde_json::to_string(&msg).unwrap()).await;
                }
            }
        }

        ControlMessage::RecordingConsentResponse { recording_id, consent } => {
            tracing::info!("User {} consent for recording {}: {:?}", token.user_id, recording_id, consent);
        }

        ControlMessage::MuteStateChanged { user_id, muted, .. } => {
            if user_id == token.user_id {
                let sfu = state.sfu.read();
                if let Some(session) = sfu.get_session(&session_id) {
                    session.is_muted.store(muted, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        ControlMessage::QualityReport { rtt_ms, jitter_ms, packet_loss } => {
            aurix_metrics::RTT_MS.observe(rtt_ms as f64);
            aurix_metrics::JITTER_MS.observe(jitter_ms as f64);
            aurix_metrics::PACKET_LOSS_RATE.set(packet_loss as f64);

            if packet_loss > 10.0 || jitter_ms > 50.0 {
                let target = if packet_loss > 20.0 { 16000 } else { 32000 };
                let cmd = ControlMessage::BitrateCommand {
                    target_bitrate_kbps: target,
                    reason: format!("loss={:.1}% jitter={:.1}ms", packet_loss, jitter_ms),
                };
                let _ = tx.send(serde_json::to_string(&cmd).unwrap()).await;
            }
        }

        _ => { warn!("Unhandled control message"); }
    }
}

// ── Server-side Event Stream (for game servers / dashboards) ──

#[derive(Deserialize)]
pub struct EventStreamQuery { pub api_key: String }

pub async fn event_stream_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
    Query(query): Query<EventStreamQuery>,
) -> Result<Response, axum::http::StatusCode> {
    let key_row = state.control.api_keys.validate_key(&query.api_key).await
        .map_err(|_| axum::http::StatusCode::UNAUTHORIZED)?;
    let app_id = AppId::from_uuid(key_row.app_id);
    Ok(ws.on_upgrade(move |socket| handle_event_stream(socket, state, app_id)))
}

async fn handle_event_stream(socket: WebSocket, state: WsState, app_id: AppId) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut event_rx = state.control.events.subscribe();

    let send_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let belongs = match &event {
                ServerEvent::ParticipantJoined { app_id: a, .. } => *a == app_id,
                ServerEvent::ParticipantLeft { app_id: a, .. } => *a == app_id,
                ServerEvent::ChannelCreated { app_id: a, .. } => *a == app_id,
                ServerEvent::ChannelDestroyed { app_id: a, .. } => *a == app_id,
                ServerEvent::UserMuted { app_id: a, .. } => *a == app_id,
                ServerEvent::UserUnmuted { app_id: a, .. } => *a == app_id,
                ServerEvent::UserBanned { app_id: a, .. } => *a == app_id,
                ServerEvent::UserKicked { app_id: a, .. } => *a == app_id,
                ServerEvent::QualityAlert { app_id: a, .. } => *a == app_id,
                ServerEvent::ModerationEvent { app_id: a, .. } => *a == app_id,
                ServerEvent::RecordingStarted { app_id: a, .. } => *a == app_id,
                ServerEvent::RecordingStopped { app_id: a, .. } => *a == app_id,
                ServerEvent::RecordingConsentRequired { app_id: a, .. } => *a == app_id,
                ServerEvent::NodeHealthChanged { .. } => true,
            };
            if belongs {
                let json = serde_json::to_string(&event).unwrap_or_default();
                if ws_sender.send(Message::Text(json)).await.is_err() { break; }
            }
        }
    });

    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            if matches!(msg, Message::Close(_)) { break; }
        }
    });

    tokio::select! { _ = send_task => {}, _ = recv_task => {} }
}