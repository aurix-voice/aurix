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
//!
//! Cross-node failover: with Redis, every live session is mirrored (`SessionMirror`) and its
//! ownership fenced. A client that cannot reach its node reconnects to one of the
//! `SessionInitAck.failover` URLs with the same resume credential; the new node validates it
//! against the mirror, claims ownership atomically, rebuilds the media session with the same
//! session id and SSRC (new media key and endpoint), rejoins its channels and answers
//! `SessionInitAck { resumed: true, migrated: true }`. The previous node, if still alive,
//! learns about it (`SessionMigrated` or a refused mirror write) and drops its copy without
//! announcing a leave.

use aurix_auth::{JwtService, ValidatedToken};
use aurix_common::crypto::{constant_time_eq, ResumeToken};
use aurix_common::e2ee;
use aurix_common::error::AurixError;
use aurix_common::protocol::{
    decode_audio_level, ChatMessage, ControlMessage, LocalMute, ParticipantBrief,
    ParticipantEnergy, ParticipantVolume, RoleChangeReason, Transcript, TranscriptOriginal,
    TransmissionMode, TtsDestination, UserPosition,
};
use aurix_common::types::*;
use aurix_control::chat::{Actor, Conversation, OutgoingMessage, SearchQuery, SYSTEM_USER};
use aurix_control::moderation_actions::{self, ModerationTarget};
use aurix_control::session_manager::ClientInfo;
use aurix_control::{
    ActionTokenService, ControlPlane, LimitScope, ListenerTranslation, MirroredChannel,
    MirroredPrefs, ParticipantSpeak, ServerEvent, SessionMirror, TakeoverRefused,
    TranslationService, MIGRATION_SEQUENCE_GAP,
};
use aurix_media::channel::{
    JoinHints, MediaChannel, RemoteParticipant, RoleChange, RosterChange, RosterEntry,
};
use aurix_media::session::{Transport, MAX_PARTICIPANT_GAIN};
use aurix_media::tunnel::MediaTunnel;
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
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_TEXT_FRAME: usize = 64 * 1024;
/// Largest binary (tunneled AURX) frame accepted on the control socket.
const MAX_MEDIA_FRAME: usize = aurix_common::protocol::MAX_PACKET_SIZE;
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
    /// Channels joined with an explicit `priority` grant; the participant may re-enable
    /// their own priority there after a moderator demotes them.
    pub priority_grants: Vec<ChannelId>,
    pub ip: String,
    pub user_agent: Option<String>,
    resume_token_hash: [u8; 32],
    /// Set while the socket is gone and the session waits for the client to resume.
    detached_since: Option<Instant>,
    /// Bumped on every (re)attach so a stale grace timer cannot close a resumed session.
    generation: u64,
    /// Server-initiated close (ban, shutdown); such sessions are never resumable.
    closing: Option<String>,
    /// Delivers `Transcript` events for the session's channels (`SetTranscripts`).
    transcripts: bool,
    /// Target language for translated transcripts / speech (`SetTranslation`).
    translation: ListenerTranslation,
}

/// A transcript recipient who wants it in another language.
struct TranslationListener {
    session_id: SessionId,
    target: String,
    speech: bool,
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
    /// Adopted from another node (cross-node resume).
    migrated: bool,
}

/// A resume credential that names no local session but a mirrored one this node may adopt.
struct TakeoverCandidate {
    session_id: SessionId,
    mirror: SessionMirror,
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
        let st = self.clone();
        tokio::spawn(async move { st.run_quality_checkpoints().await });
    }

    /// Every `quality.persist_interval_secs`, writes the running `QualitySummary` of every
    /// live session that was rated since the previous pass to `sessions.quality_stats`, so a
    /// session lost with its node keeps its history and another node can continue it.
    async fn run_quality_checkpoints(&self) {
        let every = self.control.config.quality.persist_interval_secs;
        if every == 0 {
            return;
        }
        let mut interval = tokio::time::interval(Duration::from_secs(every));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let dirty = self.sfu.read().dirty_quality_summaries();
            for (session_id, summary) in dirty {
                let Ok(value) = serde_json::to_value(&summary) else {
                    continue;
                };
                if let Err(e) = self
                    .control
                    .sessions
                    .update_session_quality(session_id, self.control.node_id, value)
                    .await
                {
                    warn!("quality checkpoint of {session_id} failed: {e}");
                }
            }
        }
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

    fn sessions_of_app(&self, app_id: AppId) -> Vec<SessionId> {
        self.connections
            .iter()
            .filter(|e| e.value().app_id == app_id)
            .map(|e| *e.key())
            .collect()
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

    fn grant_priority(&self, channel_id: ChannelId, session_id: SessionId) {
        if let Some(mut conn) = self.connections.get_mut(&session_id) {
            if !conn.priority_grants.contains(&channel_id) {
                conn.priority_grants.push(channel_id);
            }
        }
    }

    /// `broadcast_channel_in_app` narrowed to the members that currently see `subject`
    /// (channels with a `roster_radius`; everyone otherwise).
    fn broadcast_visible(
        &self,
        app_id: AppId,
        channel_id: &ChannelId,
        subject: &UserId,
        msg: &ControlMessage,
        exclude: Option<&SessionId>,
    ) {
        let Ok(json) = serde_json::to_string(msg) else {
            return;
        };
        let Some(members) = self.channel_members.get(channel_id) else {
            return;
        };
        let channel = self.sfu.read().get_channel(channel_id);
        let scoped = channel
            .as_ref()
            .filter(|c| c.roster_radius().is_some() || c.hides_listeners());
        for sid in members.iter() {
            if Some(&*sid) == exclude {
                continue;
            }
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id {
                continue;
            }
            if scoped.is_some_and(|c| !c.sees(&conn.user_id, subject)) {
                continue;
            }
            let _ = conn.tx.try_send(json.clone());
        }
    }

    /// Sends `msg` to the channel sessions of `user_id`.
    fn send_to_member(&self, app_id: AppId, channel_id: &ChannelId, user_id: &UserId, json: &str) {
        let Some(members) = self.channel_members.get(channel_id) else {
            return;
        };
        for sid in members.iter() {
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id || conn.user_id != *user_id {
                continue;
            }
            let _ = conn.tx.try_send(json.to_string());
        }
    }

    /// Roster-radius transitions become `ParticipantJoined` / `ParticipantLeft` for the
    /// local observer of each pair.
    fn emit_roster_changes(
        &self,
        app_id: AppId,
        channel_id: &ChannelId,
        channel: &MediaChannel,
        changes: Vec<RosterChange>,
    ) {
        for change in changes {
            let msg = if change.visible {
                let Some(entry) = channel.roster_entry(&change.subject) else {
                    continue;
                };
                ControlMessage::ParticipantJoined {
                    channel_id: *channel_id,
                    user_id: entry.user_id,
                    display_name: entry.display_name,
                    ssrc: entry.ssrc,
                    role: entry.role,
                    is_muted: entry.is_muted,
                    is_priority: entry.is_priority,
                }
            } else {
                ControlMessage::ParticipantLeft {
                    channel_id: *channel_id,
                    user_id: change.subject,
                }
            };
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            self.send_to_member(app_id, channel_id, &change.observer, &json);
        }
    }

    /// Tells the local members that see `user_id` about its new effective role. Channels
    /// hiding listeners show a demotion as a leave and an admission as a join instead, since
    /// the member is (or was) invisible to them.
    #[allow(clippy::too_many_arguments)]
    fn notify_role_observers(
        &self,
        app_id: AppId,
        channel_id: &ChannelId,
        channel: &MediaChannel,
        user_id: UserId,
        role: ChannelRole,
        reason: RoleChangeReason,
        observers: Option<Vec<UserId>>,
    ) {
        let msg = if !channel.hides_listeners() {
            ControlMessage::RoleChanged {
                channel_id: *channel_id,
                user_id,
                role,
                reason,
            }
        } else if role.can_speak() {
            let Some(entry) = channel.roster_entry(&user_id) else {
                return;
            };
            ControlMessage::ParticipantJoined {
                channel_id: *channel_id,
                user_id,
                display_name: entry.display_name,
                ssrc: entry.ssrc,
                role,
                is_muted: entry.is_muted,
                is_priority: entry.is_priority,
            }
        } else {
            ControlMessage::ParticipantLeft {
                channel_id: *channel_id,
                user_id,
            }
        };
        let Ok(json) = serde_json::to_string(&msg) else {
            return;
        };
        match observers {
            Some(observers) => {
                for observer in observers {
                    if observer != user_id {
                        self.send_to_member(app_id, channel_id, &observer, &json);
                    }
                }
            }
            None => {
                let Some(members) = self.channel_members.get(channel_id) else {
                    return;
                };
                for sid in members.iter() {
                    let Some(conn) = self.connections.get(&sid) else {
                        continue;
                    };
                    if conn.app_id != app_id || conn.user_id == user_id {
                        continue;
                    }
                    let _ = conn.tx.try_send(json.clone());
                }
            }
        }
    }

    /// Applies speaker-slot changes of local members: the member learns its effective role,
    /// the members that see it are told, the membership row records whether it waits, and
    /// the fleet gets a `participant.role_changed`.
    async fn apply_role_changes(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        changes: Vec<RoleChange>,
    ) {
        if changes.is_empty() {
            return;
        }
        let Some(channel) = self.sfu.read().get_channel(&channel_id) else {
            return;
        };
        for change in changes {
            let reason = if change.role.can_speak() {
                RoleChangeReason::SpeakerAdmitted
            } else {
                RoleChangeReason::SpeakerDemoted
            };
            self.send_to_session(
                &change.session_id,
                &ControlMessage::RoleChanged {
                    channel_id,
                    user_id: change.user_id,
                    role: change.role,
                    reason,
                },
            );
            self.notify_role_observers(
                app_id,
                &channel_id,
                &channel,
                change.user_id,
                change.role,
                reason,
                change.observers,
            );
            if let Err(e) = self
                .control
                .sessions
                .set_membership_waiting_to_speak(
                    app_id,
                    channel_id,
                    change.user_id,
                    !change.role.can_speak(),
                )
                .await
            {
                warn!("membership admission update failed: {e}");
            }
            self.control.events.publish(ServerEvent::RoleChanged {
                app_id,
                channel_id,
                user_id: change.user_id,
                session_id: change.session_id,
                role: change.role,
                reason,
                timestamp: chrono::Utc::now(),
            });
        }
    }

    /// Speakers hosted on other nodes that a channel not yet live here would have to count
    /// against `max_speakers`.
    async fn remote_speaker_count(&self, app_id: AppId, channel_id: ChannelId) -> u32 {
        match self
            .control
            .sessions
            .get_channel_roster(app_id, channel_id)
            .await
        {
            Ok(rows) => rows
                .iter()
                .filter(|row| {
                    row.media_node_id != self.control.node_id.0
                        && !row.waiting_to_speak
                        && parse_role(&row.role).can_speak()
                })
                .count() as u32,
            Err(e) => {
                warn!("channel roster lookup failed: {e}");
                0
            }
        }
    }

    /// Applies poses to the SFU (audio routing, text range, roster radius), notifies roster
    /// transitions and forwards each mover's position to the local members that see them.
    fn apply_positions(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        positions: Vec<UserPosition>,
        exclude: Option<&SessionId>,
    ) {
        let Some(channel) = self.sfu.read().get_channel(&channel_id) else {
            return;
        };
        if channel.app_id != app_id {
            return;
        }
        let mut changes = Vec::new();
        for up in &positions {
            changes.extend(channel.update_position(
                &up.user_id,
                up.position.clone(),
                up.orientation.clone(),
            ));
        }
        self.emit_roster_changes(app_id, &channel_id, &channel, changes);
        let Some(members) = self.channel_members.get(&channel_id) else {
            return;
        };
        let scoped = channel.roster_radius().is_some();
        let everyone = if scoped {
            None
        } else {
            serde_json::to_string(&ControlMessage::PositionUpdate {
                channel_id,
                positions: positions.clone(),
            })
            .ok()
        };
        for sid in members.iter() {
            if Some(&*sid) == exclude {
                continue;
            }
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id {
                continue;
            }
            if let Some(json) = &everyone {
                let _ = conn.tx.try_send(json.clone());
                continue;
            }
            let visible: Vec<UserPosition> = positions
                .iter()
                .filter(|p| channel.sees(&conn.user_id, &p.user_id))
                .cloned()
                .collect();
            if visible.is_empty() {
                continue;
            }
            if let Ok(json) = serde_json::to_string(&ControlMessage::PositionUpdate {
                channel_id,
                positions: visible,
            }) {
                let _ = conn.tx.try_send(json);
            }
        }
    }

    /// Publishes the poses of this node's members of `channel` so other nodes hosting the
    /// channel can route relayed audio positionally and scope presence/text.
    fn publish_pose_snapshot(&self, app_id: AppId, channel: &MediaChannel) {
        let positions = channel.local_poses();
        if positions.is_empty() {
            return;
        }
        self.control
            .events
            .publish(ServerEvent::ParticipantPositions {
                app_id,
                channel_id: channel.channel_id,
                origin: self.control.node_id,
                positions,
            });
    }

    /// Registers channel members hosted on other nodes (from the database) so the joining
    /// participant's roster and later presence include them.
    async fn sync_remote_members(&self, app_id: AppId, channel_id: ChannelId) {
        let rows = match self
            .control
            .sessions
            .get_channel_roster(app_id, channel_id)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!("channel roster lookup failed: {e}");
                return;
            }
        };
        let Some(channel) = self.sfu.read().get_channel(&channel_id) else {
            return;
        };
        let mut added_remote = false;
        let mut changes = Vec::new();
        for row in rows {
            let session_id = SessionId::from_uuid(row.session_id);
            if row.media_node_id == self.control.node_id.0
                || self.sfu.read().get_session(&session_id).is_some()
            {
                continue;
            }
            let user_id = UserId::from_uuid(row.user_id);
            if channel.has_participant(&user_id) || channel.is_remote(&user_id) {
                continue;
            }
            added_remote = true;
            let role = if row.waiting_to_speak {
                ChannelRole::Listener
            } else {
                parse_role(&row.role)
            };
            changes.extend(channel.add_remote(
                user_id,
                RemoteParticipant {
                    session_id,
                    display_name: row.display_name,
                    ssrc: row.ssrc as u32,
                    role,
                    is_muted: row.is_muted || row.is_server_muted,
                    is_priority: row.is_priority,
                },
            ));
        }
        self.emit_roster_changes(app_id, &channel_id, &channel, changes);
        if added_remote {
            self.publish_pose_snapshot(app_id, &channel);
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
            conn.priority_grants.retain(|c| c != channel_id);
        }
    }

    /// Public WebSocket URLs of other healthy nodes, best first, for `SessionInitAck.failover`.
    fn failover_endpoints(&self) -> Vec<String> {
        self.control.nodes.failover_endpoints(
            self.control.node_id,
            self.control.config.cluster.failover_endpoints,
        )
    }

    /// Snapshot of a live session for the Redis mirror; `None` once it is closing or gone.
    fn build_mirror(&self, session_id: SessionId) -> Option<SessionMirror> {
        let (conn, channels) = {
            let conn = self.connections.get(&session_id)?;
            if conn.closing.is_some() {
                return None;
            }
            (conn.clone(), conn.channels.clone())
        };
        let sfu = self.sfu.read();
        let session = sfu.get_session(&session_id).filter(|s| s.is_active())?;
        let channels = channels
            .into_iter()
            .map(|channel_id| {
                let channel = sfu.get_channel(&channel_id);
                MirroredChannel {
                    channel_id,
                    role: channel
                        .as_ref()
                        .map(|c| c.role_of(&conn.user_id))
                        .unwrap_or(ChannelRole::Speaker),
                    priority: channel
                        .as_ref()
                        .is_some_and(|c| c.has_priority_flag(&conn.user_id)),
                }
            })
            .collect();
        let prefs = session.prefs.read();
        let mirrored = MirroredPrefs {
            local_mutes: prefs.local_mutes(),
            gains: prefs.gains(),
            transmission: session.transmission(),
            focus: prefs.focus(),
            codec: session.codec(),
            downlink: session.downlink_mode(),
            noise_suppression: session.noise_suppression(),
            muted: session.is_muted.load(Ordering::Relaxed),
            transcripts: conn.transcripts,
            translation: conn.translation.language.clone(),
            spoken_language: conn.translation.spoken_language.clone(),
            translation_speech: conn.translation.speech,
        };
        Some(SessionMirror {
            session_id,
            user_id: conn.user_id,
            app_id: conn.app_id,
            display_name: conn.display_name.clone(),
            ssrc: conn.ssrc,
            audio_seq: session.audio_sequence(),
            resume_hash: SessionMirror::resume_hash_hex(&conn.resume_token_hash),
            ip: conn.ip.clone(),
            user_agent: conn.user_agent.clone(),
            channels,
            prefs: mirrored,
            updated_at: chrono::Utc::now().timestamp_millis(),
        })
    }

    /// Writes/refreshes the session's Redis locator and mirror. A refused write means another
    /// node adopted the session while this one still held it: the local copy is dropped.
    async fn mirror_session(&self, session_id: SessionId) {
        let Some(redis) = self.control.redis.as_ref() else {
            return;
        };
        let cluster = &self.control.config.cluster;
        if !cluster.session_mirror {
            let _ = redis
                .set_session_node(session_id, self.control.node_id)
                .await;
            return;
        }
        let Some(mirror) = self.build_mirror(session_id) else {
            return;
        };
        match redis
            .write_session_mirror(&mirror, cluster.session_mirror_ttl_secs)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                let owner = redis.get_session_node(session_id).await.ok().flatten();
                self.evict_migrated(session_id, owner).await;
            }
            Err(e) => debug!("session mirror write for {session_id} failed: {e}"),
        }
    }

    /// Drops this node's copy of a session that now lives on `to`: media state and channel
    /// membership go, but nothing is announced or closed in the database — the participant is
    /// still in their channels, hosted elsewhere, so local peers keep them as a remote member.
    async fn evict_migrated(&self, session_id: SessionId, to: Option<MediaNodeId>) {
        let Some((_, conn)) = self.connections.remove(&session_id) else {
            return;
        };
        if conn.detached_since.is_some() {
            aurix_metrics::WS_SESSIONS_DETACHED.dec();
        }
        let _ = conn.tx.try_send(CLOSE_SENTINEL.to_string());
        for channel_id in &conn.channels {
            if let Some(members) = self.channel_members.get(channel_id) {
                members.remove(&session_id);
            }
            self.channel_members
                .remove_if(channel_id, |_, m| m.is_empty());
        }
        {
            let sfu = self.sfu.read();
            let is_muted = sfu.get_session(&session_id).is_some_and(|s| {
                s.is_muted.load(Ordering::Relaxed) || s.is_server_muted.load(Ordering::Relaxed)
            });
            for channel_id in &conn.channels {
                let Some(channel) = sfu.get_channel(channel_id) else {
                    continue;
                };
                let role = channel.role_of(&conn.user_id);
                let is_priority = channel.has_priority_flag(&conn.user_id);
                let _ = sfu.leave_channel(&session_id, channel_id);
                if sfu.get_channel(channel_id).is_some() {
                    channel.add_remote(
                        conn.user_id,
                        RemoteParticipant {
                            session_id,
                            display_name: conn.display_name.clone(),
                            ssrc: conn.ssrc,
                            role,
                            is_muted,
                            is_priority,
                        },
                    );
                }
            }
            let _ = sfu.destroy_session(&session_id);
        }
        self.control.chat.forget_session(session_id);
        self.control.speech.cancel_for_session(session_id, None);
        info!(
            "WS session {} for user {} migrated to {}",
            session_id,
            conn.user_id,
            to.map(|n| n.to_string())
                .unwrap_or_else(|| "another node".into())
        );
    }

    /// A resume credential for a session this node does not host: valid against the Redis
    /// mirror (tenant, user, token) means the session may be adopted here.
    async fn takeover_candidate(
        &self,
        session_id: SessionId,
        token: &str,
        jwt: &ValidatedToken,
    ) -> Option<TakeoverCandidate> {
        let redis = self.control.redis.as_ref()?;
        if !self.control.config.cluster.session_mirror || self.connections.contains_key(&session_id)
        {
            return None;
        }
        let presented = ResumeToken::hash_presented(token)?;
        let mirror = match redis.get_session_mirror(session_id).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                takeover_refused("not_mirrored");
                return None;
            }
            Err(e) => {
                debug!("session mirror lookup for {session_id} failed: {e}");
                takeover_refused("redis");
                return None;
            }
        };
        if mirror.session_id != session_id
            || mirror.app_id != jwt.app_id
            || mirror.user_id != jwt.user_id
            || !mirror.resume_hash_matches(&presented)
        {
            takeover_refused("denied");
            return None;
        }
        Some(TakeoverCandidate { session_id, mirror })
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
                    app_id,
                    channel_id,
                    user_id,
                    display_name,
                    session_id,
                    ssrc,
                    role,
                    is_priority,
                    ..
                } => {
                    let local = self.connections.get(&session_id).map(|c| c.ssrc);
                    let ssrc = local.unwrap_or(ssrc);
                    let channel = self.sfu.read().get_channel(&channel_id);
                    if let Some(channel) = &channel {
                        if local.is_none() && !channel.has_participant(&user_id) {
                            let changes = channel.add_remote(
                                user_id,
                                RemoteParticipant {
                                    session_id,
                                    display_name: display_name.clone(),
                                    ssrc,
                                    role,
                                    is_muted: false,
                                    is_priority,
                                },
                            );
                            self.emit_roster_changes(app_id, &channel_id, channel, changes);
                            self.publish_pose_snapshot(app_id, channel);
                        }
                        if channel.roster_radius().is_some() {
                            // Scoped presence: the newcomer appears to others (and others to
                            // them) once their positions put them within the roster radius.
                            continue;
                        }
                        if channel.is_hidden_listener(&user_id) {
                            continue;
                        }
                    }
                    self.broadcast_visible(
                        app_id,
                        &channel_id,
                        &user_id,
                        &ControlMessage::ParticipantJoined {
                            channel_id,
                            user_id,
                            display_name,
                            ssrc,
                            role,
                            is_muted: false,
                            is_priority,
                        },
                        Some(&session_id),
                    );
                }
                ServerEvent::ParticipantLeft {
                    app_id,
                    channel_id,
                    user_id,
                    session_id,
                    hidden,
                    ..
                } => {
                    // A leave for a session this node hosts *and* still has in the channel is
                    // stale: the lost-node reaper closed memberships the session had already
                    // brought here by cross-node resume.
                    if self
                        .channel_members
                        .get(&channel_id)
                        .is_some_and(|m| m.contains(&session_id))
                    {
                        continue;
                    }
                    if let Some(rec) = &self.recording {
                        rec.live().on_participant_left(channel_id, user_id);
                    }
                    let channel = self.sfu.read().get_channel(&channel_id);
                    if let Some(channel) = &channel {
                        if channel.is_remote(&user_id) {
                            let hidden = hidden || channel.is_hidden_listener(&user_id);
                            let freed_slot = channel.role_of(&user_id).can_speak();
                            // Remote members leave the roster of whoever saw them; local
                            // leavers were announced by `leave_channel_full`.
                            let observers = channel.remove_remote(&user_id);
                            if freed_slot {
                                let admitted = channel.admit_waiting();
                                self.apply_role_changes(app_id, channel_id, admitted).await;
                            }
                            if hidden {
                                continue;
                            }
                            if let Some(observers) = observers {
                                let msg = ControlMessage::ParticipantLeft {
                                    channel_id,
                                    user_id,
                                };
                                if let Ok(json) = serde_json::to_string(&msg) {
                                    for observer in observers {
                                        self.send_to_member(app_id, &channel_id, &observer, &json);
                                    }
                                }
                                continue;
                            }
                        } else if channel.roster_radius().is_some() {
                            continue;
                        }
                    }
                    if hidden {
                        continue;
                    }
                    self.broadcast_visible(
                        app_id,
                        &channel_id,
                        &user_id,
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
                    if let Some(channel) = self.sfu.read().get_channel(&channel_id) {
                        channel.set_remote_muted(&user_id, true);
                    }
                    self.broadcast_visible(
                        app_id,
                        &channel_id,
                        &user_id,
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
                    if let Some(channel) = self.sfu.read().get_channel(&channel_id) {
                        channel.set_remote_muted(&user_id, false);
                    }
                    self.broadcast_visible(
                        app_id,
                        &channel_id,
                        &user_id,
                        &ControlMessage::MuteStateChanged {
                            channel_id,
                            user_id,
                            muted: false,
                            server_muted: false,
                        },
                        None,
                    );
                }
                ServerEvent::PriorityChanged {
                    app_id,
                    channel_id,
                    user_id,
                    priority,
                    ..
                } => {
                    let known =
                        self.sfu.read().get_channel(&channel_id).is_some_and(|c| {
                            c.app_id == app_id && c.set_priority(&user_id, priority)
                        });
                    if !known {
                        continue;
                    }
                    self.broadcast_visible(
                        app_id,
                        &channel_id,
                        &user_id,
                        &ControlMessage::PriorityChanged {
                            channel_id,
                            user_id,
                            priority,
                        },
                        None,
                    );
                }
                ServerEvent::RoleChanged {
                    app_id,
                    channel_id,
                    user_id,
                    session_id,
                    role,
                    reason,
                    ..
                } => {
                    // Local members were told by `apply_role_changes`; this is a member on
                    // another node whose slot count and roster entry change here.
                    if self.connections.contains_key(&session_id) {
                        continue;
                    }
                    let Some(channel) = self.sfu.read().get_channel(&channel_id) else {
                        continue;
                    };
                    if channel.app_id != app_id || !channel.is_remote(&user_id) {
                        continue;
                    }
                    let observers = if role.can_speak() {
                        None
                    } else {
                        channel.observers_of(&user_id)
                    };
                    channel.set_remote_role(&user_id, role);
                    let observers = if role.can_speak() {
                        channel.observers_of(&user_id)
                    } else {
                        observers
                    };
                    self.notify_role_observers(
                        app_id,
                        &channel_id,
                        &channel,
                        user_id,
                        role,
                        reason,
                        observers,
                    );
                    if !role.can_speak() {
                        let admitted = channel.admit_waiting();
                        self.apply_role_changes(app_id, channel_id, admitted).await;
                    }
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
                ServerEvent::ChannelConfigUpdated {
                    app_id,
                    channel_id,
                    config,
                    ..
                } => {
                    let audio = config.audio_policy();
                    let ducking = config.ducking;
                    let updated =
                        self.sfu
                            .read()
                            .update_channel_config(&channel_id, &app_id, config);
                    if let Some((_, admitted)) = updated {
                        self.broadcast_channel_in_app(
                            app_id,
                            &channel_id,
                            &ControlMessage::ChannelAudioPolicy {
                                channel_id,
                                audio,
                                ducking,
                            },
                        );
                        self.apply_role_changes(app_id, channel_id, admitted).await;
                    }
                }
                ServerEvent::ChatMessage {
                    app_id,
                    message,
                    from_session_id,
                } => {
                    self.deliver_chat(app_id, message, from_session_id, |message| {
                        ControlMessage::ChatMessageReceived { message }
                    });
                }
                ServerEvent::ChatMessageUpdated {
                    app_id,
                    message,
                    from_session_id,
                } => {
                    self.deliver_chat(app_id, message, from_session_id, |message| {
                        ControlMessage::ChatMessageUpdated { message }
                    });
                }
                ServerEvent::ChatReactionChanged {
                    app_id,
                    message_id,
                    channel_id,
                    message_from_user_id,
                    message_to_user_id,
                    user_id,
                    reaction,
                    added,
                    count,
                    timestamp,
                    from_session_id: _,
                } => {
                    self.deliver_reaction(
                        app_id,
                        ControlMessage::ChatReactionChanged {
                            message_id,
                            channel_id,
                            message_from_user_id,
                            message_to_user_id,
                            user_id,
                            reaction,
                            added,
                            count,
                            timestamp,
                        },
                    );
                }
                ServerEvent::ChatReadMarker { app_id, marker } => {
                    self.deliver_read_marker(app_id, marker);
                }
                ServerEvent::E2eeRelay {
                    app_id,
                    channel_id,
                    from_user,
                    from_session,
                    to,
                    message,
                } => {
                    self.deliver_e2ee(app_id, channel_id, from_user, from_session, to, &message);
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
                    self.broadcast_visible(
                        app_id,
                        &channel_id,
                        &user_id,
                        &ControlMessage::SpeakingStateChanged {
                            channel_id,
                            user_id,
                            speaking,
                        },
                        None,
                    );
                }
                ServerEvent::ChannelEnergy {
                    app_id,
                    channel_id,
                    levels,
                } => {
                    self.deliver_energy(app_id, channel_id, levels);
                }
                ServerEvent::ParticipantPositions {
                    app_id,
                    channel_id,
                    origin,
                    positions,
                } => {
                    if origin != self.control.node_id {
                        self.apply_positions(app_id, channel_id, positions, None);
                    }
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
                        self.mirror_session(sid).await;
                    }
                }
                ServerEvent::SessionMigrated { session_id, to, .. } => {
                    if to != self.control.node_id {
                        self.evict_migrated(session_id, Some(to)).await;
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
                ServerEvent::AppDeactivated { app_id, .. } => {
                    for sid in self.sessions_of_app(app_id) {
                        self.request_close(sid, "application deactivated");
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
                ServerEvent::LiveStreamStopRequested {
                    app_id,
                    stream_id,
                    node,
                    reason,
                } => {
                    if node == self.control.node_id {
                        if let Some(rec) = self.recording.as_ref() {
                            rec.live().close(Some(app_id), stream_id, &reason);
                        }
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
                ServerEvent::ChannelDestroyed {
                    app_id, channel_id, ..
                } => {
                    let members: Vec<SessionId> = self
                        .channel_members
                        .get(&channel_id)
                        .map(|m| m.iter().map(|s| *s).collect())
                        .unwrap_or_default();
                    for sid in members {
                        self.leave_channel_full(sid, channel_id, "channel_destroyed")
                            .await;
                        self.mirror_session(sid).await;
                    }
                    if let Some(rec) = self.recording.clone() {
                        tokio::spawn(async move {
                            rec.stop_channel(app_id, &channel_id).await;
                        });
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

    /// Local delivery of an accepted chat message (or an edit / tombstone of one): channel
    /// members (or the target user's sessions) that have no block relationship with the
    /// sender, plus the sender's own echo carrying `client_ref`.
    fn deliver_chat(
        &self,
        app_id: AppId,
        message: ChatMessage,
        from_session_id: Option<SessionId>,
        wrap: impl Fn(ChatMessage) -> ControlMessage,
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
        let echo = wrap(message.clone());
        let plain = wrap(ChatMessage {
            client_ref: None,
            ..message.clone()
        });
        let (Ok(echo_json), Ok(plain_json)) =
            (serde_json::to_string(&echo), serde_json::to_string(&plain))
        else {
            return;
        };
        let text_range = message
            .channel_id
            .and_then(|c| self.sfu.read().get_channel(&c))
            .filter(|c| c.text_radius().is_some());
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
                && (!self.text_allowed(&sid, &message.from_user_id)
                    || text_range
                        .as_ref()
                        .is_some_and(|c| !c.text_reaches(&message.from_user_id, &conn.user_id)))
            {
                continue;
            }
            let json = if is_sender { &echo_json } else { &plain_json };
            let _ = conn.tx.try_send(json.clone());
        }
    }

    /// A reaction change goes to whoever sees the message: the channel's local members who
    /// see the reacting user, or both parties of a direct message — never across a block.
    fn deliver_reaction(&self, app_id: AppId, event: ControlMessage) {
        let ControlMessage::ChatReactionChanged {
            channel_id,
            message_from_user_id,
            message_to_user_id,
            user_id: reactor,
            ..
        } = &event
        else {
            return;
        };
        let (recipients, channel) = match (channel_id, message_to_user_id) {
            (Some(channel_id), _) => (
                self.channel_members
                    .get(channel_id)
                    .map(|m| m.iter().map(|s| *s).collect::<Vec<_>>())
                    .unwrap_or_default(),
                self.sfu.read().get_channel(channel_id),
            ),
            (None, Some(to)) => {
                let mut r = self.sessions_of_user(app_id, *message_from_user_id);
                for sid in self.sessions_of_user(app_id, *to) {
                    if !r.contains(&sid) {
                        r.push(sid);
                    }
                }
                (r, None)
            }
            (None, None) => return,
        };
        let Ok(json) = serde_json::to_string(&event) else {
            return;
        };
        for sid in recipients {
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id {
                continue;
            }
            if conn.user_id != *reactor
                && (!self.text_allowed(&sid, reactor)
                    || channel
                        .as_ref()
                        .is_some_and(|c| !c.sees(&conn.user_id, reactor)))
            {
                continue;
            }
            let _ = conn.tx.try_send(json.clone());
        }
    }

    /// A moved read marker goes to every session of the reader (multi-device sync) and, with
    /// `chat.read_receipts`, to the channel's local members who can see the reader / to the
    /// direct peer's sessions — never to someone with a block between them and the reader.
    fn deliver_read_marker(&self, app_id: AppId, marker: aurix_common::protocol::ChatReadMarker) {
        let reader = marker.user_id;
        let mut recipients = self.sessions_of_user(app_id, reader);
        let channel = if self.control.config.chat.read_receipts {
            match (marker.channel_id, marker.peer_user_id) {
                (Some(channel_id), _) => {
                    if let Some(members) = self.channel_members.get(&channel_id) {
                        for sid in members.iter() {
                            if !recipients.contains(&sid) {
                                recipients.push(*sid);
                            }
                        }
                    }
                    self.sfu.read().get_channel(&channel_id)
                }
                (None, Some(peer)) => {
                    for sid in self.sessions_of_user(app_id, peer) {
                        if !recipients.contains(&sid) {
                            recipients.push(sid);
                        }
                    }
                    None
                }
                (None, None) => None,
            }
        } else {
            None
        };
        let Ok(json) = serde_json::to_string(&ControlMessage::ChatReadMarker { marker }) else {
            return;
        };
        for sid in recipients {
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id {
                continue;
            }
            if conn.user_id != reader
                && (!self.text_allowed(&sid, &reader)
                    || channel
                        .as_ref()
                        .is_some_and(|c| !c.sees(&conn.user_id, &reader)))
            {
                continue;
            }
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
        let channel = self.sfu.read().get_channel(&channel_id);
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
            if let Some(c) = &channel {
                if !c.text_reaches(&user_id, &conn.user_id) || !c.sees(&conn.user_id, &user_id) {
                    continue;
                }
            }
            let _ = conn.tx.try_send(json.clone());
        }
    }

    /// Relays an E2EE key message to the channel's local members of the same tenant — every
    /// other member for a `Hello`, only `to` for a `SenderKey`. Membership is the whole
    /// authorization: key material reaches exactly the people the channel admits, regardless
    /// of radius or listener visibility (they may need to decrypt later).
    fn deliver_e2ee(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        from_user: UserId,
        from_session: SessionId,
        to: Option<UserId>,
        message: &ControlMessage,
    ) {
        let Ok(json) = serde_json::to_string(message) else {
            return;
        };
        let Some(members) = self.channel_members.get(&channel_id) else {
            return;
        };
        for sid in members.iter() {
            if *sid == from_session {
                continue;
            }
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id
                || conn.user_id == from_user
                || to.is_some_and(|t| t != conn.user_id)
            {
                continue;
            }
            let _ = conn.tx.try_send(json.clone());
        }
    }

    /// A transcript goes to the channel's local members of the same tenant that did not opt
    /// out; the speaker gets their own (captions), a receiver who blocked or locally muted the
    /// speaker does not (they would not hear them either). Members who asked for another
    /// language (`SetTranslation`) get a translated copy instead, produced off this path so
    /// the original never waits for the translator.
    fn deliver_transcript(&self, app_id: AppId, transcript: Transcript) {
        let channel_id = transcript.channel_id;
        let speaker = transcript.user_id;
        let Some(members) = self.channel_members.get(&channel_id) else {
            return;
        };
        let channel = self.sfu.read().get_channel(&channel_id);
        let translator = &self.control.translation;
        let mut originals: Vec<mpsc::Sender<String>> = Vec::new();
        let mut translated: Vec<TranslationListener> = Vec::new();
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
            if channel
                .as_ref()
                .is_some_and(|c| !c.text_reaches(&speaker, &conn.user_id))
            {
                continue;
            }
            match conn.translation.language.as_deref() {
                Some(target)
                    if translator.enabled()
                        && conn.user_id != speaker
                        && TranslationService::needs_translation(
                            transcript.language.as_deref(),
                            target,
                        ) =>
                {
                    translated.push(TranslationListener {
                        session_id: *sid,
                        target: target.to_string(),
                        speech: conn.translation.speech,
                    });
                }
                _ => originals.push(conn.tx.clone()),
            }
        }
        drop(members);
        let targets = translator.select_targets(translated.iter().map(|l| l.target.as_str()));
        // Listeners whose language lost the per-channel fan-out cap fall back to the original.
        let (translated, capped): (Vec<_>, Vec<_>) = translated
            .into_iter()
            .partition(|l| targets.contains(&l.target));
        for listener in capped {
            aurix_metrics::TRANSLATIONS
                .with_label_values(&["skipped"])
                .inc();
            if let Some(conn) = self.connections.get(&listener.session_id) {
                originals.push(conn.tx.clone());
            }
        }
        if !originals.is_empty() {
            if let Ok(json) = serde_json::to_string(&ControlMessage::Transcript {
                transcript: transcript.clone(),
            }) {
                for tx in &originals {
                    let _ = tx.try_send(json.clone());
                }
            }
        }
        if translated.is_empty() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            state
                .deliver_translations(app_id, transcript, targets, translated)
                .await;
        });
    }

    /// One machine translation per target language, then per-listener delivery: the text as a
    /// `Transcript` carrying `original`, and — for listeners who asked — spoken through the TTS
    /// engine into their downlink alone. A failed translation degrades to the original text
    /// (never to silence). Eligibility is re-checked after the round trip.
    async fn deliver_translations(
        &self,
        app_id: AppId,
        transcript: Transcript,
        targets: Vec<String>,
        listeners: Vec<TranslationListener>,
    ) {
        let channel_id = transcript.channel_id;
        let speaker = transcript.user_id;
        let translator = &self.control.translation;
        let results = futures_util::future::join_all(targets.iter().map(|target| {
            translator.translate(&transcript.text, transcript.language.as_deref(), target)
        }))
        .await;
        for (target, result) in targets.iter().zip(results) {
            let translation = match result {
                Ok(t) => Some(t),
                Err(e) => {
                    debug!(
                        "Translation of {}'s transcript into {} failed: {}",
                        speaker, target, e
                    );
                    None
                }
            };
            let mut payload = transcript.clone();
            let mut speak = false;
            if let Some(translation) = translation {
                let detected = transcript
                    .language
                    .clone()
                    .or_else(|| translation.source_language.clone());
                // The provider found the speaker already talks the listener's language.
                let same_language = transcript.language.is_none()
                    && translation
                        .source_language
                        .as_deref()
                        .is_some_and(|s| aurix_common::translate::same_language(s, target));
                if same_language {
                    payload.language = detected;
                } else {
                    payload.original = Some(TranscriptOriginal {
                        text: transcript.text.clone(),
                        language: detected,
                    });
                    payload.text = translation.text;
                    payload.language = Some(target.clone());
                    payload.words = Vec::new();
                    speak = true;
                }
            }
            let Ok(json) = serde_json::to_string(&ControlMessage::Transcript {
                transcript: payload.clone(),
            }) else {
                continue;
            };
            for listener in listeners.iter().filter(|l| &l.target == target) {
                let tx = self.connections.get(&listener.session_id).and_then(|conn| {
                    (conn.app_id == app_id
                        && conn.transcripts
                        && conn.channels.contains(&channel_id)
                        && conn.translation.language.as_deref() == Some(target.as_str()))
                    .then(|| conn.tx.clone())
                });
                let Some(tx) = tx else {
                    continue;
                };
                if !self.hears(&listener.session_id, &channel_id, &speaker) {
                    continue;
                }
                let _ = tx.try_send(json.clone());
                if !speak || !listener.speech {
                    continue;
                }
                let session = self
                    .sfu
                    .read()
                    .get_session(&listener.session_id)
                    .filter(|s| s.is_active());
                let Some(session) = session else {
                    continue;
                };
                let voice =
                    translator.voice_for(target, &self.control.speech.config().default_voice);
                if let Err(e) = self.control.speech.speak_to_listener(
                    app_id,
                    channel_id,
                    session,
                    payload.text.clone(),
                    voice,
                ) {
                    debug!(
                        "Spoken translation for {} skipped: {}",
                        listener.session_id, e
                    );
                }
            }
        }
    }

    /// Energy levels reach the members of the same tenant; with a roster radius each member
    /// only gets the levels of participants they currently see.
    fn deliver_energy(&self, app_id: AppId, channel_id: ChannelId, levels: Vec<ParticipantEnergy>) {
        let channel = self
            .sfu
            .read()
            .get_channel(&channel_id)
            .filter(|c| c.roster_radius().is_some());
        let Some(channel) = channel else {
            self.broadcast_channel_in_app(
                app_id,
                &channel_id,
                &ControlMessage::ChannelEnergy { channel_id, levels },
            );
            return;
        };
        let Some(members) = self.channel_members.get(&channel_id) else {
            return;
        };
        for sid in members.iter() {
            let Some(conn) = self.connections.get(&sid) else {
                continue;
            };
            if conn.app_id != app_id {
                continue;
            }
            let visible: Vec<ParticipantEnergy> = levels
                .iter()
                .filter(|l| channel.sees(&conn.user_id, &l.user_id))
                .cloned()
                .collect();
            if visible.is_empty() {
                continue;
            }
            if let Ok(json) = serde_json::to_string(&ControlMessage::ChannelEnergy {
                channel_id,
                levels: visible,
            }) {
                let _ = conn.tx.try_send(json);
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
                MediaEvent::SessionBound {
                    session_id,
                    transport,
                } => {
                    self.send_to_session(
                        &session_id,
                        &ControlMessage::MediaBound {
                            session_id,
                            transport,
                        },
                    );
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
                    transition,
                } => {
                    self.send_to_session(&session_id, &ControlMessage::NetworkQuality { quality });
                    let policy = &self.control.config.quality;
                    if quality.uplink_loss_percent > policy.loss_alert_percent {
                        aurix_metrics::QUALITY_EVENTS
                            .with_label_values(&["uplink_packet_loss", "alert"])
                            .inc();
                        self.control.events.publish(ServerEvent::QualityAlert {
                            app_id,
                            session_id,
                            user_id,
                            metric: "uplink_packet_loss".into(),
                            value: quality.uplink_loss_percent as f64,
                            threshold: policy.loss_alert_percent as f64,
                            timestamp: chrono::Utc::now(),
                        });
                    }
                    match transition {
                        Some(MosTransition::Degraded) => {
                            aurix_metrics::QUALITY_EVENTS
                                .with_label_values(&["mos", "alert"])
                                .inc();
                            self.control.events.publish(ServerEvent::QualityAlert {
                                app_id,
                                session_id,
                                user_id,
                                metric: "mos".into(),
                                value: quality.mos as f64,
                                threshold: policy.mos_alert_threshold as f64,
                                timestamp: chrono::Utc::now(),
                            });
                        }
                        Some(MosTransition::Recovered) => {
                            aurix_metrics::QUALITY_EVENTS
                                .with_label_values(&["mos", "recovered"])
                                .inc();
                            self.control.events.publish(ServerEvent::QualityRecovered {
                                app_id,
                                session_id,
                                user_id,
                                metric: "mos".into(),
                                value: quality.mos as f64,
                                threshold: policy.mos_alert_threshold as f64,
                                timestamp: chrono::Utc::now(),
                            });
                        }
                        None => {}
                    }
                }
                MediaEvent::ParticipantStreams {
                    session_id,
                    streams,
                } => {
                    self.send_to_session(
                        &session_id,
                        &ControlMessage::ParticipantStreams { streams },
                    );
                }
                MediaEvent::RolesChanged {
                    app_id,
                    channel_id,
                    changes,
                } => {
                    self.apply_role_changes(app_id, channel_id, changes).await;
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
                    let app_id = self.connections.get(&session_id).map(|c| c.app_id);
                    for channel_id in channels {
                        let msg = ControlMessage::MuteStateChanged {
                            channel_id,
                            user_id,
                            muted,
                            server_muted,
                        };
                        match app_id {
                            Some(app_id) => {
                                self.broadcast_visible(app_id, &channel_id, &user_id, &msg, None)
                            }
                            None => self.broadcast_channel(&channel_id, &msg, None),
                        }
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
        if let Some(observers) = &left.roster_observers {
            let msg = ControlMessage::ParticipantLeft {
                channel_id,
                user_id,
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                for observer in observers {
                    self.send_to_member(app_id, &channel_id, observer, &json);
                }
            }
        }
        self.apply_role_changes(app_id, channel_id, left.admitted)
            .await;
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
            hidden: left.hidden,
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
    if state.control.config.media.cascade_relay_only {
        // A relay-only hub hosts no clients; discovery never advertises it, but a stale or
        // hand-written endpoint may still point here.
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
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
    // A draining node (operator maintenance) keeps serving the sessions it hosts — their
    // reconnects are still accepted — but takes no fresh sessions and adopts none from other
    // nodes; clients move on to the next failover endpoint. Checked before the login token is
    // spent so a refused client can still connect elsewhere.
    if state.control.nodes.is_draining(state.control.node_id) {
        let resumes_local = creds
            .resume
            .as_ref()
            .is_some_and(|(sid, _)| state.connections.contains_key(sid));
        if !resumes_local {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
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
    // Claimed before the upgrade so two racing reconnects cannot both take the session. A
    // credential for a session hosted elsewhere is checked against its Redis mirror here; the
    // ownership claim itself happens after the upgrade (`adopt_mirrored_session`).
    let mut takeover = None;
    let resume = match creds.resume {
        Some((sid, tok)) if state.claim_resume(sid, &tok, &validated) => Some(sid),
        Some((sid, tok)) => {
            takeover = state.takeover_candidate(sid, &tok, &validated).await;
            None
        }
        None => None,
    };
    if !fresh_allowed && resume.is_none() && takeover.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if resume.is_none() && state.control.nodes.is_draining(state.control.node_id) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let ws = ws.max_message_size(MAX_TEXT_FRAME);
    let ws = if creds.echo_subprotocol {
        ws.protocols([AURIX_SUBPROTOCOL])
    } else {
        ws
    };
    ws.on_upgrade(move |socket| {
        handle_ws_connection(socket, state, validated, ip, user_agent, resume, takeover)
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
        migrated: false,
    })
}

fn takeover_refused(reason: &str) {
    aurix_metrics::WS_TAKEOVERS_REFUSED
        .with_label_values(&[reason])
        .inc();
}

/// Adopts a session mirrored by another node: claims ownership in Redis (fencing out the
/// previous host and any concurrent takeover), rebuilds the media session with the same id
/// and SSRC (fresh media key/endpoint), re-homes the database row and restores channels and
/// preferences. `None` when the takeover was refused — the caller opens a fresh session.
async fn adopt_mirrored_session(
    state: &WsState,
    token: &ValidatedToken,
    candidate: TakeoverCandidate,
    ip: IpAddr,
    user_agent: Option<&str>,
    tx: &mpsc::Sender<String>,
    resume_hash: [u8; 32],
) -> Option<Attached> {
    let redis = state.control.redis.as_ref()?;
    let cluster = &state.control.config.cluster;
    let node_id = state.control.node_id;
    let TakeoverCandidate { session_id, mirror } = candidate;
    let previous = redis.get_session_node(session_id).await.ok().flatten();
    match redis
        .claim_session(
            session_id,
            previous.unwrap_or(node_id),
            cluster.session_mirror_ttl_secs,
        )
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(refused)) => {
            takeover_refused(match refused {
                TakeoverRefused::NotMirrored => "not_mirrored",
                TakeoverRefused::Denied => "denied",
                TakeoverRefused::Raced => "raced",
            });
            return None;
        }
        Err(e) => {
            warn!("session takeover claim for {session_id} failed: {e}");
            takeover_refused("redis");
            return None;
        }
    }
    // Owned by this node from here on; a refusal below hands ownership back to the previous
    // host (the mirror stays) so a still-alive node can carry on or another node can try.
    for old in state.sessions_of_user(token.app_id, token.user_id) {
        if state.take_detached(old) {
            cleanup_connection(state, old, token.user_id, "replaced").await;
        } else {
            state.request_close(old, "replaced");
        }
    }
    let adopted = state.sfu.read().adopt_session(
        session_id,
        token.user_id,
        token.app_id,
        token.display_name.clone(),
        mirror.ssrc,
        mirror.audio_seq.wrapping_add(MIGRATION_SEQUENCE_GAP),
    );
    let media_session = match adopted {
        Ok(s) => s,
        Err(e) => {
            warn!("session takeover of {session_id} refused by the SFU: {e}");
            let _ = redis.release_session_claim(session_id, previous).await;
            takeover_refused("capacity");
            return None;
        }
    };
    let migrated = match state
        .control
        .sessions
        .migrate_session(session_id, token.app_id, token.user_id, node_id)
        .await
    {
        Ok(Some(m)) => m,
        Ok(None) => {
            // No row (retention) or closed for good elsewhere: a fresh row under the same id
            // keeps the session's history continuous; a conflict means it is not migratable.
            let created = state
                .control
                .sessions
                .create_session(
                    session_id,
                    token.user_id,
                    token.app_id,
                    node_id,
                    ClientInfo {
                        ip_address: &ip.to_string(),
                        user_agent,
                    },
                    0,
                )
                .await;
            if let Err(e) = created {
                warn!("session takeover of {session_id} has no migratable row: {e}");
                let _ = state.sfu.read().destroy_session(&session_id);
                let _ = redis.release_session_claim(session_id, previous).await;
                takeover_refused("db");
                return None;
            }
            aurix_control::session_manager::MigratedSession {
                reaped: true,
                open_channels: Vec::new(),
                quality_stats: None,
            }
        }
        Err(e) => {
            warn!("session takeover of {session_id} could not update the database: {e}");
            let _ = state.sfu.read().destroy_session(&session_id);
            let _ = redis.release_session_claim(session_id, previous).await;
            takeover_refused("db");
            return None;
        }
    };
    if let Some(summary) = migrated
        .quality_stats
        .as_ref()
        .and_then(|v| serde_json::from_value::<QualitySummary>(v.clone()).ok())
    {
        media_session.seed_quality_summary(&summary);
    }
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
    {
        let mut prefs = media_session.prefs.write();
        for (sender, channel) in &mirror.prefs.local_mutes {
            prefs.set_muted(*sender, *channel, true);
        }
        for (sender, gain) in &mirror.prefs.gains {
            prefs.set_gain(*sender, *gain);
        }
    }
    media_session
        .is_muted
        .store(mirror.prefs.muted, Ordering::Relaxed);
    if mirror.prefs.codec.is_g711() && state.control.config.media.pcmu_fallback {
        let _ = media_session.set_codec(mirror.prefs.codec);
    }
    let translation = state
        .control
        .translation
        .listener_preferences(
            mirror.prefs.translation.as_deref(),
            mirror.prefs.spoken_language.as_deref(),
            mirror.prefs.translation_speech,
        )
        .unwrap_or_default();
    if let Some(pipeline) = state.sfu.read().audio_pipeline() {
        pipeline.set_spoken_language(token.user_id, translation.spoken_language.clone());
    }
    state.connections.insert(
        session_id,
        ConnectionInfo {
            tx: tx.clone(),
            user_id: token.user_id,
            app_id: token.app_id,
            display_name: token.display_name.clone(),
            ssrc: mirror.ssrc,
            channels: Vec::new(),
            priority_grants: Vec::new(),
            ip: ip.to_string(),
            user_agent: user_agent.map(str::to_string),
            resume_token_hash: resume_hash,
            detached_since: None,
            generation: 0,
            closing: None,
            transcripts: mirror.prefs.transcripts,
            translation,
        },
    );

    let mut restored = Vec::with_capacity(mirror.channels.len());
    for channel in &mirror.channels {
        let open = migrated.open_channels.contains(&channel.channel_id);
        // A membership closed by anything other than node cleanup (leave, kick) stays closed.
        if !open && !migrated.reaped {
            continue;
        }
        if restore_channel(state, token, session_id, channel, open).await {
            restored.push(channel.channel_id);
        }
    }
    if let Some(focus) = mirror.prefs.focus {
        let _ = media_session.set_focus(Some(focus));
    }
    let _ = media_session.set_transmission(mirror.prefs.transmission);
    if mirror.prefs.downlink == DownlinkMode::Mixed && state.sfu.read().downlink_mix_enabled() {
        let _ = state
            .sfu
            .read()
            .set_downlink_mode(&session_id, DownlinkMode::Mixed);
    }
    if mirror.prefs.noise_suppression {
        let _ = state.sfu.read().set_noise_suppression(&session_id, true);
    }
    state.mirror_session(session_id).await;
    state.control.events.publish(ServerEvent::SessionMigrated {
        app_id: token.app_id,
        session_id,
        user_id: token.user_id,
        from: previous.unwrap_or(node_id),
        to: node_id,
        ssrc: mirror.ssrc,
        channels: restored.clone(),
    });
    aurix_metrics::WS_SESSIONS_MIGRATED.inc();
    aurix_metrics::WS_SESSIONS_RESUMED.inc();
    info!(
        "WS session {} for user {} adopted from {} with {} channel(s)",
        session_id,
        token.user_id,
        previous
            .map(|n| n.to_string())
            .unwrap_or_else(|| "an unknown node".into()),
        restored.len()
    );
    Some(Attached {
        session_id,
        ssrc: mirror.ssrc,
        media_key: media_session.media_key,
        resumed_channels: Some(restored),
        migrated: true,
    })
}

/// Re-joins an adopted session to one of its mirrored channels. `open` says the membership
/// row survived (the previous node is merely unreachable); otherwise node cleanup closed it
/// and it is re-created and announced. Returns whether the session is in the channel.
async fn restore_channel(
    state: &WsState,
    token: &ValidatedToken,
    session_id: SessionId,
    channel: &MirroredChannel,
    open: bool,
) -> bool {
    let channel_id = channel.channel_id;
    let (app_id, user_id) = (token.app_id, token.user_id);
    let close_open_membership = |reason: &'static str| async move {
        if !open {
            return;
        }
        let _ = state
            .control
            .sessions
            .remove_channel_membership(channel_id, session_id)
            .await;
        let count = state
            .control
            .channels
            .update_participant_count(channel_id, -1)
            .await;
        if let Some(redis) = &state.control.redis {
            let _ = redis.remove_user_channel(user_id, channel_id).await;
            let _ = redis.decr_channel_participants(channel_id).await;
        }
        state.control.events.publish(ServerEvent::ParticipantLeft {
            app_id,
            channel_id,
            user_id,
            session_id,
            reason: reason.into(),
            hidden: false,
            timestamp: chrono::Utc::now(),
        });
        if let Ok(0) = count {
            state.control.channel_emptied(app_id, channel_id).await;
        }
    };
    let config = match state
        .control
        .channels
        .resolve_join(app_id, channel_id, None)
        .await
    {
        Ok(resolved) => resolved.config,
        Err(e) => {
            debug!("channel {channel_id} not restored for {session_id}: {e}");
            close_open_membership("channel_gone").await;
            return false;
        }
    };
    let remote_speakers = if config.audience.is_some_and(|a| a.max_speakers > 0)
        && channel.role.can_speak()
        && state.sfu.read().get_channel(&channel_id).is_none()
    {
        state.remote_speaker_count(app_id, channel_id).await
    } else {
        0
    };
    let joined = state.sfu.read().join_channel_with(
        &session_id,
        channel_id,
        config,
        channel.role,
        JoinHints {
            priority: channel.priority,
            remote_speakers,
        },
    );
    let admission = match joined {
        Ok(joined) => joined.admission,
        Err(e) => {
            warn!("channel {channel_id} not restored for {session_id}: {e}");
            close_open_membership("restore_failed").await;
            return false;
        }
    };
    let ssrc = state
        .connections
        .get(&session_id)
        .map(|c| c.ssrc)
        .unwrap_or_default();
    if !open {
        if let Err(e) = state
            .control
            .sessions
            .add_channel_membership(
                channel_id,
                user_id,
                session_id,
                channel.role,
                ssrc,
                channel.priority,
                admission.waiting,
            )
            .await
        {
            warn!("membership restore failed for {session_id} in {channel_id}: {e}");
            let _ = state.sfu.read().leave_channel(&session_id, &channel_id);
            return false;
        }
        if let Ok(1) = state
            .control
            .channels
            .update_participant_count(channel_id, 1)
            .await
        {
            state.control.events.publish(ServerEvent::ChannelActivated {
                app_id,
                channel_id,
                timestamp: chrono::Utc::now(),
            });
        }
        if let Some(redis) = &state.control.redis {
            let _ = redis.incr_channel_participants(channel_id).await;
        }
    }
    state.index_join(channel_id, session_id);
    if channel.priority {
        state.grant_priority(channel_id, session_id);
    }
    if let Some(redis) = &state.control.redis {
        let _ = redis.add_user_channel(user_id, channel_id).await;
    }
    state.sync_remote_members(app_id, channel_id).await;
    if admission.waiting {
        state.send_to_session(
            &session_id,
            &ControlMessage::RoleChanged {
                channel_id,
                user_id,
                role: admission.role,
                reason: RoleChangeReason::SpeakerDemoted,
            },
        );
    }
    if open {
        if let Err(e) = state
            .control
            .sessions
            .set_membership_waiting_to_speak(app_id, channel_id, user_id, admission.waiting)
            .await
        {
            warn!("membership admission update failed for {session_id} in {channel_id}: {e}");
        }
    }
    state
        .apply_role_changes(app_id, channel_id, admission.demoted.into_iter().collect())
        .await;
    if !open {
        state
            .control
            .events
            .publish(ServerEvent::ParticipantJoined {
                app_id,
                channel_id,
                user_id,
                session_id,
                display_name: token.display_name.clone(),
                ssrc,
                role: admission.role,
                is_priority: channel.priority,
                timestamp: chrono::Utc::now(),
            });
    }
    true
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

    // The application's quotas apply before any node resource is taken.
    let limits = state.control.usage.app_limits(token.app_id).await?;

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
            ClientInfo {
                ip_address: &ip.to_string(),
                user_agent,
            },
            limits.max_concurrent_sessions,
        )
        .await
    {
        {
            let sfu = state.sfu.read();
            let _ = sfu.destroy_session(&session_id);
        }
        if matches!(e, AurixError::QuotaExceeded(_)) {
            return Err(e);
        }
        warn!("session persistence failed: {e}");
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
            priority_grants: Vec::new(),
            ip: ip.to_string(),
            user_agent: user_agent.map(str::to_string),
            resume_token_hash: resume_hash,
            detached_since: None,
            generation: 0,
            closing: None,
            transcripts: true,
            translation: ListenerTranslation::default(),
        },
    );
    Ok(Attached {
        session_id,
        ssrc: media_session.ssrc,
        media_key: media_session.media_key,
        resumed_channels: None,
        migrated: false,
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
        codec: session.codec(),
        downlink: session.downlink_mode(),
        noise_suppression: session.noise_suppression(),
    })
}

/// `TranslationChanged` for a resumed/migrated session that had preferences; a fresh session
/// (nothing set) gets none.
fn translation_preferences(state: &WsState, session_id: &SessionId) -> Option<ControlMessage> {
    let conn = state.connections.get(session_id)?;
    if conn.translation == ListenerTranslation::default() {
        return None;
    }
    Some(translation_changed(&conn.translation))
}

fn translation_changed(prefs: &ListenerTranslation) -> ControlMessage {
    ControlMessage::TranslationChanged {
        language: prefs.language.clone(),
        spoken_language: prefs.spoken_language.clone(),
        speech: prefs.speech,
    }
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
    let Some(user_id) = sfu.get_session(session_id).map(|s| s.user_id) else {
        return Vec::new();
    };
    channel
        .roster_for(&user_id)
        .into_iter()
        .filter(|e| e.user_id != user_id)
        .map(brief_from_entry)
        .collect()
}

fn brief_from_entry(e: RosterEntry) -> ParticipantBrief {
    ParticipantBrief {
        user_id: e.user_id,
        display_name: e.display_name,
        ssrc: e.ssrc,
        role: e.role,
        is_muted: e.is_muted,
        is_speaking: e.is_speaking,
        is_priority: e.is_priority,
    }
}

fn parse_role(s: &str) -> ChannelRole {
    match s {
        "listener" => ChannelRole::Listener,
        "moderator" => ChannelRole::Moderator,
        "administrator" => ChannelRole::Administrator,
        _ => ChannelRole::Speaker,
    }
}

/// Radii the joining client needs to interpret presence and text scoping.
/// The `ChannelJoinAck` for `user_id`'s session in `channel_id`: roster as they see it, the
/// channel's policies and their own role.
fn channel_join_ack(
    state: &WsState,
    session_id: &SessionId,
    user_id: &UserId,
    channel_id: &ChannelId,
) -> ControlMessage {
    let participants = channel_snapshot(state, session_id, channel_id);
    let channel = state.sfu.read().get_channel(channel_id);
    let (role, participant_count, hidden_listeners, roster_radius, text_radius, positional) =
        channel
            .as_ref()
            .map(|c| {
                (
                    c.role_of(user_id),
                    (c.participant_count() as usize + c.remote_count()) as u32,
                    c.hides_listeners(),
                    c.roster_radius(),
                    c.text_radius(),
                    c.positional_config(),
                )
            })
            .unwrap_or((ChannelRole::Listener, 0, false, None, None, None));
    let ducking = channel.as_ref().and_then(|c| c.ducking_config());
    let priority = channel
        .as_ref()
        .is_some_and(|c| c.has_priority_flag(user_id));
    let waiting_to_speak = channel
        .as_ref()
        .is_some_and(|c| c.is_waiting_to_speak(user_id));
    ControlMessage::ChannelJoinAck {
        channel_id: *channel_id,
        participants,
        transcription: channel_transcribes(state, channel_id),
        safety_voice: channel_safety_monitored(state, channel_id),
        audio: channel_audio_policy(state, channel_id),
        roster_radius,
        text_radius,
        role,
        participant_count,
        hidden_listeners,
        positional,
        ducking,
        priority,
        waiting_to_speak,
    }
}

async fn handle_ws_connection(
    socket: WebSocket,
    state: WsState,
    token: ValidatedToken,
    ip: IpAddr,
    user_agent: Option<String>,
    resume: Option<SessionId>,
    takeover: Option<TakeoverCandidate>,
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
    if attached.is_none() {
        if let Some(candidate) = takeover {
            attached = adopt_mirrored_session(
                &state,
                &token,
                candidate,
                ip,
                user_agent.as_deref(),
                &tx,
                resume_token.hash,
            )
            .await;
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

    // Locator + mirror under this node's ownership (a plain resume refreshes the token hash).
    state.mirror_session(session_id).await;

    // UDP-blocked fallback: this connection may carry AURX packets as binary frames. The
    // tunnel is owned by the connection and only becomes the session's media path after an
    // authenticated SessionBind arrives through it.
    let (tunnel, mut tunnel_rx): (Option<Arc<MediaTunnel>>, mpsc::Receiver<Vec<u8>>) =
        match state.sfu.read().open_tunnel(&session_id) {
            Ok((t, rx)) => (Some(t), rx),
            Err(_) => (None, mpsc::channel(1).1),
        };

    let mut media_addrs = state.control.config.media.advertised_endpoints();
    if media_addrs.is_empty() {
        media_addrs.push(aurix_common::addr::host_port(
            &state.control.config.server.host,
            state.control.config.media.port,
        ));
    }
    let media_addr = media_addrs[0].clone();
    let init_ack = ControlMessage::SessionInitAck {
        session_id,
        ssrc: attached.ssrc,
        media_addr,
        media_addrs,
        media_key: base64::engine::general_purpose::STANDARD.encode(attached.media_key),
        resume_token: resume_token.token,
        resume_grace_ms: grace.as_millis() as u64,
        resumed,
        media_tunnel: tunnel.is_some(),
        quic: match state.sfu.read().get_session(&session_id) {
            Some(s) if s.transport() != Transport::WebRtc => state.sfu.read().quic_info(),
            _ => None,
        },
        tls_tunnel: match state.sfu.read().get_session(&session_id) {
            Some(s) if s.transport() != Transport::WebRtc => state.sfu.read().tls_tunnel_info(),
            _ => None,
        },
        webtransport: match state.sfu.read().get_session(&session_id) {
            Some(s) if s.transport() != Transport::WebRtc => state.sfu.read().webtransport_info(),
            _ => None,
        },
        downlink_mix: state.sfu.read().downlink_mix_enabled(),
        noise_suppression: state.sfu.read().noise_suppression_enabled(),
        webrtc_participant_streams: state.sfu.read().webrtc_participant_streams(),
        unfocused_channel_gain: Some(state.sfu.read().unfocused_channel_gain()),
        migrated: attached.migrated,
        failover: state.failover_endpoints(),
        translation: state.control.translation.info(),
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
    if let Some(msg) = translation_preferences(&state, &session_id) {
        send_msg(&tx, &msg).await;
    }
    // A resumed client reconciles its channel state from one ChannelJoinAck per channel.
    for channel_id in attached.resumed_channels.iter().flatten() {
        send_msg(
            &tx,
            &channel_join_ack(&state, &session_id, &token.user_id, channel_id),
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
    replay_offline_inbox(&state, &tx, session_id, token.app_id, token.user_id).await;
    aurix_metrics::WS_CONNECTIONS.inc();
    info!(
        "WS session {} {} for user {} ({})",
        session_id,
        if attached.migrated {
            "migrated"
        } else if resumed {
            "resumed"
        } else {
            "opened"
        },
        token.user_id,
        ip
    );

    let mut tunnel_open = tunnel.is_some();
    let send_task = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                // Control messages first: they are rare and must not starve behind media.
                biased;
                msg = rx.recv() => match msg {
                    Some(m) if m == CLOSE_SENTINEL => { let _ = ws_sender.close().await; break; }
                    Some(m) => { if ws_sender.send(Message::Text(m)).await.is_err() { break; } }
                    None => { let _ = ws_sender.close().await; break; }
                },
                _ = ping.tick() => { if ws_sender.send(Message::Ping(Vec::new())).await.is_err() { break; } }
                pkt = tunnel_rx.recv(), if tunnel_open => match pkt {
                    Some(p) => { if ws_sender.send(Message::Binary(p)).await.is_err() { break; } }
                    None => tunnel_open = false,
                },
            }
        }
    });

    let recv_state = state.clone();
    let recv_token = token.clone();
    let recv_tx = tx.clone();
    let recv_ip = ip;
    let recv_tunnel = tunnel.clone();
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
                        let mirror = touches_mirror(&cm);
                        handle_control_message(
                            &recv_state,
                            session_id,
                            &recv_token,
                            recv_ip,
                            cm,
                            &recv_tx,
                        )
                        .await;
                        if mirror {
                            recv_state.mirror_session(session_id).await;
                        }
                    }
                    Err(_) => {
                        send_error(&recv_tx, "VALIDATION_ERROR", "Malformed control message").await
                    }
                },
                Message::Binary(data) => {
                    let Some(tunnel) = recv_tunnel.as_ref() else {
                        send_error(
                            &recv_tx,
                            "VALIDATION_ERROR",
                            "Media tunnel is not available",
                        )
                        .await;
                        continue;
                    };
                    if data.len() > MAX_MEDIA_FRAME {
                        aurix_metrics::TUNNEL_PACKETS
                            .with_label_values(&["uplink", "rejected"])
                            .inc();
                        continue;
                    }
                    let router = recv_state.sfu.read().packet_router();
                    if let Ok(router) = router {
                        if let Err(e) = router.route_tunnel_packet(&data, tunnel).await {
                            debug!("tunnel packet from session {session_id} rejected: {e}");
                        }
                    }
                }
                Message::Close(_) => return Disconnect::ClientClose,
                _ => {}
            }
        }
    });

    // Keep the session→node mapping and mirror alive in Redis while connected.
    let redis_state = state.clone();
    let redis_task = tokio::spawn(async move {
        let cluster = &redis_state.control.config.cluster;
        let period = if cluster.session_mirror {
            (cluster.session_mirror_ttl_secs / 3).clamp(10, 60)
        } else {
            60
        };
        let mut interval = tokio::time::interval(Duration::from_secs(period));
        loop {
            interval.tick().await;
            if redis_state.connections.get(&session_id).is_none() {
                return;
            }
            redis_state.mirror_session(session_id).await;
        }
    });

    let disconnect = tokio::select! {
        _ = send_task => Disconnect::Dropped,
        r = recv_task => r.unwrap_or(Disconnect::Dropped),
    };
    redis_task.abort();
    aurix_metrics::WS_CONNECTIONS.dec();
    if let Some(tunnel) = tunnel.as_ref() {
        // The session may survive (resume grace); its media path does not.
        state.sfu.read().close_tunnel(tunnel);
    }

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

/// Control messages after which the Redis mirror must be refreshed so a takeover restores
/// the same channels and preferences.
fn touches_mirror(msg: &ControlMessage) -> bool {
    matches!(
        msg,
        ControlMessage::ChannelJoin { .. }
            | ControlMessage::ChannelLeave { .. }
            | ControlMessage::SetParticipantMute { .. }
            | ControlMessage::SetParticipantVolume { .. }
            | ControlMessage::SetTransmission { .. }
            | ControlMessage::SetChannelFocus { .. }
            | ControlMessage::SetAudioCodec { .. }
            | ControlMessage::SetDownlinkMode { .. }
            | ControlMessage::SetNoiseSuppression { .. }
            | ControlMessage::MuteStateChanged { .. }
            | ControlMessage::SetTranscripts { .. }
            | ControlMessage::SetTranslation { .. }
    )
}

async fn cleanup_connection(state: &WsState, session_id: SessionId, user_id: UserId, reason: &str) {
    // Fencing: a session another node adopted is theirs to close. Tearing it down here would
    // destroy live memberships and announce a leave for a participant who is still in the
    // channel. Deleting the mirror first (owner-guarded) also ends the takeover window: with
    // no mirror nobody can adopt the session while it is being torn down here. (The database
    // updates below are additionally guarded by `media_node_id`.)
    if let Some(redis) = &state.control.redis {
        if state.control.config.cluster.session_mirror {
            if let Ok(false) = redis.delete_session_mirror(session_id).await {
                let owner = redis.get_session_node(session_id).await.ok().flatten();
                state.evict_migrated(session_id, owner).await;
                return;
            }
        } else {
            let _ = redis.delete_session_mirror(session_id).await;
        }
    }
    let node_id = state.control.node_id;
    let channels: Vec<ChannelId> = state
        .connections
        .get(&session_id)
        .map(|c| c.channels.clone())
        .unwrap_or_default();
    for ch in &channels {
        state.leave_channel_full(session_id, *ch, reason).await;
    }
    // The session's lifetime `QualitySummary` (its `last` is the final merged report); a
    // session that was never rated keeps `quality_stats` NULL.
    let quality = {
        let sfu = state.sfu.read();
        let q = sfu
            .get_session(&session_id)
            .and_then(|s| s.quality_summary())
            .and_then(|summary| serde_json::to_value(summary).ok());
        let _ = sfu.destroy_session(&session_id);
        q
    };
    state.connections.remove(&session_id);
    state.control.chat.forget_session(session_id);
    state.control.speech.cancel_for_session(session_id, None);
    let _ = state
        .control
        .sessions
        .close_session_memberships(session_id, node_id)
        .await;
    if let Err(e) = state
        .control
        .sessions
        .close_session(session_id, node_id, reason, quality)
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
            .is_some_and(|c| c.config().transcription)
}

fn channel_safety_monitored(state: &WsState, channel_id: &ChannelId) -> bool {
    state.control.safety.voice_enabled()
        && state
            .sfu
            .read()
            .get_channel(channel_id)
            .is_some_and(|c| c.config().safety_voice)
}

fn channel_audio_policy(state: &WsState, channel_id: &ChannelId) -> AudioPolicy {
    state
        .sfu
        .read()
        .get_channel(channel_id)
        .map(|c| c.audio_policy())
        .unwrap_or_default()
}

/// Uplink bitrate the client should use for the reported link, within the merged channel
/// policy. `None` when the current command still stands.
fn adapt_bitrate(
    policy: &AudioPolicy,
    packet_loss: f32,
    jitter_ms: f32,
    commanded_kbps: u32,
) -> Option<u32> {
    let target_kbps = policy.bitrate_bps.div_ceil(1000);
    let floor_kbps = policy.min_bitrate_bps.div_ceil(1000).min(target_kbps);
    let wanted = if packet_loss > 20.0 {
        16
    } else if packet_loss > 10.0 || jitter_ms > 50.0 {
        32
    } else if packet_loss <= 2.0 && jitter_ms <= 20.0 {
        target_kbps
    } else {
        return None;
    };
    let wanted = wanted.clamp(floor_kbps, target_kbps);
    let current = if commanded_kbps == 0 {
        target_kbps
    } else {
        commanded_kbps
    };
    (wanted != current).then_some(wanted)
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

/// Join-time E2EE checks: an encrypted channel takes only sessions that announced E2EE
/// support, and a browser (one uplink for all channels) cannot hold encrypted and plaintext
/// channels at the same time.
fn e2ee_join_allowed(
    state: &WsState,
    session_id: &SessionId,
    config: &aurix_common::types::ChannelConfig,
) -> Result<(), AurixError> {
    let sfu = state.sfu.read();
    let Some(session) = sfu.get_session(session_id) else {
        return Err(AurixError::SessionNotFound(session_id.to_string()));
    };
    if config.e2ee && !session.is_e2ee_capable() {
        return Err(AurixError::E2eeRequired(
            "send E2eeHello before joining an encrypted channel".into(),
        ));
    }
    if session.transport() == Transport::WebRtc {
        let mixed = session.get_channels().iter().any(|c| {
            sfu.get_channel(c)
                .is_some_and(|ch| ch.is_e2ee() != config.e2ee)
        });
        if mixed {
            return Err(AurixError::E2eeMixedChannels(
                "a WebRTC session encrypts all channels or none".into(),
            ));
        }
    }
    Ok(())
}

/// Validation shared by the E2EE control messages: rate limit, key encoding, membership of
/// the sender in an encrypted channel of the same tenant.
async fn e2ee_relay_allowed(
    state: &WsState,
    session_id: SessionId,
    token: &ValidatedToken,
    channel_id: &ChannelId,
    public_key: &str,
) -> Result<(), AurixError> {
    if state
        .control
        .limits
        .check(LimitScope::E2ee, &token.user_id.to_string())
        .await
        .is_err()
    {
        return Err(AurixError::RateLimitExceeded(
            "too many E2EE key messages".into(),
        ));
    }
    e2ee::parse_public_key(public_key)?;
    let sfu = state.sfu.read();
    let session = sfu
        .get_session(&session_id)
        .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
    let channel = sfu
        .get_channel(channel_id)
        .filter(|c| c.app_id == token.app_id && c.has_participant(&session.user_id))
        .ok_or_else(|| {
            AurixError::AuthorizationDenied("Sender is not a participant of this channel".into())
        })?;
    if !channel.is_e2ee() {
        return Err(AurixError::Validation(
            "channel is not end-to-end encrypted".into(),
        ));
    }
    Ok(())
}

/// Authorization and flow control for a client chat message, then hand-off to `ChatService`.
/// Channel messages need active membership; directed ones a target user of the same app with
/// no block between the two. An offline target is refused (`USER_OFFLINE`) unless
/// `chat.persist` + `chat.offline_delivery` are on, in which case the message is stored with
/// `offline: true` and replayed when the user connects.
async fn send_chat(
    state: &WsState,
    session_id: SessionId,
    mut msg: OutgoingMessage,
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
            if !chat.offline_delivery() {
                return Err(AurixError::UserOffline);
            }
            let exists = aurix_db::queries::get_user(&state.control.pool, msg.app_id.0, to.0)
                .await
                .map_err(|e| AurixError::Database(e.to_string()))?
                .is_some();
            if !exists {
                return Err(AurixError::NotFound("User not found".into()));
            }
            msg.offline = true;
        }
    }
    chat.accept(msg).await.map(|_| ())
}

/// Locates a stored message and decides how `token`'s session relates to it: a channel
/// message needs active membership here (moderators of that channel may delete others'
/// messages), a direct message must involve the user — otherwise it "does not exist".
async fn chat_actor(
    state: &WsState,
    session_id: SessionId,
    token: &ValidatedToken,
    message_id: uuid::Uuid,
) -> Result<Actor, AurixError> {
    let message = state.control.chat.message(token.app_id, message_id).await?;
    let moderator = match message.channel_id {
        Some(channel_id) => channel_role(state, &session_id, &channel_id)
            .ok_or_else(|| AurixError::AuthorizationDenied("Not a member of this channel".into()))?
            .can_moderate(),
        None => {
            if message.from_user_id != token.user_id && message.to_user_id != Some(token.user_id) {
                return Err(AurixError::NotFound("Message not found".into()));
            }
            false
        }
    };
    Ok(Actor {
        user_id: token.user_id,
        session_id: Some(session_id),
        moderator,
    })
}

/// Resolves a client's `channel_id` / `user_id` conversation selector against its
/// memberships: a channel needs active membership on this node, a direct conversation must
/// name another user of the app.
fn chat_conversation(
    state: &WsState,
    session_id: SessionId,
    user_id: UserId,
    channel_id: Option<ChannelId>,
    peer: Option<UserId>,
) -> Result<Conversation, AurixError> {
    match (channel_id, peer) {
        (Some(channel_id), _) => {
            if channel_role(state, &session_id, &channel_id).is_none() {
                return Err(AurixError::AuthorizationDenied(
                    "Not a member of this channel".into(),
                ));
            }
            Ok(Conversation::Channel(channel_id))
        }
        (None, Some(p)) if p == user_id => Err(AurixError::Validation(
            "Direct conversation needs another user".into(),
        )),
        (None, Some(peer)) => Ok(Conversation::Direct {
            user: user_id,
            peer,
        }),
        (None, None) => Err(AurixError::Validation(
            "Either channel_id or user_id is required".into(),
        )),
    }
}

/// Channel members (local and remote) that `observer` currently sees in the roster — the
/// users whose read receipts they may learn about.
fn visible_participants(state: &WsState, channel_id: &ChannelId, observer: &UserId) -> Vec<UserId> {
    state
        .sfu
        .read()
        .get_channel(channel_id)
        .map(|c| {
            c.roster_for(observer)
                .into_iter()
                .map(|e| e.user_id)
                .collect()
        })
        .unwrap_or_default()
}

/// Directed messages that arrived while the user was offline: replayed oldest-first into
/// this connection (plain `ChatMessageReceived` with `offline: true`), followed by
/// `ChatInboxSynced`. Read markers decide what is "new", so every device replays until the
/// user reads (`ChatMarkRead`); duplicate `id`s are the client's dedupe key.
async fn replay_offline_inbox(
    state: &WsState,
    tx: &mpsc::Sender<String>,
    session_id: SessionId,
    app_id: AppId,
    user_id: UserId,
) {
    let chat = &state.control.chat;
    if !chat.offline_delivery() {
        return;
    }
    let (messages, truncated) = match chat.offline_backlog(app_id, user_id).await {
        Ok(b) => b,
        Err(e) => {
            warn!("offline chat backlog for {user_id}: {e}");
            return;
        }
    };
    let mut delivered = 0u32;
    for message in messages {
        let blocked = state
            .sfu
            .read()
            .get_session(&session_id)
            .is_some_and(|s| s.prefs.read().is_blocked_either_way(&message.from_user_id));
        if blocked {
            continue;
        }
        send_msg(tx, &ControlMessage::ChatMessageReceived { message }).await;
        delivered += 1;
    }
    send_msg(
        tx,
        &ControlMessage::ChatInboxSynced {
            delivered,
            truncated,
        },
    )
    .await;
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
            if state
                .control
                .limits
                .check(LimitScope::Join, &token.user_id.to_string())
                .await
                .is_err()
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
            // Monthly participant-minutes quota: a join is what starts charging.
            let quota = match state.control.usage.app_limits(token.app_id).await {
                Ok(limits) => {
                    state
                        .control
                        .usage
                        .check_minutes_quota(token.app_id, limits.monthly_participant_minutes)
                        .await
                }
                Err(e) => Err(e),
            };
            if let Err(e) = quota {
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
            let priority = state
                .control
                .rbac
                .priority_from_permissions(&channel_id, &perms);
            if let Err(e) = e2ee_join_allowed(state, &session_id, &config) {
                if resolved.created {
                    state.control.release_ad_hoc(token.app_id, channel_id).await;
                }
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            if let Some(claim) = claim {
                if let Err(e) = state.control.action_tokens.consume(&claim).await {
                    if resolved.created {
                        state.control.release_ad_hoc(token.app_id, channel_id).await;
                    }
                    return send_error(tx, e.error_code(), &e.public_message()).await;
                }
            }
            let remote_speakers = if config.audience.is_some_and(|a| a.max_speakers > 0)
                && role.can_speak()
                && state.sfu.read().get_channel(&channel_id).is_none()
            {
                state.remote_speaker_count(token.app_id, channel_id).await
            } else {
                0
            };
            let join_result = {
                let sfu = state.sfu.read();
                sfu.join_channel_with(
                    &session_id,
                    channel_id,
                    config,
                    role,
                    JoinHints {
                        priority,
                        remote_speakers,
                    },
                )
            };
            let joined = match join_result {
                Ok(joined) => joined,
                Err(e) => {
                    if resolved.created {
                        state.control.release_ad_hoc(token.app_id, channel_id).await;
                    }
                    return send_error(tx, e.error_code(), &e.public_message()).await;
                }
            };
            let admission = joined.admission;
            let ssrc = state
                .connections
                .get(&session_id)
                .map(|c| c.ssrc)
                .unwrap_or(0);
            if let Err(e) = state
                .control
                .sessions
                .add_channel_membership(
                    channel_id,
                    token.user_id,
                    session_id,
                    role,
                    ssrc,
                    priority,
                    admission.waiting,
                )
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
            if priority {
                state.grant_priority(channel_id, session_id);
            }
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
            state.sync_remote_members(token.app_id, channel_id).await;
            send_msg(
                tx,
                &channel_join_ack(state, &session_id, &token.user_id, &channel_id),
            )
            .await;
            state
                .apply_role_changes(
                    token.app_id,
                    channel_id,
                    admission.demoted.into_iter().collect(),
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
                    role: admission.role,
                    is_priority: priority,
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

        ControlMessage::SetAudioCodec { codec } => {
            if codec.is_g711() && !state.control.config.media.pcmu_fallback {
                return send_error(
                    tx,
                    "CODEC_NOT_AVAILABLE",
                    "The G.711 fallback codecs are disabled on this node",
                )
                .await;
            }
            let result = {
                let sfu = state.sfu.read();
                match sfu.get_session(&session_id) {
                    Some(s) => s.set_codec(codec),
                    None => Err(AurixError::SessionNotFound(session_id.to_string())),
                }
            };
            match result {
                Ok(()) => send_msg(tx, &ControlMessage::AudioCodecChanged { codec }).await,
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::SetDownlinkMode { mode } => {
            let result = state.sfu.read().set_downlink_mode(&session_id, mode);
            match result {
                Ok(()) => send_msg(tx, &ControlMessage::DownlinkModeChanged { mode }).await,
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::SetNoiseSuppression { enabled } => {
            let result = state.sfu.read().set_noise_suppression(&session_id, enabled);
            match result {
                Ok(()) => send_msg(tx, &ControlMessage::NoiseSuppressionChanged { enabled }).await,
                Err(e) => return send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::SetUserBlock { user_id, blocked } => {
            if user_id == token.user_id {
                return send_error(tx, "VALIDATION_ERROR", "Cannot block yourself").await;
            }
            if state
                .control
                .limits
                .check(LimitScope::Block, &token.user_id.to_string())
                .await
                .is_err()
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
            let has_remote = state
                .sfu
                .read()
                .get_channel(&channel_id)
                .is_some_and(|c| c.remote_count() > 0);
            state.apply_positions(
                token.app_id,
                channel_id,
                accepted.clone(),
                Some(&session_id),
            );
            if has_remote {
                state
                    .control
                    .events
                    .publish(ServerEvent::ParticipantPositions {
                        app_id: token.app_id,
                        channel_id,
                        origin: state.control.node_id,
                        positions: accepted,
                    });
            }
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
            let adaptation = {
                let sfu = state.sfu.read();
                sfu.get_session(&session_id).and_then(|s| {
                    let policy = sfu.session_audio_policy(&s);
                    let commanded = s.commanded_bitrate_kbps.load(Ordering::Relaxed);
                    adapt_bitrate(&policy, packet_loss, jitter_ms, commanded).map(|target| {
                        let at_target = target == policy.bitrate_bps.div_ceil(1000);
                        s.commanded_bitrate_kbps
                            .store(if at_target { 0 } else { target }, Ordering::Relaxed);
                        (target, at_target)
                    })
                })
            };
            if let Some((target, at_target)) = adaptation {
                let reason = if at_target {
                    format!("recovered loss={packet_loss:.1}% jitter={jitter_ms:.1}ms")
                } else {
                    format!("loss={packet_loss:.1}% jitter={jitter_ms:.1}ms")
                };
                send_msg(
                    tx,
                    &ControlMessage::BitrateCommand {
                        target_bitrate_kbps: target,
                        reason,
                        expected_loss_percent: packet_loss.round().clamp(0.0, 100.0) as u8,
                    },
                )
                .await;
            }
            let loss_threshold = state.control.config.quality.loss_alert_percent;
            if packet_loss > loss_threshold {
                aurix_metrics::QUALITY_EVENTS
                    .with_label_values(&["packet_loss", "alert"])
                    .inc();
                state.control.events.publish(ServerEvent::QualityAlert {
                    app_id: token.app_id,
                    session_id,
                    user_id: token.user_id,
                    metric: "packet_loss".into(),
                    value: packet_loss as f64,
                    threshold: loss_threshold as f64,
                    timestamp: chrono::Utc::now(),
                });
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
                offline: false,
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
                offline: false,
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

        ControlMessage::ChatHistory {
            channel_id,
            user_id,
            before,
            after,
            limit,
            client_ref,
        } => {
            let result = async {
                let conversation =
                    chat_conversation(state, session_id, token.user_id, channel_id, user_id)?;
                state
                    .control
                    .chat
                    .history(
                        token.app_id,
                        conversation,
                        before.as_deref(),
                        after.as_deref(),
                        limit,
                        Some(token.user_id),
                    )
                    .await
            }
            .await;
            match result {
                Ok(page) => {
                    send_msg(
                        tx,
                        &ControlMessage::ChatHistoryResult {
                            channel_id,
                            user_id,
                            messages: page.messages,
                            next_before: page.next_before,
                            next_after: page.next_after,
                            client_ref,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    send_error_ref(tx, e.error_code(), &e.public_message(), client_ref).await;
                }
            }
        }

        ControlMessage::ChatSearch {
            channel_id,
            user_id,
            query,
            from_user_id,
            before,
            limit,
            client_ref,
        } => {
            let result = async {
                let chat = &state.control.chat;
                chat.check_search_rate(session_id)?;
                // No selector: every direct conversation of the requesting user.
                let conversation = if channel_id.is_none() && user_id.is_none() {
                    Conversation::User(token.user_id)
                } else {
                    chat_conversation(state, session_id, token.user_id, channel_id, user_id)?
                };
                chat.search(
                    token.app_id,
                    conversation,
                    SearchQuery {
                        query: &query,
                        from_user_id,
                        before: before.as_deref(),
                        limit,
                    },
                    Some(token.user_id),
                )
                .await
            }
            .await;
            match result {
                Ok(page) => {
                    send_msg(
                        tx,
                        &ControlMessage::ChatSearchResult {
                            channel_id,
                            user_id,
                            query,
                            messages: page.messages,
                            next_before: page.next_before,
                            client_ref,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    send_error_ref(tx, e.error_code(), &e.public_message(), client_ref).await;
                }
            }
        }

        ControlMessage::ChatEdit {
            message_id,
            text,
            metadata,
            client_ref,
        } => {
            // The edited copy arrives through the event bus like a fresh message.
            let result = async {
                let chat = &state.control.chat;
                chat_sender_allowed(state, &session_id)?;
                chat.check_flood(session_id)?;
                let actor = chat_actor(state, session_id, token, message_id).await?;
                chat.edit(
                    token.app_id,
                    message_id,
                    actor,
                    text,
                    metadata,
                    client_ref.clone(),
                )
                .await
            }
            .await;
            if let Err(e) = result {
                send_error_ref(tx, e.error_code(), &e.public_message(), client_ref).await;
            }
        }

        ControlMessage::ChatDelete {
            message_id,
            client_ref,
        } => {
            let result = async {
                let chat = &state.control.chat;
                chat.check_flood(session_id)?;
                let actor = chat_actor(state, session_id, token, message_id).await?;
                chat.delete(token.app_id, message_id, actor, client_ref.clone())
                    .await
            }
            .await;
            match result {
                // A fresh tombstone fans out through the event bus (echo carries client_ref);
                // repeating the delete only answers the requester with the existing one.
                Ok(deletion) if !deletion.changed => {
                    let mut message = deletion.message;
                    message.client_ref = client_ref;
                    send_msg(tx, &ControlMessage::ChatMessageUpdated { message }).await;
                }
                Ok(_) => {}
                Err(e) => {
                    send_error_ref(tx, e.error_code(), &e.public_message(), client_ref).await;
                }
            }
        }

        ControlMessage::ChatReact {
            message_id,
            reaction,
            add,
        } => {
            // A no-op (already set / already absent) is silently accepted; changes fan out
            // as `ChatReactionChanged` to everyone who sees the message, including the actor.
            let result = async {
                let chat = &state.control.chat;
                chat.validate_reaction(&reaction)?;
                chat_sender_allowed(state, &session_id)?;
                chat.check_flood(session_id)?;
                let actor = chat_actor(state, session_id, token, message_id).await?;
                chat.react(token.app_id, message_id, actor, &reaction, add)
                    .await
            }
            .await;
            if let Err(e) = result {
                send_error(tx, e.error_code(), &e.public_message()).await;
            }
        }

        ControlMessage::ChatMarkRead {
            channel_id,
            user_id,
            message_id,
        } => {
            let result = async {
                let conversation =
                    chat_conversation(state, session_id, token.user_id, channel_id, user_id)?;
                state
                    .control
                    .chat
                    .mark_read(token.app_id, token.user_id, conversation, message_id)
                    .await
            }
            .await;
            // The marker itself arrives through the event bus (all of the user's devices).
            if let Err(e) = result {
                send_error(tx, e.error_code(), &e.public_message()).await;
            }
        }

        ControlMessage::ChatReadMarkers {
            channel_id,
            user_id,
        } => {
            let result = async {
                let conversation =
                    chat_conversation(state, session_id, token.user_id, channel_id, user_id)?;
                let others = match conversation {
                    Conversation::Channel(c) => visible_participants(state, &c, &token.user_id),
                    Conversation::Direct { peer, .. } => vec![peer],
                    Conversation::User(_) => Vec::new(),
                };
                state
                    .control
                    .chat
                    .read_markers(token.app_id, token.user_id, conversation, &others)
                    .await
            }
            .await;
            match result {
                Ok((markers, unread_count)) => {
                    send_msg(
                        tx,
                        &ControlMessage::ChatReadMarkersResult {
                            channel_id,
                            user_id,
                            markers,
                            unread_count,
                        },
                    )
                    .await;
                }
                Err(e) => send_error(tx, e.error_code(), &e.public_message()).await,
            }
        }

        ControlMessage::SetTranscripts { enabled } => {
            if let Some(mut conn) = state.connections.get_mut(&session_id) {
                conn.transcripts = enabled;
            }
        }

        ControlMessage::SetTranslation {
            language,
            spoken_language,
            speech,
        } => {
            let prefs = match state.control.translation.listener_preferences(
                language.as_deref(),
                spoken_language.as_deref(),
                speech,
            ) {
                Ok(prefs) => prefs,
                Err(e) => {
                    send_error(tx, e.error_code(), &e.public_message()).await;
                    return;
                }
            };
            if let Some(pipeline) = state.sfu.read().audio_pipeline() {
                pipeline.set_spoken_language(token.user_id, prefs.spoken_language.clone());
            }
            let reply = translation_changed(&prefs);
            if let Some(mut conn) = state.connections.get_mut(&session_id) {
                conn.translation = prefs;
            }
            send_msg(tx, &reply).await;
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

        ControlMessage::SetParticipantStreams { pinned } => {
            let result = state
                .sfu
                .read()
                .set_participant_streams(&session_id, pinned);
            if let Err(e) = result {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
        }

        ControlMessage::SetPriority {
            channel_id,
            user_id,
            priority,
        } => {
            let target = user_id.unwrap_or(token.user_id);
            let (member, granted) = state
                .connections
                .get(&session_id)
                .map(|c| {
                    (
                        c.channels.contains(&channel_id),
                        c.priority_grants.contains(&channel_id),
                    )
                })
                .unwrap_or((false, false));
            if !member {
                return send_error(tx, "NOT_IN_CHANNEL", "Not a member of this channel").await;
            }
            let Some(channel) = state.sfu.read().get_channel(&channel_id) else {
                return send_error(tx, "CHANNEL_NOT_FOUND", "Channel not found").await;
            };
            if channel.ducking_config().is_none() {
                return send_error(
                    tx,
                    "VALIDATION_ERROR",
                    "Priority speakers require `ducking` in the channel config",
                )
                .await;
            }
            let moderator = channel.role_of(&token.user_id).can_moderate();
            let allowed = if target == token.user_id {
                moderator || granted || (!priority && channel.has_priority_flag(&target))
            } else {
                moderator
            };
            if !allowed {
                return send_error(
                    tx,
                    "AUTH_DENIED",
                    "Priority speaker changes require a moderator role or a priority grant",
                )
                .await;
            }
            if channel.member_role(&target).is_none() {
                return send_error(tx, "NOT_FOUND", "Participant not in channel").await;
            }
            if channel.has_priority_flag(&target) == priority {
                return;
            }
            if let Err(e) = state
                .control
                .sessions
                .set_membership_priority(token.app_id, channel_id, target, priority)
                .await
            {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            channel.set_priority(&target, priority);
            state.control.events.publish(ServerEvent::PriorityChanged {
                app_id: token.app_id,
                channel_id,
                user_id: target,
                priority,
                changed_by: token.user_id,
                timestamp: chrono::Utc::now(),
            });
        }

        ControlMessage::PriorityChanged { .. } | ControlMessage::RoleChanged { .. } => {}

        ControlMessage::E2eeHello {
            channel_id,
            public_key,
            ..
        } => {
            let Some(channel_id) = channel_id else {
                // Capability announcement: no relay, but the key must at least parse.
                if let Err(e) = e2ee::parse_public_key(&public_key) {
                    return send_error(tx, e.error_code(), &e.public_message()).await;
                }
                if let Some(session) = state.sfu.read().get_session(&session_id) {
                    session.set_e2ee_capable(true);
                }
                return;
            };
            if let Err(e) =
                e2ee_relay_allowed(state, session_id, token, &channel_id, &public_key).await
            {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            if let Some(session) = state.sfu.read().get_session(&session_id) {
                session.set_e2ee_capable(true);
            }
            state.control.events.publish(ServerEvent::E2eeRelay {
                app_id: token.app_id,
                channel_id,
                from_user: token.user_id,
                from_session: session_id,
                to: None,
                message: ControlMessage::E2eeHello {
                    channel_id: Some(channel_id),
                    user_id: Some(token.user_id),
                    public_key,
                },
            });
        }

        ControlMessage::E2eeSenderKey {
            channel_id,
            to,
            public_key,
            generation,
            key,
            ..
        } => {
            if let Err(e) =
                e2ee_relay_allowed(state, session_id, token, &channel_id, &public_key).await
            {
                return send_error(tx, e.error_code(), &e.public_message()).await;
            }
            match e2ee::decode_bytes(&key) {
                Ok(k) if k.len() == e2ee::WRAPPED_KEY_LEN => {}
                _ => {
                    return send_error(tx, "VALIDATION_ERROR", "Malformed wrapped sender key").await
                }
            }
            if to == token.user_id {
                return send_error(tx, "VALIDATION_ERROR", "Cannot key yourself").await;
            }
            state.control.events.publish(ServerEvent::E2eeRelay {
                app_id: token.app_id,
                channel_id,
                from_user: token.user_id,
                from_session: session_id,
                to: Some(to),
                message: ControlMessage::E2eeSenderKey {
                    channel_id,
                    from: Some(token.user_id),
                    to,
                    public_key,
                    generation,
                    key,
                },
            });
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
        | ControlMessage::ChannelAudioPolicy { .. }
        | ControlMessage::ParticipantJoined { .. }
        | ControlMessage::ParticipantLeft { .. }
        | ControlMessage::SpeakingStateChanged { .. }
        | ControlMessage::UserBlockChanged { .. }
        | ControlMessage::ReceiverPreferences { .. }
        | ControlMessage::TransmissionChanged { .. }
        | ControlMessage::ChannelFocusChanged { .. }
        | ControlMessage::AudioCodecChanged { .. }
        | ControlMessage::DownlinkModeChanged { .. }
        | ControlMessage::NoiseSuppressionChanged { .. }
        | ControlMessage::TranslationChanged { .. }
        | ControlMessage::BitrateCommand { .. }
        | ControlMessage::NetworkQuality { .. }
        | ControlMessage::RecordingNotification { .. }
        | ControlMessage::Error { .. }
        | ControlMessage::Kick { .. }
        | ControlMessage::ModerateParticipantAck { .. }
        | ControlMessage::ChatMessageReceived { .. }
        | ControlMessage::ChatMessageUpdated { .. }
        | ControlMessage::ChatReactionChanged { .. }
        | ControlMessage::ChatHistoryResult { .. }
        | ControlMessage::ChatSearchResult { .. }
        | ControlMessage::ChatReadMarker { .. }
        | ControlMessage::ChatReadMarkersResult { .. }
        | ControlMessage::ChatInboxSynced { .. }
        | ControlMessage::ParticipantTyping { .. }
        | ControlMessage::ChannelEnergy { .. }
        | ControlMessage::Transcript { .. }
        | ControlMessage::TtsStatus { .. }
        | ControlMessage::WebRtcAnswer { .. }
        | ControlMessage::ParticipantStreams { .. }
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

    #[test]
    fn adaptive_bitrate_stays_inside_the_channel_policy() {
        let policy = AudioPolicy {
            bitrate_bps: 48_000,
            min_bitrate_bps: 24_000,
            ..AudioPolicy::default()
        };
        // Nothing commanded, link is fine: already at target, no command.
        assert_eq!(adapt_bitrate(&policy, 0.0, 5.0, 0), None);
        // Moderate loss steps down to 32 kbit/s (inside the floor).
        assert_eq!(adapt_bitrate(&policy, 12.0, 5.0, 0), Some(32));
        assert_eq!(
            adapt_bitrate(&policy, 12.0, 5.0, 32),
            None,
            "same command twice"
        );
        // Severe loss wants 16 kbit/s but the channel floor is 24.
        assert_eq!(adapt_bitrate(&policy, 30.0, 5.0, 32), Some(24));
        // In-between conditions leave the current command alone.
        assert_eq!(adapt_bitrate(&policy, 5.0, 30.0, 24), None);
        // Recovery goes back up to the policy target, never above it.
        assert_eq!(adapt_bitrate(&policy, 1.0, 10.0, 24), Some(48));

        // A low-bitrate channel: "step down to 32" must not raise the bitrate.
        let narrow = AudioPolicy {
            bitrate_bps: 16_000,
            min_bitrate_bps: 8_000,
            ..AudioPolicy::default()
        };
        assert_eq!(adapt_bitrate(&narrow, 12.0, 5.0, 0), None);
        assert_eq!(
            adapt_bitrate(&narrow, 30.0, 5.0, 0),
            None,
            "16 is already the target"
        );
        assert_eq!(adapt_bitrate(&narrow, 30.0, 5.0, 12), Some(16));
    }

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
            priority_grants: vec![],
            ip: "127.0.0.1".into(),
            user_agent: None,
            resume_token_hash: token.hash,
            detached_since: Some(Instant::now()),
            generation: 1,
            closing: None,
            transcripts: true,
            translation: ListenerTranslation::default(),
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
