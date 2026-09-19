//! WebSocket signalling for players (`/ws`) and the tenant event stream (`/events`).
//!
//! Every player connection owns exactly one media session. The connection lifecycle is persisted
//! (sessions, channel memberships) and all fan-out (participant joins, speaking, mutes, kicks,
//! recording notices) runs through two node-wide tasks so cost is per event, not per connection.
//!
//! Session resume: when a WebSocket drops without a client `Close` (network blip, NAT rebind,
//! backgrounded app) the session is *detached* rather than destroyed. It keeps its SSRC, media
//! key and channel memberships for `server.session_resume_grace_secs`; the client reconnects
//! presenting `<session_id>.<resume_token>` (header `X-Aurix-Resume`, sub-protocol
//! `resume.<session_id>.<token>` or query `resume=`) together with a valid JWT for the same
//! user and receives `SessionInitAck { resumed: true }` plus one `ChannelJoinAck` per channel.
//! Peers never see a leave/join pair. A failed resume falls back to a fresh session.

use aurix_auth::{JwtService, ValidatedToken};
use aurix_common::crypto::{constant_time_eq, ResumeToken};
use aurix_common::error::AurixError;
use aurix_common::protocol::{
    decode_audio_level, ChatMessage, ControlMessage, LocalMute, ParticipantBrief,
    ParticipantEnergy, ParticipantVolume, TransmissionMode, TtsDestination,
};
use aurix_common::types::*;
use aurix_control::chat::{OutgoingMessage, SYSTEM_USER};
use aurix_control::moderation_actions::{self, ModerationTarget};
use aurix_control::{ActionTokenService, ControlPlane, ParticipantSpeak, ServerEvent};
use aurix_media::session::MAX_PARTICIPANT_GAIN;
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
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const OUTBOUND_QUEUE: usize = 256;
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Uplink loss (server-measured, per quality period) above which a `quality.alert` is raised;
/// mirrors the client-reported downlink threshold.
const UPLINK_LOSS_ALERT_PERCENT: f32 = 20.0;
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_TEXT_FRAME: usize = 64 * 1024;
const MAX_POSITIONS_PER_UPDATE: usize = 64;
/// Sub-protocol prefix browsers use to pass the JWT (`Sec-WebSocket-Protocol: aurix, bearer.<jwt>`).
const BEARER_SUBPROTOCOL_PREFIX: &str = "bearer.";
/// Sub-protocol prefix carrying `<session_id>.<resume_token>` for session resume.
const RESUME_SUBPROTOCOL_PREFIX: &str = "resume.";
const RESUME_HEADER: &str = "x-aurix-resume";
const AURIX_SUBPROTOCOL: &str = "aurix";
/// Queued on a connection's outbound channel to make its send loop close the socket.
const CLOSE_SENTINEL: &str = "";

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
    resume_token_hash: [u8; 32],
    /// Set while the socket is gone and the session waits for the client to resume.
    detached_since: Option<Instant>,
    /// Bumped on every (re)attach so a stale grace timer cannot close a resumed session.
    generation: u64,
    /// Server-initiated close (ban, shutdown); such sessions are never resumable.
    closing: Option<String>,
    /// Delivers `Transcript` events for the session's channels (`SetTranscripts`).
    transcripts: bool,
}

impl ConnectionInfo {
    /// Resume precondition check and claim; the caller holds the exclusive map entry, which makes
    /// the check-then-claim atomic with respect to concurrent reconnects for the same session.
    fn try_claim(&mut self, presented: &[u8; 32], jwt: &ValidatedToken) -> bool {
        if self.detached_since.is_none()
            || self.closing.is_some()
            || self.user_id != jwt.user_id
            || self.app_id != jwt.app_id
            || !constant_time_eq(presented, &self.resume_token_hash)
        {
            return false;
        }
        self.detached_since = None;
        self.generation += 1;
        true
    }
}

enum Disconnect {
    /// Client sent a `Close` frame (or `SessionClose`): a deliberate logout.
    ClientClose,
    /// Socket error, EOF or idle timeout: the client may come back.
    Dropped,
}

/// Outcome of the WebSocket handshake for a session (fresh or resumed).
struct Attached {
    session_id: SessionId,
    ssrc: u32,
    media_key: [u8; 32],
    /// Channels restored from a detached session (`None` for a fresh session).
    resumed_channels: Option<Vec<ChannelId>>,
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

    /// Sends `SessionClose` to every player and closes their sockets (non-resumable).
    pub fn close_all(&self, reason: &str) {
        let ids: Vec<SessionId> = self.connections.iter().map(|e| *e.key()).collect();
        for sid in ids {
            self.request_close(sid, reason);
        }
    }

    /// Marks a session as closing by the server, tells the client and closes the socket. A
    /// detached session is cleaned up right away since nobody is listening.
    fn request_close(&self, session_id: SessionId, reason: &str) {
        let Some(mut conn) = self.connections.get_mut(&session_id) else {
            return;
        };
        conn.closing = Some(reason.to_string());
        let msg = ControlMessage::SessionClose {
            session_id,
            reason: reason.into(),
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = conn.tx.try_send(json);
        }
        let _ = conn.tx.try_send(CLOSE_SENTINEL.to_string());
        let user_id = conn.user_id;
        drop(conn);
        // Attached sessions clean up when their socket task exits; detached ones have nobody
        // to do it, so take over from the grace timer.
        if self.take_detached(session_id) {
            let st = self.clone();
            let reason = reason.to_string();
            tokio::spawn(async move {
                cleanup_connection(&st, session_id, user_id, &reason).await;
            });
        }
    }

    /// Ends a session's detached state without resuming it (the caller must clean it up).
    /// Returns `false` if the session is not detached, so the grace timer stays responsible.
    fn take_detached(&self, session_id: SessionId) -> bool {
        let Some(mut conn) = self.connections.get_mut(&session_id) else {
            return false;
        };
        if conn.detached_since.take().is_none() {
            return false;
        }
        conn.generation += 1;
        aurix_metrics::WS_SESSIONS_DETACHED.dec();
        true
    }

    /// Atomically claims a detached session for resume. The caller must hold a valid JWT for
    /// the same tenant/user; the token is compared in constant time against the stored hash.
    fn claim_resume(&self, session_id: SessionId, token: &str, jwt: &ValidatedToken) -> bool {
        let Some(presented) = ResumeToken::hash_presented(token) else {
            return false;
        };
        let Some(mut conn) = self.connections.get_mut(&session_id) else {
            return false;
        };
        if !conn.try_claim(&presented, jwt) {
            return false;
        }
        aurix_metrics::WS_SESSIONS_DETACHED.dec();
        true
    }

    /// Parks a session whose socket dropped; destroys it if no resume happens within `grace`.
    fn detach(&self, session_id: SessionId, user_id: UserId, grace: Duration) {
        let generation = {
            let Some(mut conn) = self.connections.get_mut(&session_id) else {
                return;
            };
            conn.detached_since = Some(Instant::now());
            conn.generation += 1;
            conn.generation
        };
        aurix_metrics::WS_SESSIONS_DETACHED.inc();
        info!(
            "WS session {} detached for user {}; resumable for {:?}",
            session_id, user_id, grace
        );
        let st = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let expired = st
                .connections
                .get_mut(&session_id)
                .filter(|c| c.detached_since.is_some() && c.generation == generation)
                .map(|mut c| {
                    c.detached_since = None;
                    aurix_metrics::WS_SESSIONS_DETACHED.dec();
                })
                .is_some();
            if expired {
                cleanup_connection(&st, session_id, user_id, "resume_timeout").await;
            }
        });
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

    /// `broadcast_channel` for events that may originate on another node: members are only
    /// addressed when their connection belongs to the event's tenant.
    fn broadcast_channel_in_app(
        &self,
        app_id: AppId,
        channel_id: &ChannelId,
        msg: &ControlMessage,
    ) {
        let Ok(json) = serde_json::to_string(msg) else {
            return;
        };
        let Some(members) = self.channel_members.get(channel_id) else {
            return;
        };
        for sid in members.iter() {
            if let Some(conn) = self.connections.get(&sid) {
                if conn.app_id != app_id {
                    continue;
                }
                let _ = conn.tx.try_send(json.clone());
            }
        }
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
                    ssrc,
                    ..
                } => {
                    let ssrc = self
                        .connections
                        .get(&session_id)
                        .map(|c| c.ssrc)
                        .unwrap_or(ssrc);
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
                    if let Some(rec) = &self.recording {
                        rec.live().on_participant_left(channel_id, user_id);
                    }
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
                ServerEvent::UserBlockChanged {
                    app_id,
                    user_id,
                    blocked_user_id,
                    blocked,
                    ..
                } => {
                    self.apply_block_change(app_id, user_id, blocked_user_id, blocked);
                }
                ServerEvent::ChatMessage {
                    app_id,
                    message,
                    from_session_id,
                } => {
                    self.deliver_chat(app_id, message, from_session_id);
                }
                ServerEvent::ParticipantTyping {
                    app_id,
                    channel_id,
                    user_id,
                    session_id,
                    typing,
                } => {
                    self.deliver_typing(app_id, channel_id, user_id, session_id, typing);
                }
                ServerEvent::ParticipantSpeaking {
                    app_id,
                    channel_id,
                    user_id,
                    speaking,
                } => {
                    self.broadcast_channel_in_app(
                        app_id,
                        &channel_id,
                        &ControlMessage::SpeakingStateChanged {
                            channel_id,
                            user_id,
                            speaking,
                        },
                    );
                }
                ServerEvent::ChannelEnergy {
                    app_id,
                    channel_id,
                    levels,
                } => {
                    self.broadcast_channel_in_app(
                        app_id,
                        &channel_id,
                        &ControlMessage::ChannelEnergy { channel_id, levels },
                    );
                }
                ServerEvent::Transcript { app_id, transcript } => {
                    self.deliver_transcript(app_id, transcript);
                }
                ServerEvent::TtsAnnouncement {
                    app_id,
                    channel_id,
                    request_id,
                    text,
                    voice,
                } => {
                    self.control
                        .speech
                        .play_announcement(&self.sfu, app_id, channel_id, request_id, text, voice);
                }
                ServerEvent::TtsStatus {
                    app_id,
                    request_id,
                    session_id: Some(session_id),
                    client_ref,
                    state,
                    duration_ms,
                    message,
                    ..
                } => {
                    let same_app = self
                        .connections
                        .get(&session_id)
                        .is_some_and(|c| c.app_id == app_id);
                    if same_app {
                        self.send_to_session(
                            &session_id,
                            &ControlMessage::TtsStatus {
                                request_id,
                                client_ref,
                                state,
                                duration_ms,
                                message,
                            },
                        );
                    }
                }
                ServerEvent::TtsStatus {
                    session_id: None, ..
                } => {}
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
                        self.request_close(sid, &format!("banned: {reason}"));
                    }
                }
                ServerEvent::UserDeleted {
                    app_id, user_id, ..
                } => {
                    for sid in self.sessions_of_user(app_id, user_id) {
                        self.request_close(sid, "user deleted");
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
                    let capture = self.recording.as_ref().and_then(|r| {
                        r.active_in_channel(&channel_id)
                            .into_iter()
                            .find(|c| c.id == recording_id)
                    });
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::RecordingNotification {
                            channel_id,
                            recording_id,
                            active: true,
                            initiated_by: capture
                                .map(|c| c.initiated_by)
                                .unwrap_or(UserId(uuid::Uuid::nil())),
                            live: capture.is_some_and(|c| c.live),
                        },
                        None,
                    );
                }
                ServerEvent::LiveStreamStarted {
                    channel_id,
                    stream_id,
                    ..
                } => {
                    // Only the node hosting the tap knows the stream; other nodes' members
                    // of a cascaded channel are still told, so they can respond to consent.
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::RecordingNotification {
                            channel_id,
                            recording_id: stream_id,
                            active: true,
                            initiated_by: UserId(uuid::Uuid::nil()),
                            live: true,
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
                            live: false,
                        },
                        None,
                    );
                }
                ServerEvent::RecordingConsentGiven {
                    app_id,
                    recording_id,
                    user_id,
                    consent,
                } => {
                    if let Some(rec) = self.recording.clone() {
                        tokio::spawn(async move {
                            match rec
                                .set_consent(app_id, recording_id, user_id, consent)
                                .await
                            {
                                Ok(()) | Err(AurixError::NotFound(_)) => {}
                                Err(e) => warn!(
                                    "Relayed consent for {} from {} rejected: {e}",
                                    recording_id, user_id
                                ),
                            }
                        });
                    }
                }
                ServerEvent::LiveStreamStopped {
                    channel_id,
                    stream_id,
                    ..
                } => {
                    self.broadcast_channel(
                        &channel_id,
                        &ControlMessage::RecordingNotification {
                            channel_id,
                            recording_id: stream_id,
                            active: false,
                            initiated_by: UserId(uuid::Uuid::nil()),
                            live: true,
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

    /// Cross-mute is mutual: the blocker stops hearing the target and the target stops hearing
    /// the blocker, in every live session of both on this node. The blocker's sessions get a
    /// `UserBlockChanged` (ack / cross-device sync); the target is not told.
    fn apply_block_change(&self, app_id: AppId, user_id: UserId, target: UserId, blocked: bool) {
        {
            let sfu = self.sfu.read();
            for s in sfu.sessions_for_user(&user_id) {
                if s.app_id == app_id {
                    s.prefs.write().set_blocked(target, blocked);
                }
            }
            for s in sfu.sessions_for_user(&target) {
                if s.app_id == app_id {
                    s.prefs.write().set_blocked_by(user_id, blocked);
                }
            }
        }
        let ack = ControlMessage::UserBlockChanged {
            user_id: target,
            blocked,
        };
        for sid in self.sessions_of_user(app_id, user_id) {
            self.send_to_session(&sid, &ack);
        }
    }

    /// Whether `session_id` may receive text from `sender` (no persistent block either way).
    fn text_allowed(&self, session_id: &SessionId, sender: &UserId) -> bool {
        let sfu = self.sfu.read();
        match sfu.get_session(session_id) {
            Some(s) => !s.prefs.read().is_blocked_either_way(sender),
            None => true,
        }
    }

    /// Whether `session_id` would hear `speaker` in `channel_id` (no block, not locally muted).
    fn hears(&self, session_id: &SessionId, channel_id: &ChannelId, speaker: &UserId) -> bool {
        let sfu = self.sfu.read();
        match sfu.get_session(session_id) {
            Some(s) => s.gain_for(speaker, channel_id).is_some(),
            None => false,
        }
    }

    /// Local delivery of an accepted chat message: channel members (or the target user's
    /// sessions) that have no block relationship with the sender, plus the sender's own echo
    /// carrying `client_ref`.
    fn deliver_chat(
        &self,
        app_id: AppId,
        message: ChatMessage,
        from_session_id: Option<SessionId>,
    ) {
        let mut recipients: Vec<SessionId> = match (message.channel_id, message.to_user_id) {
            (Some(channel_id), _) => self
                .channel_members
                .get(&channel_id)
                .map(|m| m.iter().map(|s| *s).collect())
                .unwrap_or_default(),
            (None, Some(to)) => self.sessions_of_user(app_id, to),
            (None, None) => return,
        };
        // Directed messages echo to every session of the sender (multi-device), channel
        // messages already include the sender through channel membership.
        if message.channel_id.is_none() && message.from_user_id != SYSTEM_USER {
            for sid in self.sessions_of_user(app_id, message.from_user_id) {
                if !recipients.contains(&sid) {
                    recipients.push(sid);
                }
            }
        }
        let echo = ControlMessage::ChatMessageReceived {
            message: message.clone(),
        };
        let plain = ControlMessage::ChatMessageReceived {
            message: ChatMessage {
                client_ref: None,
                ..message.clone()
            },
        };
        let (Ok(echo_json), Ok(plain_json)) =
            (serde_json::to_string(&echo), serde_json::to_string(&plain))
        else {
            return;
        };
        for sid in recipients {
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            // Channel membership is per node and tenant-checked at join; the app filter here
            // guards the (theoretical) case of a channel UUID reused across tenants.
            if conn.app_id != app_id {
                continue;
            }
            let is_sender = Some(sid) == from_session_id;
            if !is_sender
                && conn.user_id != message.from_user_id
                && !self.text_allowed(&sid, &message.from_user_id)
            {
                continue;
            }
            let json = if is_sender { &echo_json } else { &plain_json };
            let _ = conn.tx.try_send(json.clone());
        }
    }

    fn deliver_typing(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        session_id: SessionId,
        typing: bool,
    ) {
        let Ok(json) = serde_json::to_string(&ControlMessage::ParticipantTyping {
            channel_id,
            user_id,
            typing,
        }) else {
            return;
        };
        let Some(members) = self.channel_members.get(&channel_id) else {
            return;
        };
        for sid in members.iter() {
            if *sid == session_id {
                continue;
            }
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id
                || conn.user_id == user_id
                || !self.text_allowed(&sid, &user_id)
            {
                continue;
            }
            let _ = conn.tx.try_send(json.clone());
        }
    }

    /// A transcript goes to the channel's local members of the same tenant that did not opt
    /// out; the speaker gets their own (captions), a receiver who blocked or locally muted the
    /// speaker does not (they would not hear them either).
    fn deliver_transcript(&self, app_id: AppId, transcript: aurix_common::protocol::Transcript) {
        let channel_id = transcript.channel_id;
        let speaker = transcript.user_id;
        let Ok(json) = serde_json::to_string(&ControlMessage::Transcript { transcript }) else {
            return;
        };
        let Some(members) = self.channel_members.get(&channel_id) else {
            return;
        };
        for sid in members.iter() {
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id || !conn.transcripts {
                continue;
            }
            if conn.user_id != speaker && !self.hears(&sid, &channel_id, &speaker) {
                continue;
            }
            let _ = conn.tx.try_send(json.clone());
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
                    session_id,
                    user_id,
                    channels,
                    speaking,
                } => {
                    // Published (not just broadcast) so members hosted on other nodes see
                    // the indicator too; local delivery happens in the ServerEvent fan-out.
                    let Some(app_id) = self.connections.get(&session_id).map(|c| c.app_id) else {
                        continue;
                    };
                    for channel_id in channels {
                        self.control
                            .events
                            .publish(ServerEvent::ParticipantSpeaking {
                                app_id,
                                channel_id,
                                user_id,
                                speaking,
                            });
                    }
                }
                MediaEvent::ChannelEnergy {
                    app_id,
                    channel_id,
                    levels,
                } => {
                    self.control.events.publish(ServerEvent::ChannelEnergy {
                        app_id,
                        channel_id,
                        levels: levels
                            .into_iter()
                            .map(|(user_id, level)| ParticipantEnergy {
                                user_id,
                                energy: decode_audio_level(level),
                            })
                            .collect(),
                    });
                }
                MediaEvent::NetworkQuality {
                    session_id,
                    app_id,
                    user_id,
                    quality,
                } => {
                    self.send_to_session(&session_id, &ControlMessage::NetworkQuality { quality });
                    if quality.uplink_loss_percent > UPLINK_LOSS_ALERT_PERCENT {
                        self.control.events.publish(ServerEvent::QualityAlert {
                            app_id,
                            session_id,
                            user_id,
                            metric: "uplink_packet_loss".into(),
                            value: quality.uplink_loss_percent as f64,
                            threshold: UPLINK_LOSS_ALERT_PERCENT as f64,
                            timestamp: chrono::Utc::now(),
                        });
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
        let Some((user_id, app_id, tx)) = self
            .connections
            .get(&session_id)
            .map(|c| (c.user_id, c.app_id, c.tx.clone()))
        else {
            return;
        };
        let was_member = self
            .channel_members
            .get(&channel_id)
            .map(|m| m.contains(&session_id))
            .unwrap_or(false);
        let left = {
            let sfu = self.sfu.read();
            sfu.leave_channel(&session_id, &channel_id)
                .unwrap_or_default()
        };
        self.index_leave(&channel_id, &session_id);
        if left.transmission_reset {
            send_msg(
                &tx,
                &ControlMessage::TransmissionChanged {
                    mode: TransmissionMode::None,
                },
            )
            .await;
        }
        if left.focus_reset {
            send_msg(
                &tx,
                &ControlMessage::ChannelFocusChanged { channel_id: None },
            )
            .await;
        }
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
        let count = self
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
        if let Ok(0) = count {
            self.control.channel_emptied(app_id, channel_id).await;
        }
    }
}

#[derive(Deserialize, Default)]
pub struct WsQuery {
    /// Deprecated fallback for clients that cannot set headers or sub-protocols. Tokens in URLs
    /// end up in proxy/access logs; prefer the `bearer.<jwt>` sub-protocol.
    pub token: Option<String>,
    /// `<session_id>.<resume_token>` fallback for clients that cannot set headers/sub-protocols.
    pub resume: Option<String>,
}

struct WsCredentials {
    token: String,
    echo_subprotocol: bool,
    resume: Option<(SessionId, String)>,
}

fn parse_resume(value: &str) -> Option<(SessionId, String)> {
    let (sid, tok) = value.split_once('.')?;
    let sid = uuid::Uuid::parse_str(sid).ok()?;
    (!tok.is_empty()).then(|| (SessionId::from_uuid(sid), tok.to_string()))
}

/// Extracts the JWT from (in order) `Authorization: Bearer`, the `bearer.<jwt>` sub-protocol,
/// or the `token` query parameter, plus the optional resume credential from the
/// `X-Aurix-Resume` header, the `resume.<sid>.<token>` sub-protocol or the `resume` query.
fn extract_ws_credentials(headers: &HeaderMap, query: &WsQuery) -> Option<WsCredentials> {
    let mut resume = headers
        .get(RESUME_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_resume);
    let mut token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let mut echo_subprotocol = false;
    if let Some(protocols) = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
    {
        for p in protocols.split(',').map(str::trim) {
            if p == AURIX_SUBPROTOCOL {
                echo_subprotocol = true;
            } else if let Some(t) = p.strip_prefix(BEARER_SUBPROTOCOL_PREFIX) {
                token.get_or_insert_with(|| t.to_string());
            } else if let Some(r) = p.strip_prefix(RESUME_SUBPROTOCOL_PREFIX) {
                if resume.is_none() {
                    resume = parse_resume(r);
                }
            }
        }
    }
    let token = token.or_else(|| query.token.clone())?;
    if resume.is_none() {
        resume = query.resume.as_deref().and_then(parse_resume);
    }
    Some(WsCredentials {
        token,
        echo_subprotocol,
        resume,
    })
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
    let Some(creds) = extract_ws_credentials(&headers, &query) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let ip = peer_ip(&state, &headers, peer.map(|p| p.0));
    let (validated, one_time) = match state
        .control
        .authenticate_session(&creds.token, &ip.to_string())
        .await
    {
        Ok((v, _node, one_time)) => (v, one_time),
        Err(AurixError::UserBanned(_)) | Err(AurixError::ActionTokenRequired(_)) => {
            return StatusCode::FORBIDDEN.into_response()
        }
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
    // A `login` action token opens exactly one fresh session. It is claimed on a resume as
    // well (so a token presented for a resume cannot open a second session later), and an
    // already-spent token is honoured only for reattaching a session it proves ownership of.
    let mut fresh_allowed = true;
    if let Some(claim) = one_time {
        match state.control.consume_login(&claim).await {
            Ok(()) => {}
            Err(AurixError::TokenReused) if creds.resume.is_some() => fresh_allowed = false,
            Err(AurixError::TokenReused) => return StatusCode::UNAUTHORIZED.into_response(),
            Err(e) => {
                warn!("login token claim failed: {e}");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        }
    }
    // Claimed before the upgrade so two racing reconnects cannot both take the session.
    let resume = creds
        .resume
        .filter(|(sid, tok)| state.claim_resume(*sid, tok, &validated))
        .map(|(sid, _)| sid);
    if !fresh_allowed && resume.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ws = ws.max_message_size(MAX_TEXT_FRAME);
    let ws = if creds.echo_subprotocol {
        ws.protocols([AURIX_SUBPROTOCOL])
    } else {
        ws
    };
    ws.on_upgrade(move |socket| {
        handle_ws_connection(socket, state, validated, ip, user_agent, resume)
    })
}

async fn send_direct(
    ws_sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &ControlMessage,
) {
    let _ = ws_sender
        .send(Message::Text(
            serde_json::to_string(msg).unwrap_or_default(),
        ))
        .await;
}

/// Reattaches a claimed (detached) session to a new socket. `None` if its media session is
/// gone in the meantime (SFU timeout), in which case the caller opens a fresh session.
fn reattach_session(
    state: &WsState,
    session_id: SessionId,
    tx: &mpsc::Sender<String>,
    resume_hash: [u8; 32],
) -> Option<Attached> {
    let media = {
        let sfu = state.sfu.read();
        sfu.get_session(&session_id).filter(|s| s.is_active())
    }?;
    let mut conn = state.connections.get_mut(&session_id)?;
    conn.tx = tx.clone();
    conn.resume_token_hash = resume_hash;
    aurix_metrics::WS_SESSIONS_RESUMED.inc();
    Some(Attached {
        session_id,
        ssrc: media.ssrc,
        media_key: media.media_key,
        resumed_channels: Some(conn.channels.clone()),
    })
}

async fn open_session(
    state: &WsState,
    token: &ValidatedToken,
    ip: IpAddr,
    user_agent: Option<&str>,
    tx: &mpsc::Sender<String>,
    resume_hash: [u8; 32],
) -> Result<Attached, AurixError> {
    // One live session per user and node (`Sfu::create_session` tears down the media side of
    // any previous one): close its control side too so channels and DB memberships follow.
    for old in state.sessions_of_user(token.app_id, token.user_id) {
        if state.take_detached(old) {
            cleanup_connection(state, old, token.user_id, "replaced").await;
        } else {
            state.request_close(old, "replaced");
        }
    }

    let session_id = SessionId::new();
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
    let media_session = created?;
    match state
        .control
        .blocks
        .load_for_user(token.app_id, token.user_id)
        .await
    {
        Ok(blocks) => media_session
            .prefs
            .write()
            .load_blocks(blocks.blocked, blocks.blocked_by),
        Err(e) => warn!("block list load failed for {}: {e}", token.user_id),
    }

    if let Err(e) = state
        .control
        .sessions
        .create_session(
            session_id,
            token.user_id,
            token.app_id,
            state.control.node_id,
            &ip.to_string(),
            user_agent,
        )
        .await
    {
        warn!("session persistence failed: {e}");
        {
            let sfu = state.sfu.read();
            let _ = sfu.destroy_session(&session_id);
        }
        return Err(AurixError::Internal("Session could not be created".into()));
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
            resume_token_hash: resume_hash,
            detached_since: None,
            generation: 0,
            closing: None,
            transcripts: true,
        },
    );
    Ok(Attached {
        session_id,
        ssrc: media_session.ssrc,
        media_key: media_session.media_key,
        resumed_channels: None,
    })
}

fn receiver_preferences(state: &WsState, session_id: &SessionId) -> Option<ControlMessage> {
    let session = state.sfu.read().get_session(session_id)?;
    let transmission = session.transmission();
    let prefs = session.prefs.read();
    Some(ControlMessage::ReceiverPreferences {
        blocked_users: prefs.blocked_users(),
        local_mutes: prefs
            .local_mutes()
            .into_iter()
            .map(|(user_id, channel_id)| LocalMute {
                user_id,
                channel_id,
            })
            .collect(),
        volumes: prefs
            .gains()
            .into_iter()
            .map(|(user_id, volume)| ParticipantVolume { user_id, volume })
            .collect(),
        transmission,
        focus_channel: prefs.focus(),
    })
}

fn channel_snapshot(
    state: &WsState,
    session_id: &SessionId,
    channel_id: &ChannelId,
) -> Vec<ParticipantBrief> {
    let sfu = state.sfu.read();
    let Some(channel) = sfu.get_channel(channel_id) else {
        return Vec::new();
    };
    channel
        .get_all_participants()
        .into_iter()
        .filter(|s| s.session_id != *session_id)
        .map(|s| ParticipantBrief {
            user_id: s.user_id,
            display_name: s.display_name.clone(),
            ssrc: s.ssrc,
            role: channel.get_role(&s.user_id),
            is_muted: s.is_muted.load(Ordering::Relaxed)
                || s.is_server_muted.load(Ordering::Relaxed),
            is_speaking: s.is_speaking.load(Ordering::Relaxed),
        })
        .collect()
}

async fn handle_ws_connection(
    socket: WebSocket,
    state: WsState,
    token: ValidatedToken,
    ip: IpAddr,
    user_agent: Option<String>,
    resume: Option<SessionId>,
) {
    state.start_fanout();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(OUTBOUND_QUEUE);
    let grace = Duration::from_secs(state.control.config.server.session_resume_grace_secs);
    let resume_token = match ResumeToken::generate() {
        Ok(t) => t,
        Err(e) => {
            warn!("resume token generation failed: {e}");
            send_direct(
                &mut ws_sender,
                &ControlMessage::Error {
                    code: "INTERNAL_ERROR".into(),
                    message: "Session could not be created".into(),
                    client_ref: None,
                },
            )
            .await;
            let _ = ws_sender.close().await;
            return;
        }
    };

    let mut attached = None;
    if let Some(sid) = resume {
        attached = reattach_session(&state, sid, &tx, resume_token.hash);
        if attached.is_none() {
            // Claimed, but the media session died meanwhile: release it and start over.
            cleanup_connection(&state, sid, token.user_id, "resume_failed").await;
        }
    }
    let attached = match attached {
        Some(a) => a,
        None => match open_session(
            &state,
            &token,
            ip,
            user_agent.as_deref(),
            &tx,
            resume_token.hash,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                send_direct(
                    &mut ws_sender,
                    &ControlMessage::Error {
                        code: e.error_code().into(),
                        message: e.public_message(),
                        client_ref: None,
                    },
                )
                .await;
                let _ = ws_sender.close().await;
                return;
            }
        },
    };
    let session_id = attached.session_id;
    let resumed = attached.resumed_channels.is_some();

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
        ssrc: attached.ssrc,
        media_addr,
        media_key: base64::engine::general_purpose::STANDARD.encode(attached.media_key),
        resume_token: resume_token.token,
        resume_grace_ms: grace.as_millis() as u64,
        resumed,
    };
    if tx
        .send(serde_json::to_string(&init_ack).unwrap_or_default())
        .await
        .is_err()
    {
        cleanup_connection(&state, session_id, token.user_id, "send_failed").await;
        return;
    }
    if let Some(prefs) = receiver_preferences(&state, &session_id) {
        send_msg(&tx, &prefs).await;
    }
    // A resumed client reconciles its channel state from one ChannelJoinAck per channel.
    for channel_id in attached.resumed_channels.iter().flatten() {
        send_msg(
            &tx,
            &ControlMessage::ChannelJoinAck {
                channel_id: *channel_id,
                participants: channel_snapshot(&state, &session_id, channel_id),
                transcription: channel_transcribes(&state, channel_id),
            },
        )
        .await;
        if let Some(rec) = &state.recording {
            for capture in rec.active_in_channel(channel_id) {
                send_msg(
                    &tx,
                    &ControlMessage::RecordingNotification {
                        channel_id: *channel_id,
                        recording_id: capture.id,
                        active: true,
                        initiated_by: capture.initiated_by,
                        live: capture.live,
                    },
                )
                .await;
            }
        }
    }
    aurix_metrics::WS_CONNECTIONS.inc();
    info!(
        "WS session {} {} for user {} ({})",
        session_id,
        if resumed { "resumed" } else { "opened" },
        token.user_id,
        ip
    );

    let send_task = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) if m == CLOSE_SENTINEL => { let _ = ws_sender.close().await; break; }
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
    let recv_ip = ip;
    let recv_task = tokio::spawn(async move {
        loop {
            let frame = tokio::time::timeout(IDLE_TIMEOUT, ws_receiver.next()).await;
            let msg = match frame {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(_))) | Ok(None) => return Disconnect::Dropped,
                Err(_) => {
                    debug!("WS session {session_id} idle timeout");
                    return Disconnect::Dropped;
                }
            };
            match msg {
                Message::Text(text) => match serde_json::from_str::<ControlMessage>(&text) {
                    Ok(ControlMessage::SessionClose { .. }) => return Disconnect::ClientClose,
                    Ok(cm) => {
                        handle_control_message(
                            &recv_state,
                            session_id,
                            &recv_token,
                            recv_ip,
                            cm,
                            &recv_tx,
                        )
                        .await
                    }
                    Err(_) => {
                        send_error(&recv_tx, "VALIDATION_ERROR", "Malformed control message").await
                    }
                },
                Message::Close(_) => return Disconnect::ClientClose,
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

    let disconnect = tokio::select! {
        _ = send_task => Disconnect::Dropped,
        r = recv_task => r.unwrap_or(Disconnect::Dropped),
    };
    redis_task.abort();
    aurix_metrics::WS_CONNECTIONS.dec();

    let closing = state
        .connections
        .get(&session_id)
        .map(|c| c.closing.clone());
    match closing {
        // Server-initiated close (ban, shutdown) or the entry is already gone.
        Some(Some(reason)) => cleanup_connection(&state, session_id, token.user_id, &reason).await,
        None => cleanup_connection(&state, session_id, token.user_id, "disconnected").await,
        Some(None) => match disconnect {
            Disconnect::Dropped if !grace.is_zero() => {
                state.detach(session_id, token.user_id, grace);
            }
            _ => cleanup_connection(&state, session_id, token.user_id, "disconnected").await,
        },
    }
}

async fn cleanup_connection(state: &WsState, session_id: SessionId, user_id: UserId, reason: &str) {
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
    state.control.chat.forget_session(session_id);
    state.control.speech.cancel_for_session(session_id, None);
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
    info!(
        "WS session {} closed for user {} ({})",
        session_id, user_id, reason
    );
}

async fn send_error(tx: &mpsc::Sender<String>, code: &str, message: &str) {
    send_error_ref(tx, code, message, None).await;
}

async fn send_error_ref(
    tx: &mpsc::Sender<String>,
    code: &str,
    message: &str,
    client_ref: Option<String>,
) {
    let err = ControlMessage::Error {
        code: code.into(),
        message: message.into(),
        client_ref,
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

/// Whether speech in `channel_id` is transcribed on this node (channel opt-in and STT configured).
fn channel_transcribes(state: &WsState, channel_id: &ChannelId) -> bool {
    state.control.config.stt.enabled
        && state
            .sfu
            .read()
            .get_channel(channel_id)
            .is_some_and(|c| c.config.transcription)
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

/// Authorization and flow control for a client chat message, then hand-off to `ChatService`.
/// Channel messages need active membership; directed ones an online target in the same app
/// (live-only: no offline delivery) with no block between the two users.
async fn send_chat(
    state: &WsState,
    session_id: SessionId,
    msg: OutgoingMessage,
) -> Result<(), AurixError> {
    let chat = &state.control.chat;
    if !chat.enabled() {
        return Err(AurixError::ChatDisabled);
    }
    match (msg.channel_id, msg.to_user_id) {
        (Some(channel_id), _) => {
            if channel_role(state, &session_id, &channel_id).is_none() {
                return Err(AurixError::AuthorizationDenied(
                    "Not a member of this channel".into(),
                ));
            }
        }
        (None, Some(to)) if to == msg.from_user_id => {
            return Err(AurixError::Validation("Cannot message yourself".into()));
        }
        (None, Some(to)) => {
            let blocked = {
                let sfu = state.sfu.read();
                sfu.get_session(&session_id)
                    .map(|s| s.prefs.read().is_blocked_either_way(&to))
                    .unwrap_or(false)
            };
            if blocked {
                return Err(AurixError::AuthorizationDenied(
                    "Cannot message this user".into(),
                ));
            }
        }
        (None, None) => return Err(AurixError::Validation("No recipient".into())),
    }
    chat_sender_allowed(state, &session_id)?;
    chat.validate(&msg.text, msg.metadata.as_ref())?;
    chat.check_flood(session_id)?;
    if let Some(to) = msg.to_user_id {
        // Sessions of every node are in the DB, so this covers targets on other nodes.
        let online = state
            .control
            .sessions
            .get_active_sessions_for_user(to)
            .await?
            .iter()
            .any(|s| s.app_id == msg.app_id.0);
        if !online {
            return Err(AurixError::UserOffline);
        }
    }
    chat.accept(msg).await.map(|_| ())
}

/// A server-muted participant may not send text when `chat.server_mute_blocks_text` is set.
fn chat_sender_allowed(state: &WsState, session_id: &SessionId) -> Result<(), AurixError> {
    if !state.control.config.chat.server_mute_blocks_text {
        return Ok(());
    }
    sender_not_server_muted(
        state,
        session_id,
        "Server-muted participants cannot send text",
    )
}

fn sender_not_server_muted(
    state: &WsState,
    session_id: &SessionId,
    message: &str,
) -> Result<(), AurixError> {
    let muted = {
        let sfu = state.sfu.read();
        sfu.get_session(session_id)
            .map(|s| s.is_server_muted.load(Ordering::Relaxed))
            .unwrap_or(false)
    };
    if muted {
        Err(AurixError::UserMuted(message.into()))
    } else {
        Ok(())
    }
}

struct SpeakArgs {
    channel_id: Option<ChannelId>,
    text: String,
    voice: Option<String>,
    destination: TtsDestination,
    client_ref: Option<String>,
}

/// A player's `TtsSpeak`: resolve the target channel (explicit, or the single channel the
/// session transmits to), check membership and server mute, then hand over to
/// `SpeechService` for policy and synthesis.
async fn speak(
    state: &WsState,
    session_id: SessionId,
    token: &ValidatedToken,
    args: SpeakArgs,
) -> Result<uuid::Uuid, AurixError> {
    let speech = &state.control.speech;
    if !speech.enabled() {
        return Err(AurixError::TtsDisabled);
    }
    let session = {
        let sfu = state.sfu.read();
        sfu.get_session(&session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?
    };
    let channel_id = match args.channel_id {
        Some(id) => id,
        None => {
            let mut targets = session
                .get_channels()
                .into_iter()
                .filter(|c| session.transmits_to(c));
            let first = targets
                .next()
                .ok_or_else(|| AurixError::Validation("Session transmits to no channel".into()))?;
            if targets.next().is_some() {
                return Err(AurixError::Validation(
                    "Session transmits to several channels; specify channel_id".into(),
                ));
            }
            first
        }
    };
    if channel_role(state, &session_id, &channel_id).is_none() {
        return Err(AurixError::AuthorizationDenied(
            "Not a member of this channel".into(),
        ));
    }
    let (to_channel, to_self) = match args.destination {
        TtsDestination::Channel => (true, false),
        TtsDestination::Local => (false, true),
        TtsDestination::Both => (true, true),
    };
    if to_channel {
        sender_not_server_muted(
            state,
            &session_id,
            "Server-muted participants cannot speak in the channel",
        )?;
    }
    speech
        .speak_as_participant(ParticipantSpeak {
            app_id: token.app_id,
            channel_id,
            session,
            display_name: token.display_name.clone(),
            text: args.text,
            voice: args.voice,
            to_channel,
            to_self,
            client_ref: args.client_ref,
        })
        .await
}

async fn handle_control_message(
    state: &WsState,
    session_id: SessionId,
    token: &ValidatedToken,
    ip: IpAddr,
    msg: ControlMessage,
    tx: &mpsc::Sender<String>,
) {
    match msg {
        ControlMessage::ChannelJoin {
            channel_id,
            token: join_token,
        } => {
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
            // Permissions come from a one-time `join` action token when the client presents
            // one, otherwise from the session JWT's channel claims (unless the server
            // requires action tokens).
            let (perms, claim) = if JwtService::is_action_token(&join_token) {
                let v = match state
                    .control
                    .action_tokens
                    .verify(&join_token, &[ActionKind::Join])
                {
                    Ok(v) => v,
                    Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
                };
                if v.app_id != token.app_id || v.user_id != token.user_id {
                    return send_error(tx, "AUTH_DENIED", "Join token is for another user").await;
                }
                if v.channel_id != Some(channel_id) {
                    return send_error(tx, "AUTH_DENIED", "Join token is for another channel")
                        .await;
                }
                let claim = ActionTokenService::pending(&v);
                (v.channel_permission().into_iter().collect(), Some(claim))
            } else if state.control.action_tokens.required() {
                return send_error(
                    tx,
                    "ACTION_TOKEN_REQUIRED",
                    "This server requires a 'join' action token",
                )
                .await;
            } else {
                (token.channels.clone(), None)
            };
            if let Err(e) = state.control.rbac.check_channel_join(&channel_id, &perms) {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            // Tenant check + persisted configuration (limits, spatial settings, codec). An
            // ad-hoc grant creates the channel on first join.
            let ad_hoc = perms
                .iter()
                .find(|p| p.channel_id == channel_id)
                .and_then(|p| p.ad_hoc.as_ref());
            let resolved = match state
                .control
                .channels
                .resolve_join(token.app_id, channel_id, ad_hoc)
                .await
            {
                Ok(c) => c,
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            };
            let config = resolved.config;
            let channel_type = config.channel_type;
            if resolved.created {
                state.control.events.publish(ServerEvent::ChannelCreated {
                    app_id: token.app_id,
                    channel_id,
                    channel_type,
                    timestamp: chrono::Utc::now(),
                });
            }
            let role = state
                .control
                .rbac
                .role_from_permissions(&channel_id, &perms);
            if let Some(claim) = claim {
                if let Err(e) = state.control.action_tokens.consume(&claim).await {
                    if resolved.created {
                        state.control.release_ad_hoc(token.app_id, channel_id).await;
                    }
                    return send_error(tx, e.error_code(), &e.public_message()).await;
                }
            }
            let join_result = {
                let sfu = state.sfu.read();
                sfu.join_channel(&session_id, channel_id, config, role)
            };
            let existing = match join_result {
                Ok(existing) => existing,
                Err(e) => {
                    if resolved.created {
                        state.control.release_ad_hoc(token.app_id, channel_id).await;
                    }
                    return send_error(tx, e.error_code(), &e.public_message()).await;
                }
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
                if resolved.created {
                    state.control.release_ad_hoc(token.app_id, channel_id).await;
                }
                return send_error(tx, "INTERNAL_ERROR", "Channel join could not be persisted")
                    .await;
            }
            state.index_join(channel_id, session_id);
            let count = state
                .control
                .channels
                .update_participant_count(channel_id, 1)
                .await;
            if let Ok(1) = count {
                state.control.events.publish(ServerEvent::ChannelActivated {
                    app_id: token.app_id,
                    channel_id,
                    timestamp: chrono::Utc::now(),
                });
            }
            if resolved.ad_hoc && !resolved.created {
                match state
                    .control
                    .channels
                    .revive_if_released(token.app_id, channel_id)
                    .await
                {
                    Ok(true) => state.control.events.publish(ServerEvent::ChannelCreated {
                        app_id: token.app_id,
                        channel_id,
                        channel_type,
                        timestamp: chrono::Utc::now(),
                    }),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("ad-hoc channel revive check failed: {e}"),
                }
            }
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
                    transcription: channel_transcribes(state, &channel_id),
                },
            )
            .await;
            // Active recordings in the channel must be disclosed to the newcomer.
            if let Some(rec) = &state.recording {
                for capture in rec.active_in_channel(&channel_id) {
                    send_msg(
                        tx,
                        &ControlMessage::RecordingNotification {
                            channel_id,
                            recording_id: capture.id,
                            active: true,
                            initiated_by: capture.initiated_by,
                            live: capture.live,
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
                    ssrc: state
                        .connections
                        .get(&session_id)
                        .map(|c| c.ssrc)
                        .unwrap_or(0),
                    timestamp: chrono::Utc::now(),
                });
        }

        ControlMessage::ModerateParticipant {
            channel_id,
            user_id,
            action,
            token: action_token,
            reason,
        } => {
            let v = match state.control.action_tokens.verify(
                &action_token,
                &[ActionKind::Kick, ActionKind::Mute, ActionKind::Unmute],
            ) {
                Ok(v) => v,
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            };
            if v.action != action
                || v.app_id != token.app_id
                || v.user_id != token.user_id
                || v.channel_id != Some(channel_id)
                || v.target_user_id != Some(user_id)
            {
                return send_error(
                    tx,
                    "AUTH_DENIED",
                    "Action token does not match this request",
                )
                .await;
            }
            if let Err(e) = state
                .control
                .action_tokens
                .consume(&ActionTokenService::pending(&v))
                .await
            {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            let target = ModerationTarget {
                app_id: token.app_id,
                channel_id,
                user_id,
                actor: token.user_id,
                ip: Some(ip.to_string()),
            };
            let result = match action {
                ActionKind::Kick => moderation_actions::kick_from_channel(
                    &state.control,
                    &state.sfu,
                    target,
                    reason.unwrap_or_else(|| "kicked by moderator".into()),
                )
                .await
                .map(|_| ()),
                ActionKind::Mute | ActionKind::Unmute => {
                    moderation_actions::set_server_mute(
                        &state.control,
                        &state.sfu,
                        target,
                        action == ActionKind::Mute,
                    )
                    .await
                }
                ActionKind::Login | ActionKind::Join => unreachable!("filtered by verify"),
            };
            match result {
                Ok(()) => {
                    send_msg(
                        tx,
                        &ControlMessage::ModerateParticipantAck {
                            channel_id,
                            user_id,
                            action,
                        },
                    )
                    .await
                }
                Err(e) => send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::SetParticipantMute {
            user_id,
            channel_id,
            muted,
        } => {
            if user_id == token.user_id {
                return send_error(
                    tx,
                    "VALIDATION_ERROR",
                    "Use MuteStateChanged to mute yourself",
                )
                .await;
            }
            if let Some(ch) = channel_id {
                let member = state
                    .connections
                    .get(&session_id)
                    .map(|c| c.channels.contains(&ch))
                    .unwrap_or(false);
                if !member {
                    return send_error(tx, "NOT_IN_CHANNEL", "Not a member of this channel").await;
                }
            }
            let sfu = state.sfu.read();
            if let Some(s) = sfu.get_session(&session_id) {
                s.prefs.write().set_muted(user_id, channel_id, muted);
            }
        }

        ControlMessage::SetParticipantVolume { user_id, volume } => {
            if user_id == token.user_id {
                return send_error(tx, "VALIDATION_ERROR", "Cannot set your own volume").await;
            }
            if !volume.is_finite() || !(0.0..=MAX_PARTICIPANT_GAIN).contains(&volume) {
                return send_error(
                    tx,
                    "VALIDATION_ERROR",
                    &format!("volume must be within 0.0..={MAX_PARTICIPANT_GAIN}"),
                )
                .await;
            }
            let sfu = state.sfu.read();
            if let Some(s) = sfu.get_session(&session_id) {
                s.prefs.write().set_gain(user_id, volume);
            }
        }

        ControlMessage::SetTransmission { mode } => {
            let result = {
                let sfu = state.sfu.read();
                match sfu.get_session(&session_id) {
                    Some(s) => s.set_transmission(mode),
                    None => Err(AurixError::SessionNotFound(session_id.to_string())),
                }
            };
            match result {
                Ok(()) => send_msg(tx, &ControlMessage::TransmissionChanged { mode }).await,
                Err(AurixError::ChannelNotFound(_)) => {
                    return send_error(tx, "NOT_IN_CHANNEL", "Not a member of this channel").await
                }
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::SetChannelFocus { channel_id } => {
            let result = {
                let sfu = state.sfu.read();
                match sfu.get_session(&session_id) {
                    Some(s) => s.set_focus(channel_id),
                    None => Err(AurixError::SessionNotFound(session_id.to_string())),
                }
            };
            match result {
                Ok(()) => send_msg(tx, &ControlMessage::ChannelFocusChanged { channel_id }).await,
                Err(AurixError::ChannelNotFound(_)) => {
                    return send_error(tx, "NOT_IN_CHANNEL", "Not a member of this channel").await
                }
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::SetUserBlock { user_id, blocked } => {
            if user_id == token.user_id {
                return send_error(tx, "VALIDATION_ERROR", "Cannot block yourself").await;
            }
            let key = format!("block:{}", token.user_id);
            if state.control.config.rate_limiting.enabled
                && !state.control.rate_limiter.check_with_cost(&key, 1.0)
            {
                return send_error(tx, "RATE_LIMIT_EXCEEDED", "Too many block changes").await;
            }
            let res = if blocked {
                state
                    .control
                    .blocks
                    .block(token.app_id, token.user_id, user_id)
                    .await
            } else {
                state
                    .control
                    .blocks
                    .unblock(token.app_id, token.user_id, user_id)
                    .await
            };
            if let Err(e) = res {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            state.control.events.publish(ServerEvent::UserBlockChanged {
                app_id: token.app_id,
                user_id: token.user_id,
                blocked_user_id: user_id,
                blocked,
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
                    sfu.update_position(
                        &up.user_id,
                        &channel_id,
                        up.position.clone(),
                        up.orientation.clone(),
                    );
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
            match rec
                .set_consent(token.app_id, recording_id, token.user_id, consent)
                .await
            {
                Ok(()) => {}
                // The capture lives on another node of a cascaded channel: hand the decision
                // over the bus; the hosting node applies it.
                Err(AurixError::NotFound(_)) => {
                    state
                        .control
                        .events
                        .publish(ServerEvent::RecordingConsentGiven {
                            app_id: token.app_id,
                            recording_id,
                            user_id: token.user_id,
                            consent,
                        });
                }
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
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
            let rtt_ms = rtt_ms.clamp(0.0, 10_000.0);
            let jitter_ms = jitter_ms.clamp(0.0, 10_000.0);
            let packet_loss = packet_loss.clamp(0.0, 100.0);
            aurix_metrics::RTT_MS.observe(rtt_ms as f64);
            aurix_metrics::JITTER_MS.observe(jitter_ms as f64);
            aurix_metrics::PACKET_LOSS_RATE.set(packet_loss as f64);
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

        ControlMessage::ChatSend {
            channel_id,
            text,
            metadata,
            client_ref,
        } => {
            let msg = OutgoingMessage {
                app_id: token.app_id,
                channel_id: Some(channel_id),
                to_user_id: None,
                from_user_id: token.user_id,
                display_name: token.display_name.clone(),
                text,
                metadata,
                client_ref: client_ref.clone(),
                from_session_id: Some(session_id),
            };
            if let Err(e) = send_chat(state, session_id, msg).await {
                send_error_ref(tx, e.error_code(), &e.public_message(), client_ref).await;
            }
        }

        ControlMessage::ChatSendDirect {
            user_id,
            text,
            metadata,
            client_ref,
        } => {
            let msg = OutgoingMessage {
                app_id: token.app_id,
                channel_id: None,
                to_user_id: Some(user_id),
                from_user_id: token.user_id,
                display_name: token.display_name.clone(),
                text,
                metadata,
                client_ref: client_ref.clone(),
                from_session_id: Some(session_id),
            };
            if let Err(e) = send_chat(state, session_id, msg).await {
                send_error_ref(tx, e.error_code(), &e.public_message(), client_ref).await;
            }
        }

        ControlMessage::ChatTyping { channel_id, typing } => {
            let chat = &state.control.chat;
            if !chat.enabled() || channel_role(state, &session_id, &channel_id).is_none() {
                return;
            }
            if chat_sender_allowed(state, &session_id).is_err() {
                return;
            }
            // "Stopped typing" always passes so a throttled start cannot leave a stale indicator.
            if typing && !chat.typing_allowed(session_id, channel_id) {
                return;
            }
            chat.publish_typing(token.app_id, channel_id, token.user_id, session_id, typing);
        }

        ControlMessage::SetTranscripts { enabled } => {
            if let Some(mut conn) = state.connections.get_mut(&session_id) {
                conn.transcripts = enabled;
            }
        }

        ControlMessage::TtsSpeak {
            channel_id,
            text,
            voice,
            destination,
            client_ref,
        } => {
            let req = SpeakArgs {
                channel_id,
                text,
                voice,
                destination,
                client_ref: client_ref.clone(),
            };
            if let Err(e) = speak(state, session_id, token, req).await {
                send_error_ref(tx, e.error_code(), &e.public_message(), client_ref).await;
            }
        }

        ControlMessage::TtsCancel => {
            state.control.speech.cancel_for_session(session_id, None);
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
        | ControlMessage::UserBlockChanged { .. }
        | ControlMessage::ReceiverPreferences { .. }
        | ControlMessage::TransmissionChanged { .. }
        | ControlMessage::ChannelFocusChanged { .. }
        | ControlMessage::BitrateCommand { .. }
        | ControlMessage::NetworkQuality { .. }
        | ControlMessage::RecordingNotification { .. }
        | ControlMessage::Error { .. }
        | ControlMessage::Kick { .. }
        | ControlMessage::ModerateParticipantAck { .. }
        | ControlMessage::ChatMessageReceived { .. }
        | ControlMessage::ParticipantTyping { .. }
        | ControlMessage::ChannelEnergy { .. }
        | ControlMessage::Transcript { .. }
        | ControlMessage::TtsStatus { .. }
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
            if event.is_realtime_noise() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(user_id: UserId, app_id: AppId) -> ValidatedToken {
        ValidatedToken {
            user_id,
            app_id,
            display_name: "p".into(),
            channels: vec![],
            metadata: None,
            jti: "j".into(),
            issued_at: 0,
        }
    }

    fn detached(user_id: UserId, app_id: AppId, token: &ResumeToken) -> ConnectionInfo {
        let (tx, _rx) = mpsc::channel(1);
        ConnectionInfo {
            tx,
            user_id,
            app_id,
            display_name: "p".into(),
            ssrc: 7,
            channels: vec![],
            resume_token_hash: token.hash,
            detached_since: Some(Instant::now()),
            generation: 1,
            closing: None,
            transcripts: true,
        }
    }

    #[test]
    fn claim_requires_detached_state_matching_identity_and_token() {
        let (user, app) = (UserId::new(), AppId::new());
        let token = ResumeToken::generate().unwrap();
        let presented = ResumeToken::hash_presented(&token.token).unwrap();

        let mut ok = detached(user, app, &token);
        assert!(ok.try_claim(&presented, &jwt(user, app)));
        assert!(ok.detached_since.is_none());
        assert_eq!(ok.generation, 2);
        // A claimed (attached) session cannot be claimed again with the same token.
        assert!(!ok.try_claim(&presented, &jwt(user, app)));

        let mut wrong_user = detached(user, app, &token);
        assert!(!wrong_user.try_claim(&presented, &jwt(UserId::new(), app)));
        assert!(wrong_user.detached_since.is_some());

        let mut wrong_app = detached(user, app, &token);
        assert!(!wrong_app.try_claim(&presented, &jwt(user, AppId::new())));

        let other = ResumeToken::generate().unwrap();
        let mut wrong_token = detached(user, app, &token);
        assert!(!wrong_token.try_claim(
            &ResumeToken::hash_presented(&other.token).unwrap(),
            &jwt(user, app)
        ));

        let mut closing = detached(user, app, &token);
        closing.closing = Some("banned".into());
        assert!(!closing.try_claim(&presented, &jwt(user, app)));

        let mut attached = detached(user, app, &token);
        attached.detached_since = None;
        assert!(!attached.try_claim(&presented, &jwt(user, app)));
    }

    #[test]
    fn concurrent_claims_admit_exactly_one() {
        let (user, app) = (UserId::new(), AppId::new());
        let token = ResumeToken::generate().unwrap();
        let sid = SessionId::new();
        let map: Arc<DashMap<SessionId, ConnectionInfo>> = Arc::new(DashMap::new());
        map.insert(sid, detached(user, app, &token));
        let jwt = Arc::new(jwt(user, app));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (map, jwt, barrier, tok) = (
                    map.clone(),
                    jwt.clone(),
                    barrier.clone(),
                    token.token.clone(),
                );
                std::thread::spawn(move || {
                    let presented = ResumeToken::hash_presented(&tok).unwrap();
                    barrier.wait();
                    map.get_mut(&sid).unwrap().try_claim(&presented, &jwt)
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn parse_resume_accepts_only_uuid_dot_token() {
        let sid = SessionId::new();
        let parsed = parse_resume(&format!("{sid}.abc-DEF_123")).unwrap();
        assert_eq!(parsed.0, sid);
        assert_eq!(parsed.1, "abc-DEF_123");
        assert!(parse_resume("").is_none());
        assert!(parse_resume("not-a-uuid.tok").is_none());
        assert!(parse_resume(&format!("{sid}.")).is_none());
        assert!(parse_resume(&sid.to_string()).is_none());
    }

    #[test]
    fn ws_credentials_prefer_header_then_subprotocol_then_query() {
        let sid = SessionId::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            format!("aurix, bearer.JWT1, resume.{sid}.sub")
                .parse()
                .unwrap(),
        );
        let query = WsQuery {
            token: Some("JWT2".into()),
            resume: Some(format!("{sid}.query")),
        };
        let c = extract_ws_credentials(&headers, &query).unwrap();
        assert_eq!(c.token, "JWT1");
        assert!(c.echo_subprotocol);
        assert_eq!(c.resume.as_ref().unwrap().1, "sub");

        headers.insert(RESUME_HEADER, format!("{sid}.header").parse().unwrap());
        let c = extract_ws_credentials(&headers, &query).unwrap();
        assert_eq!(c.resume.as_ref().unwrap().1, "header");

        let c = extract_ws_credentials(&HeaderMap::new(), &query).unwrap();
        assert_eq!(c.token, "JWT2");
        assert!(!c.echo_subprotocol);
        assert_eq!(c.resume.as_ref().unwrap().1, "query");

        assert!(extract_ws_credentials(&HeaderMap::new(), &WsQuery::default()).is_none());
    }
}
