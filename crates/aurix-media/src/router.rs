//! AURX/UDP packet router.
//!
//! Security model:
//! * A UDP source address is only associated with a session after an authenticated
//!   `SessionBind` (HMAC with the per-session media key handed out over the WebSocket).
//! * Every subsequent packet must come from the bound address; when
//!   `require_packet_auth` is on it must also carry a valid HMAC tag and a fresh
//!   sequence number (64-packet anti-replay window shared by audio and control packets).
//! * The SSRC in the header is never used to look up a sender.

use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::*;
use aurix_common::sink::AudioSink;
use aurix_common::types::*;
use bytes::{BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::audio_pipeline::AudioAnalysisPipeline;
use crate::cascade::CascadeRelay;
use crate::channel::MediaChannel;
use crate::session::{MediaSession, Transport};
use crate::webrtc::{ForwardMedia, WebRtcManager};

/// Notifications from the media plane to the control plane (WebSocket layer).
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// UDP address authenticated for the session.
    SessionBound {
        session_id: SessionId,
        addr: SocketAddr,
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
}

pub struct RouterShared {
    pub channels: Arc<DashMap<ChannelId, Arc<MediaChannel>>>,
    pub channels_by_hash: Arc<DashMap<u32, ChannelId>>,
    pub sessions_by_id: Arc<DashMap<SessionId, Arc<MediaSession>>>,
    pub sessions_by_addr: Arc<DashMap<SocketAddr, Arc<MediaSession>>>,
}

pub struct PacketRouter {
    shared: RouterShared,
    socket: Arc<UdpSocket>,
    cascade: Option<Arc<CascadeRelay>>,
    audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
    webrtc: Option<Arc<WebRtcManager>>,
    audio_sink: Option<Arc<dyn AudioSink>>,
    require_packet_auth: bool,
    events: broadcast::Sender<MediaEvent>,
}

impl PacketRouter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: RouterShared,
        socket: Arc<UdpSocket>,
        cascade: Option<Arc<CascadeRelay>>,
        audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
        webrtc: Option<Arc<WebRtcManager>>,
        audio_sink: Option<Arc<dyn AudioSink>>,
        require_packet_auth: bool,
        events: broadcast::Sender<MediaEvent>,
    ) -> Self {
        Self {
            shared,
            socket,
            cascade,
            audio_pipeline,
            webrtc,
            audio_sink,
            require_packet_auth,
            events,
        }
    }

    pub async fn route_packet(&self, data: &[u8], src_addr: SocketAddr) -> Result<()> {
        let mut packet = AurixPacket::decode(data)?;
        if packet.header.packet_type == PacketType::SessionBind {
            return self.handle_session_bind(&packet, src_addr).await;
        }

        let session = self.authenticate(&mut packet, src_addr)?;
        match packet.header.packet_type {
            PacketType::Audio | PacketType::AudioFec => {
                self.route_audio_packet(&packet, &session).await
            }
            PacketType::Heartbeat => self.handle_heartbeat(&packet, &session, src_addr).await,
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

    /// Resolve the sender by *bound address*, verify the tag, decrypt the payload in place and
    /// check the anti-replay window.
    fn authenticate(
        &self,
        packet: &mut AurixPacket,
        src_addr: SocketAddr,
    ) -> Result<Arc<MediaSession>> {
        let session = self
            .shared
            .sessions_by_addr
            .get(&src_addr)
            .map(|s| s.value().clone())
            .ok_or_else(|| AurixError::AuthenticationFailed("Unbound media source".into()))?;
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

    async fn handle_session_bind(&self, packet: &AurixPacket, src_addr: SocketAddr) -> Result<()> {
        if packet.header.has_flag(PacketFlags::Encrypted) {
            return Err(AurixError::AuthenticationFailed(
                "SessionBind must be signed, not encrypted".into(),
            ));
        }
        let (session_id, unix_ms, _nonce) = packet.parse_session_bind()?;
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

        if let Some(old) = session.get_remote_addr() {
            if old != src_addr {
                self.shared.sessions_by_addr.remove(&old);
            }
        }
        session.set_transport(Transport::Aurx);
        session.set_remote_addr(src_addr);
        session.update_heartbeat();
        self.shared
            .sessions_by_addr
            .insert(src_addr, session.clone());

        let ack =
            AurixPacket::session_bind_ack(session.ssrc, session.next_downlink_sequence(), now)
                .seal(&session.keys);
        let _ = self.socket.send_to(&ack, src_addr).await;
        let _ = self.events.send(MediaEvent::SessionBound {
            session_id,
            addr: src_addr,
        });
        debug!("Session {} bound to {}", session_id, src_addr);
        Ok(())
    }

    async fn route_audio_packet(
        &self,
        packet: &AurixPacket,
        sender: &Arc<MediaSession>,
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

        if sender.mark_audio_activity() {
            let _ = self.events.send(MediaEvent::SpeakingChanged {
                session_id: sender.session_id,
                user_id: sender.user_id,
                channels: vec![channel_id],
                speaking: true,
            });
        }

        self.tap_audio(&channel, sender, packet);
        self.fan_out(&channel, sender, packet).await;
        if let Some(ref cascade) = self.cascade {
            cascade
                .forward_to_peers(&channel_id, &sender.user_id, packet)
                .await;
        }
        Ok(())
    }

    /// Route depayloaded Opus audio received from a WebRTC session.
    pub async fn route_webrtc_audio(
        &self,
        sender: &Arc<MediaSession>,
        rtp_time: u32,
        payload: Vec<u8>,
    ) -> Result<()> {
        sender.record_packet_received(payload.len() as u64);
        sender.update_heartbeat();
        aurix_metrics::PACKETS_RECEIVED.inc();
        aurix_metrics::BYTES_RECEIVED.inc_by(payload.len() as u64);
        if !sender.is_transmitting_allowed() {
            return Ok(());
        }
        let channels = sender.get_channels();
        if channels.is_empty() {
            return Ok(());
        }
        if sender.mark_audio_activity() {
            let _ = self.events.send(MediaEvent::SpeakingChanged {
                session_id: sender.session_id,
                user_id: sender.user_id,
                channels: channels.clone(),
                speaking: true,
            });
        }
        let seq = sender
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for channel_id in channels {
            let channel = match self.shared.channels.get(&channel_id) {
                Some(c) => c.value().clone(),
                None => continue,
            };
            if !channel.can_transmit(&sender.user_id) {
                continue;
            }
            let packet = AurixPacket::audio(
                seq,
                rtp_time,
                sender.ssrc,
                channel_id_hash(&channel_id),
                Bytes::from(payload.clone()),
            );
            self.tap_audio(&channel, sender, &packet);
            self.fan_out(&channel, sender, &packet).await;
            if let Some(ref cascade) = self.cascade {
                cascade
                    .forward_to_peers(&channel_id, &sender.user_id, &packet)
                    .await;
            }
        }
        Ok(())
    }

    /// Inject a packet relayed from another node (already authenticated by the cascade layer);
    /// `sender` is the remote participant it originates from.
    pub async fn route_relayed_audio(&self, sender: &UserId, packet: &AurixPacket) -> Result<()> {
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
        let receivers = channel.get_receivers_for_relayed_audio(sender);
        self.deliver(&channel, receivers, packet).await;
        Ok(())
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
        if let Some(ref sink) = self.audio_sink {
            if sink.wants_channel(&channel.channel_id) {
                sink.on_audio(
                    channel.channel_id,
                    sender.user_id,
                    sender.ssrc,
                    packet.header.timestamp,
                    &packet.payload,
                );
            }
        }
        if let Some(ref pipeline) = self.audio_pipeline {
            if pipeline.is_enabled() {
                pipeline.process_opus_packet(sender.user_id, channel.channel_id, &packet.payload);
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
        self.deliver(channel, receivers, packet).await;
    }

    async fn deliver(
        &self,
        channel: &Arc<MediaChannel>,
        receivers: Vec<(Arc<MediaSession>, f32)>,
        packet: &AurixPacket,
    ) {
        // Downlink packets are sealed per receiver (encrypted + authenticated with the receiver's
        // session keys); the volume-attenuated body is built once per distinct volume on demand.
        let e2ee = packet.header.has_flag(PacketFlags::E2ee);
        for (receiver, volume) in receivers {
            if !receiver.is_active() {
                continue;
            }
            match receiver.transport() {
                Transport::WebRtc => {
                    if e2ee {
                        continue;
                    }
                    if let Some(ref webrtc) = self.webrtc {
                        let ok = webrtc.send_to_session(
                            &receiver.session_id,
                            ForwardMedia {
                                sender_ssrc: packet.header.ssrc,
                                volume,
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
                    let Some(addr) = receiver.get_remote_addr() else {
                        continue;
                    };
                    let out = if !e2ee && (volume - 1.0).abs() > 0.01 {
                        let (header, body) = Self::attenuated_body(packet, volume);
                        AurixPacket::seal_parts(&header, &body, &receiver.keys)
                    } else {
                        AurixPacket::seal_parts(&packet.header, &packet.payload, &receiver.keys)
                    };
                    match self.send_udp(&out, addr).await {
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
                    }
                }
            }
        }
    }

    /// Payload with a leading one-byte gain factor and a header carrying `VolumeAttenuated`.
    fn attenuated_body(packet: &AurixPacket, volume: f32) -> (PacketHeader, BytesMut) {
        let mut header = packet.header.clone();
        header.flags |= PacketFlags::VolumeAttenuated as u16;
        let mut body = BytesMut::with_capacity(1 + packet.payload.len());
        body.put_u8(encode_volume_byte(volume));
        body.put_slice(&packet.payload);
        (header, body)
    }

    async fn handle_heartbeat(
        &self,
        packet: &AurixPacket,
        session: &Arc<MediaSession>,
        src_addr: SocketAddr,
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
        let _ = self.socket.send_to(&bytes, src_addr).await;
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
