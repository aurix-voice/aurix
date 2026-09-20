//! AURX packet router (UDP and the WebSocket tunnel fallback).
//!
//! Security model:
//! * A UDP source address is only associated with a session after an authenticated
//!   `SessionBind` (HMAC with the per-session media key handed out over the WebSocket).
//! * Every subsequent packet must come from the bound address; when
//!   `require_packet_auth` is on it must also carry a valid HMAC tag and a fresh
//!   sequence number (64-packet anti-replay window shared by audio and control packets).
//! * Packets arriving through a WebSocket tunnel are attributed to the session that
//!   connection authenticated as (never to the session named in the packet), then pass the
//!   same tag/replay checks as UDP packets; the bind/ack handshake is identical.
//! * The SSRC in the header is never used to look up a sender.

use crate::transport::MediaSocket;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::*;
use aurix_common::sink::AudioSink;
use aurix_common::types::*;
use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::audio_pipeline::AudioAnalysisPipeline;
use crate::cascade::CascadeRelay;
use crate::channel::{MediaChannel, Mix};
use crate::mix::MixHub;
use crate::session::{MediaEndpoint, MediaSession, Transport};
use crate::transcode::{PcmuDownlink, PcmuUplink};
use crate::tunnel::MediaTunnel;
use crate::webrtc::{ForwardMedia, WebRtcManager};

/// Sweep idle PCMU downlink decoders when the table grows past this.
const PCMU_DOWNLINK_PRUNE_AT: usize = 256;
/// A PCMU downlink decoder unused this long belongs to a stream that ended.
const PCMU_DOWNLINK_IDLE: Duration = Duration::from_secs(30);

/// One synthesized Opus frame handed to the router for injection.
pub struct InjectedFrame {
    pub ssrc: u32,
    pub sequence: u32,
    pub timestamp: u32,
    pub payload: Bytes,
}

impl InjectedFrame {
    fn into_packet(self, channel_id: &ChannelId) -> AurixPacket {
        AurixPacket::audio(
            self.sequence,
            self.timestamp,
            self.ssrc,
            channel_id_hash(channel_id),
            self.payload,
        )
    }
}

/// Notifications from the media plane to the control plane (WebSocket layer).
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// A media path was authenticated for the session.
    SessionBound {
        session_id: SessionId,
        transport: MediaTransportKind,
    },
    SpeakingChanged {
        session_id: SessionId,
        user_id: UserId,
        channels: Vec<ChannelId>,
        speaking: bool,
    },
    MuteChanged {
        session_id: SessionId,
        user_id: UserId,
        channels: Vec<ChannelId>,
        muted: bool,
    },
    /// Periodic level report for one channel: `(user, -dBov level)` of every local member whose
    /// level changed since the last report.
    ChannelEnergy {
        app_id: AppId,
        channel_id: ChannelId,
        levels: Vec<(UserId, u8)>,
    },
    /// Periodic per-session link report (see `SfuOptions::quality_interval_ms`);
    /// `transition` is set when the debounced MOS alert state flipped on this evaluation.
    NetworkQuality {
        session_id: SessionId,
        app_id: AppId,
        user_id: UserId,
        quality: NetworkQuality,
        transition: Option<aurix_common::types::MosTransition>,
    },
    /// A WebRTC session's per-participant downlink tracks changed hands (full snapshot).
    ParticipantStreams {
        session_id: SessionId,
        streams: Vec<aurix_common::protocol::ParticipantStream>,
    },
}

/// Where an uplink packet came from.
#[derive(Clone, Copy)]
enum PacketSource<'a> {
    Udp(SocketAddr),
    Tunnel(&'a Arc<MediaTunnel>),
}

impl std::fmt::Display for PacketSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketSource::Udp(addr) => write!(f, "{addr}"),
            PacketSource::Tunnel(t) => write!(f, "tunnel#{}", t.id()),
        }
    }
}

pub struct RouterShared {
    pub channels: Arc<DashMap<ChannelId, Arc<MediaChannel>>>,
    pub channels_by_hash: Arc<DashMap<u32, ChannelId>>,
    pub sessions_by_id: Arc<DashMap<SessionId, Arc<MediaSession>>>,
    pub sessions_by_addr: Arc<DashMap<SocketAddr, Arc<MediaSession>>>,
}

pub struct PacketRouter {
    shared: RouterShared,
    socket: Arc<MediaSocket>,
    cascade: Option<Arc<CascadeRelay>>,
    audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
    webrtc: Option<Arc<WebRtcManager>>,
    audio_sink: Option<Arc<dyn AudioSink>>,
    require_packet_auth: bool,
    speaking_energy_threshold: f32,
    events: broadcast::Sender<MediaEvent>,
    /// Opus → μ-law decoders for PCMU receivers, per `(sender ssrc, channel hash)` stream.
    pcmu_downlinks: DashMap<(u32, u32), Mutex<PcmuDownlink>>,
    /// Server-side mixers for native receivers in `DownlinkMode::Mixed`
    /// (`None`: `media.downlink_mix = false`, everyone gets per-speaker streams).
    mix: Option<Arc<MixHub>>,
}

impl PacketRouter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: RouterShared,
        socket: Arc<MediaSocket>,
        cascade: Option<Arc<CascadeRelay>>,
        audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
        webrtc: Option<Arc<WebRtcManager>>,
        audio_sink: Option<Arc<dyn AudioSink>>,
        require_packet_auth: bool,
        speaking_energy_threshold: f32,
        events: broadcast::Sender<MediaEvent>,
        mix: Option<Arc<MixHub>>,
    ) -> Self {
        Self {
            shared,
            socket,
            cascade,
            audio_pipeline,
            webrtc,
            audio_sink,
            require_packet_auth,
            speaking_energy_threshold,
            events,
            pcmu_downlinks: DashMap::new(),
            mix,
        }
    }

    /// Server-side downlink mixers of this node, if enabled.
    pub fn mix_hub(&self) -> Option<&Arc<MixHub>> {
        self.mix.as_ref()
    }

    pub async fn route_packet(&self, data: &[u8], src_addr: SocketAddr) -> Result<()> {
        self.route_from(data, PacketSource::Udp(src_addr)).await
    }

    /// Routes one AURX packet that arrived as a binary frame on the WebSocket connection
    /// owning `tunnel`.
    pub async fn route_tunnel_packet(&self, data: &[u8], tunnel: &Arc<MediaTunnel>) -> Result<()> {
        let res = self.route_from(data, PacketSource::Tunnel(tunnel)).await;
        aurix_metrics::TUNNEL_PACKETS
            .with_label_values(&["uplink", if res.is_ok() { "received" } else { "rejected" }])
            .inc();
        res
    }

    async fn route_from(&self, data: &[u8], source: PacketSource<'_>) -> Result<()> {
        let mut packet = AurixPacket::decode(data)?;
        if packet.header.packet_type == PacketType::SessionBind {
            return self.handle_session_bind(&packet, source).await;
        }

        let session = self.authenticate(&mut packet, source)?;
        let is_audio = matches!(
            packet.header.packet_type,
            PacketType::Audio | PacketType::AudioFec
        );
        session.record_uplink(
            packet.header.sequence as u64,
            is_audio.then_some(packet.header.timestamp),
            data.len(),
        );
        match packet.header.packet_type {
            PacketType::Audio | PacketType::AudioFec => {
                let level = packet.take_audio_level();
                // The uplink sequence is shared with heartbeats and reports (one anti-replay
                // window per session); receivers get a per-sender audio-only numbering that
                // keeps short runs of lost frames as gaps.
                packet.header.sequence =
                    session.next_audio_sequence(packet.header.sequence, packet.header.timestamp);
                self.route_audio_packet(&packet, &session, level).await
            }
            PacketType::Heartbeat => self.handle_heartbeat(&packet, &session, source).await,
            PacketType::QualityReport => {
                self.handle_quality_report(&packet, &session);
                Ok(())
            }
            PacketType::MuteState => {
                self.handle_mute_state(&packet, &session);
                Ok(())
            }
            PacketType::SpeakingState => Ok(()),
            other => {
                debug!(
                    "Unhandled packet type {:?} from {}",
                    other, session.session_id
                );
                Ok(())
            }
        }
    }

    /// Resolve the sender by *bound address* (UDP) or by the *authenticated connection* (tunnel),
    /// verify the tag, decrypt the payload in place and check the anti-replay window.
    fn authenticate(
        &self,
        packet: &mut AurixPacket,
        source: PacketSource<'_>,
    ) -> Result<Arc<MediaSession>> {
        let session = match source {
            PacketSource::Udp(src_addr) => self
                .shared
                .sessions_by_addr
                .get(&src_addr)
                .map(|s| s.value().clone())
                .ok_or_else(|| AurixError::AuthenticationFailed("Unbound media source".into()))?,
            PacketSource::Tunnel(tunnel) => {
                let session = self
                    .shared
                    .sessions_by_id
                    .get(&tunnel.session_id())
                    .map(|s| s.value().clone())
                    .ok_or_else(|| AurixError::SessionNotFound("Tunnel session is gone".into()))?;
                // Same rule as UDP ("from the bound address"): media is accepted only through
                // the tunnel the session bound, never while it is on UDP or on an older tunnel.
                if !session.tunnel().is_some_and(|t| *t == **tunnel) {
                    return Err(AurixError::AuthenticationFailed(
                        "Unbound media tunnel".into(),
                    ));
                }
                session
            }
        };
        if !session.is_active() {
            return Err(AurixError::SessionNotFound("Session inactive".into()));
        }
        if packet.header.ssrc != session.ssrc {
            return Err(AurixError::AuthenticationFailed(
                "SSRC does not match session".into(),
            ));
        }
        if packet.is_authenticated() {
            if self.require_packet_auth && !packet.header.has_flag(PacketFlags::Encrypted) {
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(AurixError::AuthenticationFailed(
                    "Encrypted payload required".into(),
                ));
            }
            if !packet.open(&session.keys) {
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(AurixError::AuthenticationFailed(
                    "Invalid packet authentication tag".into(),
                ));
            }
            if !session.accept_sequence(packet.header.sequence) {
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(AurixError::AuthenticationFailed("Replayed packet".into()));
            }
        } else if self.require_packet_auth || packet.header.has_flag(PacketFlags::Encrypted) {
            aurix_metrics::PACKETS_DROPPED.inc();
            return Err(AurixError::AuthenticationFailed(
                "Packet authentication required".into(),
            ));
        }
        Ok(session)
    }

    async fn handle_session_bind(
        &self,
        packet: &AurixPacket,
        source: PacketSource<'_>,
    ) -> Result<()> {
        if packet.header.has_flag(PacketFlags::Encrypted) {
            return Err(AurixError::AuthenticationFailed(
                "SessionBind must be signed, not encrypted".into(),
            ));
        }
        let (session_id, unix_ms, _nonce) = packet.parse_session_bind()?;
        if let PacketSource::Tunnel(tunnel) = source {
            // The connection already proved who it is; a bind for anyone else is an attack.
            if tunnel.session_id() != session_id {
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(AurixError::AuthenticationFailed(
                    "SessionBind for a session the connection does not own".into(),
                ));
            }
        }
        let session = self
            .shared
            .sessions_by_id
            .get(&session_id)
            .map(|s| s.value().clone())
            .ok_or_else(|| AurixError::SessionNotFound("Unknown session in SessionBind".into()))?;
        if !session.is_active() {
            return Err(AurixError::SessionNotFound("Session inactive".into()));
        }
        if !packet.is_authenticated() || !packet.verify_auth(&session.keys) {
            aurix_metrics::PACKETS_DROPPED.inc();
            return Err(AurixError::AuthenticationFailed(
                "SessionBind authentication failed".into(),
            ));
        }
        if packet.header.ssrc != session.ssrc {
            return Err(AurixError::AuthenticationFailed(
                "SessionBind SSRC mismatch".into(),
            ));
        }
        let now = chrono::Utc::now().timestamp_millis();
        if (now - unix_ms).abs() > SESSION_BIND_MAX_SKEW_MS {
            return Err(AurixError::AuthenticationFailed(
                "SessionBind timestamp outside allowed skew".into(),
            ));
        }
        // Binds must be strictly newer than the last accepted one (blocks replayed binds).
        let prev = session
            .last_bind_ms
            .load(std::sync::atomic::Ordering::Acquire);
        if unix_ms <= prev {
            return Err(AurixError::AuthenticationFailed(
                "Replayed SessionBind".into(),
            ));
        }
        session
            .last_bind_ms
            .store(unix_ms, std::sync::atomic::Ordering::Release);

        session.set_transport(Transport::Aurx);
        let transport = match source {
            PacketSource::Udp(src_addr) => {
                if let Some(old) = session.get_remote_addr() {
                    if old != src_addr {
                        self.shared.sessions_by_addr.remove(&old);
                    }
                }
                session.set_remote_addr(src_addr);
                self.shared
                    .sessions_by_addr
                    .insert(src_addr, session.clone());
                MediaTransportKind::Udp
            }
            PacketSource::Tunnel(tunnel) => {
                if let Some(old) = session.set_tunnel(tunnel.clone()) {
                    self.shared.sessions_by_addr.remove(&old);
                }
                MediaTransportKind::Tunnel
            }
        };
        session.update_heartbeat();

        let ack =
            AurixPacket::session_bind_ack(session.ssrc, session.next_downlink_sequence(), now)
                .seal(&session.keys);
        self.reply(source, &ack).await;
        let _ = self.events.send(MediaEvent::SessionBound {
            session_id,
            transport,
        });
        debug!("Session {} bound to {}", session_id, source);
        Ok(())
    }

    /// Sends a server-originated packet back to where an uplink packet came from.
    async fn reply(&self, source: PacketSource<'_>, bytes: &[u8]) {
        match source {
            PacketSource::Udp(addr) => {
                let _ = self.socket.send_to(bytes, addr).await;
            }
            PacketSource::Tunnel(tunnel) => {
                tunnel.send(bytes.to_vec());
            }
        }
    }

    async fn route_audio_packet(
        &self,
        packet: &AurixPacket,
        sender: &Arc<MediaSession>,
        level: Option<u8>,
    ) -> Result<()> {
        sender.record_packet_received(packet.payload.len() as u64);
        sender.update_heartbeat();
        aurix_metrics::PACKETS_RECEIVED.inc();
        aurix_metrics::BYTES_RECEIVED.inc_by(packet.payload.len() as u64);

        if !sender.is_transmitting_allowed() {
            return Ok(());
        }

        let channel_id = match self
            .shared
            .channels_by_hash
            .get(&packet.header.channel_id_hash)
        {
            Some(c) => *c.value(),
            None => return Err(AurixError::ChannelNotFound("Unknown channel hash".into())),
        };
        let channel = match self.shared.channels.get(&channel_id) {
            Some(c) => c.value().clone(),
            None => return Err(AurixError::ChannelNotFound(channel_id.to_string())),
        };
        if channel.app_id != sender.app_id || !channel.can_transmit(&sender.user_id) {
            return Err(AurixError::AuthorizationDenied(
                "Sender may not transmit in this channel".into(),
            ));
        }
        // Frames for a channel outside the transmission mode are dropped silently: the client
        // may legitimately still be sending while the mode change is in flight.
        if !sender.transmits_to(&channel_id) {
            return Ok(());
        }
        if channel.is_e2ee() && !packet.header.has_flag(PacketFlags::E2ee) {
            aurix_metrics::PACKETS_DROPPED.inc();
            return Err(AurixError::Validation(
                "channel requires end-to-end encrypted frames".into(),
            ));
        }

        // A PCMU uplink enters the channel as Opus so recording, transcription, cascade and
        // Opus receivers never see μ-law; PCMU receivers get it re-encoded in `deliver`.
        let transcoded;
        let packet = if packet.header.has_flag(PacketFlags::Pcmu) {
            if packet.header.has_flag(PacketFlags::E2ee) {
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(AurixError::Validation(
                    "PCMU frames cannot be end-to-end encrypted".into(),
                ));
            }
            if sender.codec() != AudioCodec::Pcmu {
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(AurixError::AuthorizationDenied(
                    "PCMU frame from a session that did not negotiate pcmu".into(),
                ));
            }
            transcoded = self.transcode_pcmu_uplink(sender, packet)?;
            &transcoded
        } else {
            packet
        };

        if sender.record_audio_level(level, self.speaking_energy_threshold) {
            let _ = self.events.send(MediaEvent::SpeakingChanged {
                session_id: sender.session_id,
                user_id: sender.user_id,
                channels: vec![channel_id],
                speaking: true,
            });
        }

        self.tap_audio(&channel, sender, packet);
        self.fan_out(&channel, sender, packet).await;
        if let Some(cascade) = self.cascade.as_ref().filter(|_| channel.relays_to_peers()) {
            cascade
                .forward_to_peers(&channel_id, &sender.user_id, packet, level)
                .await;
        }
        Ok(())
    }

    /// Route depayloaded Opus audio received from a WebRTC session; `rtp_seq` is the RTP
    /// sequence (lets short uplink losses stay gaps for native receivers, see
    /// [`MediaSession::next_audio_sequence`]), `level` the RTP audio-level extension
    /// (`-dBov`) when the browser sent one.
    pub async fn route_webrtc_audio(
        &self,
        sender: &Arc<MediaSession>,
        rtp_seq: Option<u32>,
        rtp_time: u32,
        payload: Vec<u8>,
        level: Option<u8>,
    ) -> Result<()> {
        sender.record_packet_received(payload.len() as u64);
        sender.update_heartbeat();
        aurix_metrics::PACKETS_RECEIVED.inc();
        aurix_metrics::BYTES_RECEIVED.inc_by(payload.len() as u64);
        if !sender.is_transmitting_allowed() {
            return Ok(());
        }
        let channels: Vec<ChannelId> = sender
            .get_channels()
            .into_iter()
            .filter(|c| sender.transmits_to(c))
            .collect();
        if channels.is_empty() {
            return Ok(());
        }
        if sender.record_audio_level(level, self.speaking_energy_threshold) {
            let _ = self.events.send(MediaEvent::SpeakingChanged {
                session_id: sender.session_id,
                user_id: sender.user_id,
                channels: channels.clone(),
                speaking: true,
            });
        }
        let seq = match rtp_seq {
            Some(rtp_seq) => sender.next_audio_sequence(rtp_seq, rtp_time),
            None => sender.next_sequence(),
        };
        // A browser sends one frame for all its channels, so a receiver sharing several of them
        // with the sender must get it exactly once — through the channel where it hears the
        // sender loudest (focus / positional attenuation) — or the mixers would double the audio.
        let mut per_channel: Vec<(Arc<MediaChannel>, AurixPacket)> = Vec::new();
        let mut best: HashMap<SessionId, (usize, Arc<MediaSession>, Mix)> = HashMap::new();
        // A browser has one uplink for all its channels and encrypts it end-to-end as soon as
        // any joined channel is encrypted (Insertable Streams); the node cannot tell from the
        // bytes, so the frame is flagged from the channel policies. A browser that never
        // announced E2EE support is not heard in an encrypted channel.
        let e2ee = sender.is_e2ee_capable()
            && sender.get_channels().iter().any(|c| {
                self.shared
                    .channels
                    .get(c)
                    .is_some_and(|ch| ch.value().is_e2ee())
            });
        for channel_id in channels {
            let channel = match self.shared.channels.get(&channel_id) {
                Some(c) => c.value().clone(),
                None => continue,
            };
            if !channel.can_transmit(&sender.user_id) {
                continue;
            }
            if channel.is_e2ee() && !e2ee {
                aurix_metrics::PACKETS_DROPPED.inc();
                continue;
            }
            let mut packet = AurixPacket::audio(
                seq,
                rtp_time,
                sender.ssrc,
                channel_id_hash(&channel_id),
                Bytes::from(payload.clone()),
            );
            if e2ee {
                packet.header.flags |= PacketFlags::E2ee as u16;
            }
            self.tap_audio(&channel, sender, &packet);
            let idx = per_channel.len();
            for (receiver, mix) in channel.get_receivers_for_audio(sender.ssrc) {
                match best.get_mut(&receiver.session_id) {
                    Some(slot) if slot.2.volume >= mix.volume => {}
                    Some(slot) => *slot = (idx, receiver, mix),
                    None => {
                        best.insert(receiver.session_id, (idx, receiver, mix));
                    }
                }
            }
            if let Some(cascade) = self.cascade.as_ref().filter(|_| channel.relays_to_peers()) {
                cascade
                    .forward_to_peers(&channel_id, &sender.user_id, &packet, level)
                    .await;
            }
            per_channel.push((channel, packet));
        }
        let mut receivers: Vec<Vec<(Arc<MediaSession>, Mix)>> = vec![Vec::new(); per_channel.len()];
        for (idx, receiver, mix) in best.into_values() {
            receivers[idx].push((receiver, mix));
        }
        for ((channel, packet), receivers) in per_channel.iter().zip(receivers) {
            if !receivers.is_empty() {
                self.deliver(channel, receivers, packet, Some(sender.user_id))
                    .await;
            }
        }
        Ok(())
    }

    /// Inject a packet relayed from another node (already authenticated by the cascade layer);
    /// `sender` is the remote participant it originates from. The origin node re-attaches the
    /// sender-reported level for ambient ranking; it is stripped again before delivery.
    pub async fn route_relayed_audio(
        &self,
        sender: &UserId,
        mut packet: AurixPacket,
    ) -> Result<()> {
        let channel_id = match self
            .shared
            .channels_by_hash
            .get(&packet.header.channel_id_hash)
        {
            Some(c) => *c.value(),
            None => return Ok(()),
        };
        let channel = match self.shared.channels.get(&channel_id) {
            Some(c) => c.value().clone(),
            None => return Ok(()),
        };
        let level = packet.take_audio_level();
        if !packet.header.has_flag(PacketFlags::E2ee) {
            self.tap_sink(&channel, *sender, packet.header.ssrc, &packet);
        }
        let receivers = channel.get_receivers_for_relayed_audio(sender, level);
        self.deliver(&channel, receivers, &packet, Some(*sender))
            .await;
        Ok(())
    }

    /// Inject a synthesized Opus frame spoken *by* `sender` into `channel_id`. It is routed like
    /// the participant's own uplink — channel type rules, receiver preferences (local mute,
    /// gain, blocks), cascade — but carries `ssrc` (a synthesized-stream SSRC, see
    /// [`crate::tts`]) so receivers keep it apart from the microphone stream. Synthesized audio
    /// is neither recorded nor transcribed. `to_channel` sends it to the other participants,
    /// `to_self` echoes it back to the sender.
    pub async fn inject_participant_audio(
        &self,
        sender: &Arc<MediaSession>,
        channel_id: &ChannelId,
        frame: InjectedFrame,
        to_channel: bool,
        to_self: bool,
    ) -> Result<()> {
        if !sender.is_active() {
            return Err(AurixError::SessionNotFound(sender.session_id.to_string()));
        }
        let channel = self.channel_for_sender(channel_id, sender)?;
        Self::reject_plaintext_injection(&channel)?;
        let packet = frame.into_packet(channel_id);
        if to_channel {
            if sender
                .is_server_muted
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(AurixError::AuthorizationDenied(
                    "Participant is muted by the server".into(),
                ));
            }
            if !channel.can_transmit(&sender.user_id) {
                return Err(AurixError::AuthorizationDenied(
                    "Sender may not transmit in this channel".into(),
                ));
            }
            if sender.mark_audio_activity() {
                let _ = self.events.send(MediaEvent::SpeakingChanged {
                    session_id: sender.session_id,
                    user_id: sender.user_id,
                    channels: vec![*channel_id],
                    speaking: true,
                });
            }
            if let Some(cascade) = self.cascade.as_ref().filter(|_| channel.relays_to_peers()) {
                cascade
                    .forward_to_peers(channel_id, &sender.user_id, &packet, None)
                    .await;
            }
            self.fan_out(&channel, sender, &packet).await;
        }
        if to_self {
            // Meant for this receiver only: must never enter a mixer shared with others.
            self.deliver_scoped(
                &channel,
                vec![(sender.clone(), Mix::UNITY)],
                &packet,
                false,
                None,
            )
            .await;
        }
        Ok(())
    }

    /// Inject a synthesized Opus frame from the server itself (announcement) into `channel_id`:
    /// every local participant hears it at their channel-focus gain; nothing else filters it and
    /// it is not relayed to other nodes (each node announces to its own participants).
    pub async fn inject_system_audio(
        &self,
        app_id: &AppId,
        channel_id: &ChannelId,
        frame: InjectedFrame,
    ) -> Result<()> {
        let channel = self
            .shared
            .channels
            .get(channel_id)
            .map(|c| c.value().clone())
            .ok_or_else(|| AurixError::ChannelNotFound(channel_id.to_string()))?;
        if channel.app_id != *app_id {
            return Err(AurixError::AuthorizationDenied(
                "Channel belongs to a different application".into(),
            ));
        }
        Self::reject_plaintext_injection(&channel)?;
        let packet = frame.into_packet(channel_id);
        let receivers = channel.get_receivers_for_announcement();
        self.deliver(&channel, receivers, &packet, None).await;
        Ok(())
    }

    /// Inject a synthesized Opus frame meant for `listener` alone (a spoken translation):
    /// sealed for that receiver only, never mixed with or relayed to anyone else, not recorded
    /// or transcribed. `listener` must be a participant of `channel_id`.
    pub async fn inject_listener_audio(
        &self,
        listener: &Arc<MediaSession>,
        channel_id: &ChannelId,
        frame: InjectedFrame,
    ) -> Result<()> {
        if !listener.is_active() {
            return Err(AurixError::SessionNotFound(listener.session_id.to_string()));
        }
        let channel = self.channel_for_sender(channel_id, listener)?;
        Self::reject_plaintext_injection(&channel)?;
        let packet = frame.into_packet(channel_id);
        self.deliver_scoped(
            &channel,
            vec![(listener.clone(), Mix::UNITY)],
            &packet,
            false,
            None,
        )
        .await;
        Ok(())
    }

    /// Synthesized (server-side) audio has no sender key: it cannot enter an encrypted channel.
    fn reject_plaintext_injection(channel: &Arc<MediaChannel>) -> Result<()> {
        if channel.is_e2ee() {
            return Err(AurixError::Validation(
                "cannot inject server-side audio into an end-to-end encrypted channel".into(),
            ));
        }
        Ok(())
    }

    fn channel_for_sender(
        &self,
        channel_id: &ChannelId,
        sender: &Arc<MediaSession>,
    ) -> Result<Arc<MediaChannel>> {
        let channel = self
            .shared
            .channels
            .get(channel_id)
            .map(|c| c.value().clone())
            .ok_or_else(|| AurixError::ChannelNotFound(channel_id.to_string()))?;
        if channel.app_id != sender.app_id || !channel.has_participant(&sender.user_id) {
            return Err(AurixError::AuthorizationDenied(
                "Sender is not a participant of this channel".into(),
            ));
        }
        Ok(channel)
    }

    fn transcode_pcmu_uplink(
        &self,
        sender: &Arc<MediaSession>,
        packet: &AurixPacket,
    ) -> Result<AurixPacket> {
        let mut slot = sender.pcmu_uplink.lock();
        let uplink = match slot.as_mut() {
            Some(u) => u,
            None => slot.insert(PcmuUplink::new()?),
        };
        let opus = match uplink.transcode(&packet.payload) {
            Ok(o) => o,
            Err(e) => {
                aurix_metrics::PCMU_FRAMES
                    .with_label_values(&["uplink", "error"])
                    .inc();
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(e);
            }
        };
        aurix_metrics::PCMU_FRAMES
            .with_label_values(&["uplink", "ok"])
            .inc();
        let mut header = packet.header.clone();
        header.flags &= !(PacketFlags::Pcmu as u16);
        Ok(AurixPacket::new(header, opus))
    }

    /// μ-law copy of an Opus frame for PCMU receivers; `None` when it cannot be decoded.
    fn pcmu_downlink_frame(&self, packet: &AurixPacket) -> Option<Bytes> {
        let key = (packet.header.ssrc, packet.header.channel_id_hash);
        if !self.pcmu_downlinks.contains_key(&key) {
            if self.pcmu_downlinks.len() >= PCMU_DOWNLINK_PRUNE_AT {
                self.pcmu_downlinks
                    .retain(|_, d| d.lock().last_used.elapsed() < PCMU_DOWNLINK_IDLE);
            }
            match PcmuDownlink::new() {
                Ok(d) => {
                    self.pcmu_downlinks
                        .entry(key)
                        .or_insert_with(|| Mutex::new(d));
                }
                Err(e) => {
                    warn!("PCMU downlink decoder: {e}");
                    return None;
                }
            }
        }
        let entry = self.pcmu_downlinks.get(&key)?;
        let result = entry.lock().transcode(&packet.payload);
        drop(entry);
        match result {
            Ok(ulaw) => {
                aurix_metrics::PCMU_FRAMES
                    .with_label_values(&["downlink", "ok"])
                    .inc();
                Some(ulaw)
            }
            Err(e) => {
                aurix_metrics::PCMU_FRAMES
                    .with_label_values(&["downlink", "error"])
                    .inc();
                debug!("PCMU downlink transcode failed: {e}");
                None
            }
        }
    }

    /// Forget the μ-law decoders of a sender that left (or of a whole channel).
    pub fn forget_pcmu_downlinks(&self, sender_ssrc: Option<u32>, channel_hash: Option<u32>) {
        self.pcmu_downlinks.retain(|(ssrc, hash), _| {
            !(sender_ssrc.is_none_or(|s| s == *ssrc) && channel_hash.is_none_or(|h| h == *hash))
        });
    }

    fn tap_audio(
        &self,
        channel: &Arc<MediaChannel>,
        sender: &Arc<MediaSession>,
        packet: &AurixPacket,
    ) {
        if packet.header.has_flag(PacketFlags::E2ee) {
            return;
        }
        self.tap_sink(channel, sender.user_id, sender.ssrc, packet);
        if let Some(ref pipeline) = self.audio_pipeline {
            pipeline.process_opus_packet(channel, sender.user_id, &packet.payload);
        }
    }

    /// Recording / live-stream sink only. Relayed audio from other nodes goes here too so a
    /// tap sees every participant of a cascaded channel, while transcription stays with the
    /// node that owns the speaker (each node publishes its own transcripts).
    fn tap_sink(
        &self,
        channel: &Arc<MediaChannel>,
        user_id: UserId,
        ssrc: u32,
        packet: &AurixPacket,
    ) {
        if let Some(ref sink) = self.audio_sink {
            if sink.wants_channel(&channel.channel_id) {
                sink.on_audio(
                    channel.channel_id,
                    user_id,
                    ssrc,
                    packet.header.timestamp,
                    &packet.payload,
                );
            }
        }
    }

    /// Fast-path UDP send: try the non-blocking syscall first and only park the task on
    /// socket writability when the kernel send buffer is actually full.
    async fn send_udp(&self, out: &[u8], addr: SocketAddr) -> std::io::Result<usize> {
        match self.socket.try_send_to(out, addr) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.socket.send_to(out, addr).await
            }
            Err(e) => Err(e),
        }
    }

    async fn fan_out(
        &self,
        channel: &Arc<MediaChannel>,
        sender: &Arc<MediaSession>,
        packet: &AurixPacket,
    ) {
        let receivers = channel.get_receivers_for_audio(sender.ssrc);
        self.deliver(channel, receivers, packet, Some(sender.user_id))
            .await;
    }

    /// Hands the native receivers served by a server mix (`DownlinkMode::Mixed` or listeners
    /// of a `mix_for_listeners` channel) to the mix hub and returns the rest, which get
    /// per-speaker streams. E2EE frames cannot be mixed and always go per-speaker.
    /// `shared_ok = false` marks a frame meant for the listed receivers only (it may not
    /// enter a mixer other members subscribe to).
    fn split_mixed(
        &self,
        channel: &Arc<MediaChannel>,
        receivers: Vec<(Arc<MediaSession>, Mix)>,
        packet: &AurixPacket,
        e2ee: bool,
        shared_ok: bool,
    ) -> Vec<(Arc<MediaSession>, Mix)> {
        let Some(hub) = self.mix.as_ref().filter(|_| !e2ee) else {
            return receivers;
        };
        if !channel.may_mix() {
            return receivers;
        }
        let mut streams = Vec::with_capacity(receivers.len());
        let mut mixed = Vec::new();
        for (receiver, mix) in receivers {
            if receiver.transport() == Transport::Aurx
                && receiver.is_active()
                && channel.wants_mix(&receiver)
            {
                mixed.push((receiver, mix));
            } else {
                streams.push((receiver, mix));
            }
        }
        if !mixed.is_empty() {
            streams.extend(hub.push(channel, mixed, packet, shared_ok));
        }
        streams
    }

    /// `speaker`: the participant whose microphone the frame is (a WebRTC receiver may carry
    /// them on a per-participant track); `None` for synthesized audio, which is always mixed.
    async fn deliver(
        &self,
        channel: &Arc<MediaChannel>,
        receivers: Vec<(Arc<MediaSession>, Mix)>,
        packet: &AurixPacket,
        speaker: Option<UserId>,
    ) {
        self.deliver_scoped(channel, receivers, packet, true, speaker)
            .await;
    }

    async fn deliver_scoped(
        &self,
        channel: &Arc<MediaChannel>,
        receivers: Vec<(Arc<MediaSession>, Mix)>,
        packet: &AurixPacket,
        shared_ok: bool,
        speaker: Option<UserId>,
    ) {
        // Downlink packets are sealed per receiver (encrypted + authenticated with the receiver's
        // session keys) with that receiver's gain/direction metadata in front of the frame.
        let e2ee = packet.header.has_flag(PacketFlags::E2ee);
        let receivers = self.split_mixed(channel, receivers, packet, e2ee, shared_ok);
        let speaker = speaker.filter(|_| channel.browser_reproducible_gain());
        // Computed once per packet, on the first PCMU receiver.
        let mut pcmu_frame: Option<Option<Bytes>> = None;
        for (receiver, mix) in receivers {
            if !receiver.is_active() {
                continue;
            }
            // Only clients that announced E2EE support get encrypted frames; the rest would
            // drop them anyway.
            if e2ee && !receiver.is_e2ee_capable() {
                continue;
            }
            match receiver.transport() {
                Transport::WebRtc => {
                    if e2ee && speaker.is_none() {
                        aurix_metrics::PACKETS_DROPPED.inc();
                        continue;
                    }
                    if let Some(ref webrtc) = self.webrtc {
                        let ok = webrtc.send_to_session(
                            &receiver.session_id,
                            ForwardMedia {
                                speaker,
                                sender_ssrc: packet.header.ssrc,
                                sender_ts: packet.header.timestamp,
                                volume: mix.volume,
                                direction: mix.direction,
                                e2ee,
                                payload: packet.payload.to_vec(),
                            },
                        );
                        if ok {
                            receiver.record_packet_sent(packet.payload.len() as u64);
                        } else {
                            aurix_metrics::PACKETS_DROPPED.inc();
                        }
                    }
                }
                Transport::Aurx => {
                    let Some(endpoint) = receiver.endpoint() else {
                        continue;
                    };
                    let out = if e2ee {
                        AurixPacket::seal_parts(&packet.header, &packet.payload, &receiver.keys)
                    } else if receiver.codec() == AudioCodec::Pcmu {
                        let frame = pcmu_frame
                            .get_or_insert_with(|| self.pcmu_downlink_frame(packet))
                            .clone();
                        let Some(frame) = frame else {
                            aurix_metrics::PACKETS_DROPPED.inc();
                            continue;
                        };
                        let (mut header, body) =
                            packet.downlink_parts_with(&frame, mix.volume, mix.direction.as_ref());
                        header.flags |= PacketFlags::Pcmu as u16;
                        AurixPacket::seal_parts(&header, &body, &receiver.keys)
                    } else {
                        let (header, body) =
                            packet.downlink_parts(mix.volume, mix.direction.as_ref());
                        AurixPacket::seal_parts(&header, &body, &receiver.keys)
                    };
                    match endpoint {
                        MediaEndpoint::Udp(addr) => match self.send_udp(&out, addr).await {
                            Ok(n) => {
                                receiver.record_packet_sent(n as u64);
                                aurix_metrics::PACKETS_SENT.inc();
                                aurix_metrics::BYTES_SENT.inc_by(n as u64);
                            }
                            Err(e) => {
                                aurix_metrics::PACKETS_DROPPED.inc();
                                warn!(
                                    "Failed to send audio to {} in {}: {}",
                                    receiver.user_id, channel.channel_id, e
                                );
                            }
                        },
                        MediaEndpoint::Tunnel(tunnel) => {
                            let n = out.len() as u64;
                            if tunnel.send(out.to_vec()) {
                                receiver.record_packet_sent(n);
                                aurix_metrics::PACKETS_SENT.inc();
                                aurix_metrics::BYTES_SENT.inc_by(n);
                            } else {
                                aurix_metrics::PACKETS_DROPPED.inc();
                            }
                        }
                    }
                }
            }
        }
    }

    async fn handle_heartbeat(
        &self,
        packet: &AurixPacket,
        session: &Arc<MediaSession>,
        source: PacketSource<'_>,
    ) -> Result<()> {
        session.update_heartbeat();
        let ack = AurixPacket::new(
            PacketHeader::new(
                PacketType::HeartbeatAck,
                session.next_downlink_sequence(),
                packet.header.timestamp,
                packet.header.ssrc,
            ),
            Bytes::new(),
        );
        let bytes = if packet.is_authenticated() {
            ack.seal(&session.keys)
        } else {
            ack.encode()
        };
        self.reply(source, &bytes).await;
        Ok(())
    }

    fn handle_quality_report(&self, packet: &AurixPacket, session: &Arc<MediaSession>) {
        if packet.payload.len() < 12 {
            return;
        }
        let p = &packet.payload;
        let rtt = f32::from_be_bytes([p[0], p[1], p[2], p[3]]);
        let jitter = f32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        let loss = f32::from_be_bytes([p[8], p[9], p[10], p[11]]);
        if !(rtt.is_finite() && jitter.is_finite() && loss.is_finite()) {
            return;
        }
        let mut metrics = QualityMetrics {
            rtt_ms: rtt.clamp(0.0, 10_000.0),
            jitter_ms: jitter.clamp(0.0, 10_000.0),
            packet_loss_percent: loss.clamp(0.0, 100.0),
            bitrate_kbps: 0,
            mos_score: 0.0,
        };
        metrics.mos_score = metrics.calculate_mos();
        aurix_metrics::RTT_MS.observe(metrics.rtt_ms as f64);
        aurix_metrics::JITTER_MS.observe(metrics.jitter_ms as f64);
        aurix_metrics::PACKET_LOSS_RATE.set(metrics.packet_loss_percent as f64);
        session.update_quality(metrics);
    }

    fn handle_mute_state(&self, packet: &AurixPacket, session: &Arc<MediaSession>) {
        let Some(&flag) = packet.payload.first() else {
            return;
        };
        let muted = flag != 0;
        let prev = session
            .is_muted
            .swap(muted, std::sync::atomic::Ordering::Relaxed);
        if prev != muted {
            let _ = self.events.send(MediaEvent::MuteChanged {
                session_id: session.session_id,
                user_id: session.user_id,
                channels: session.get_channels(),
                muted,
            });
        }
    }
}
