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
use aurix_common::g711::Law;
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
use crate::denoise::{Cleaned, DenoisePool, UplinkDenoiser};
use crate::mix::MixHub;
use crate::quic::QuicLink;
use crate::session::g711_metric_label;
use crate::session::{MediaEndpoint, MediaSession, Transport};
use crate::tls::TlsLink;
use crate::transcode::{G711Downlink, G711Uplink, Transcoded};
use crate::tunnel::MediaTunnel;
use crate::webrtc::{ForwardMedia, WebRtcManager};
use crate::webtransport::WebTransportLink;

/// Sweep idle G.711 downlink decoders when the table grows past this.
const G711_DOWNLINK_PRUNE_AT: usize = 256;
/// A G.711 downlink decoder unused this long belongs to a stream that ended.
const G711_DOWNLINK_IDLE: Duration = Duration::from_secs(30);

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
    /// Speaker slots of a channel changed hands (`audience.speaker_admission`): idle
    /// speakers were demoted and/or waiting members admitted.
    RolesChanged {
        app_id: AppId,
        channel_id: ChannelId,
        changes: Vec<crate::channel::RoleChange>,
    },
}

/// Where an uplink packet came from.
#[derive(Clone, Copy)]
enum PacketSource<'a> {
    Udp(SocketAddr),
    Tunnel(&'a Arc<MediaTunnel>),
    Quic(&'a Arc<QuicLink>),
    Tls(&'a Arc<TlsLink>),
    WebTransport(&'a Arc<WebTransportLink>),
}

impl std::fmt::Display for PacketSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketSource::Udp(addr) => write!(f, "{addr}"),
            PacketSource::Tunnel(t) => write!(f, "tunnel#{}", t.id()),
            PacketSource::Quic(l) => write!(f, "quic#{} ({})", l.id(), l.remote_address()),
            PacketSource::Tls(l) => write!(f, "tls#{} ({})", l.id(), l.remote_address()),
            PacketSource::WebTransport(l) => {
                write!(f, "webtransport#{} ({})", l.id(), l.remote_address())
            }
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
    /// Opus → G.711 decoders for PCMU / PCMA receivers, per `(sender ssrc, channel hash, law)`.
    g711_downlinks: DashMap<(u32, u32, Law), Mutex<G711Downlink>>,
    /// Server-side mixers for native receivers in `DownlinkMode::Mixed`
    /// (`None`: `media.downlink_mix = false`, everyone gets per-speaker streams).
    mix: Option<Arc<MixHub>>,
    /// Budget of uplinks the node denoises (`media.noise_suppression`).
    denoise: Arc<DenoisePool>,
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
        denoise: Arc<DenoisePool>,
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
            g711_downlinks: DashMap::new(),
            mix,
            denoise,
        }
    }

    /// Server-side downlink mixers of this node, if enabled.
    pub fn mix_hub(&self) -> Option<&Arc<MixHub>> {
        self.mix.as_ref()
    }

    /// The node's uplink denoiser budget.
    pub fn denoise_pool(&self) -> &Arc<DenoisePool> {
        &self.denoise
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

    /// Routes one AURX packet that arrived as a QUIC datagram on `link`. Also notices peer
    /// address changes (connection migration) of links bound to a session.
    pub async fn route_quic_packet(&self, data: &[u8], link: &Arc<QuicLink>) -> Result<()> {
        if let Some(old) = link.observe_path() {
            if let Some(session_id) = link.session_id() {
                aurix_metrics::QUIC_MIGRATIONS.inc();
                debug!(
                    "Session {} migrated quic#{} {} -> {}",
                    session_id,
                    link.id(),
                    old,
                    link.remote_address()
                );
            }
        }
        let res = self.route_from(data, PacketSource::Quic(link)).await;
        aurix_metrics::QUIC_PACKETS
            .with_label_values(&["uplink", if res.is_ok() { "received" } else { "rejected" }])
            .inc();
        res
    }

    /// A QUIC connection closed: drop it as the media path of the session it was bound to
    /// (unless a newer bind already moved the session elsewhere).
    pub fn quic_link_closed(&self, link: &QuicLink) -> bool {
        link.session_id().is_some_and(|session_id| {
            self.shared
                .sessions_by_id
                .get(&session_id)
                .is_some_and(|s| s.clear_quic(link))
        })
    }

    /// Routes one AURX packet that arrived as a frame on the TLS tunnel connection `link`.
    pub async fn route_tls_packet(&self, data: &[u8], link: &Arc<TlsLink>) -> Result<()> {
        let res = self.route_from(data, PacketSource::Tls(link)).await;
        aurix_metrics::TLS_TUNNEL_PACKETS
            .with_label_values(&["uplink", if res.is_ok() { "received" } else { "rejected" }])
            .inc();
        res
    }

    /// A TLS tunnel connection closed: drop it as the media path of the session it was bound
    /// to (unless a newer bind already moved the session elsewhere).
    pub fn tls_link_closed(&self, link: &TlsLink) -> bool {
        link.session_id().is_some_and(|session_id| {
            self.shared
                .sessions_by_id
                .get(&session_id)
                .is_some_and(|s| s.clear_tls(link))
        })
    }

    /// Routes one AURX packet that arrived as a datagram on the WebTransport session `link`.
    pub async fn route_webtransport_packet(
        &self,
        data: &[u8],
        link: &Arc<WebTransportLink>,
    ) -> Result<()> {
        let res = self
            .route_from(data, PacketSource::WebTransport(link))
            .await;
        aurix_metrics::WEBTRANSPORT_PACKETS
            .with_label_values(&["uplink", if res.is_ok() { "received" } else { "rejected" }])
            .inc();
        res
    }

    /// A WebTransport session closed: drop it as the media path of the session it was bound
    /// to (unless a newer bind already moved the session elsewhere).
    pub fn webtransport_link_closed(&self, link: &WebTransportLink) -> bool {
        link.session_id().is_some_and(|session_id| {
            self.shared
                .sessions_by_id
                .get(&session_id)
                .is_some_and(|s| s.clear_webtransport(link))
        })
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

    /// Resolve the sender by *bound address* (UDP) or by the *authenticated connection*
    /// (tunnel, QUIC), verify the tag, decrypt the payload in place and check the anti-replay
    /// window.
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
            PacketSource::Quic(link) => {
                // The connection speaks for a session only after an authenticated
                // `SessionBind` arrived on it, and only while it is still the session's path.
                let session_id = link.session_id().ok_or_else(|| {
                    AurixError::AuthenticationFailed("Unbound QUIC connection".into())
                })?;
                let session = self
                    .shared
                    .sessions_by_id
                    .get(&session_id)
                    .map(|s| s.value().clone())
                    .ok_or_else(|| AurixError::SessionNotFound("QUIC session is gone".into()))?;
                if !session.quic().is_some_and(|l| *l == **link) {
                    return Err(AurixError::AuthenticationFailed(
                        "Stale QUIC connection".into(),
                    ));
                }
                session
            }
            PacketSource::Tls(link) => {
                let session_id = link.session_id().ok_or_else(|| {
                    AurixError::AuthenticationFailed("Unbound TLS tunnel connection".into())
                })?;
                let session = self
                    .shared
                    .sessions_by_id
                    .get(&session_id)
                    .map(|s| s.value().clone())
                    .ok_or_else(|| {
                        AurixError::SessionNotFound("TLS tunnel session is gone".into())
                    })?;
                if !session.tls().is_some_and(|l| *l == **link) {
                    return Err(AurixError::AuthenticationFailed(
                        "Stale TLS tunnel connection".into(),
                    ));
                }
                session
            }
            PacketSource::WebTransport(link) => {
                let session_id = link.session_id().ok_or_else(|| {
                    AurixError::AuthenticationFailed("Unbound WebTransport session".into())
                })?;
                let session = self
                    .shared
                    .sessions_by_id
                    .get(&session_id)
                    .map(|s| s.value().clone())
                    .ok_or_else(|| {
                        AurixError::SessionNotFound("WebTransport session is gone".into())
                    })?;
                if !session.webtransport().is_some_and(|l| *l == **link) {
                    return Err(AurixError::AuthenticationFailed(
                        "Stale WebTransport session".into(),
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
        // A connection that already authenticated as one session stays that session's.
        let owner = match source {
            PacketSource::Quic(link) => link.session_id(),
            PacketSource::Tls(link) => link.session_id(),
            PacketSource::WebTransport(link) => link.session_id(),
            PacketSource::Udp(_) | PacketSource::Tunnel(_) => None,
        };
        if owner.is_some_and(|owner| owner != session_id) {
            aurix_metrics::PACKETS_DROPPED.inc();
            return Err(AurixError::AuthenticationFailed(
                "SessionBind for a session the connection does not own".into(),
            ));
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
                if let Some(old) = session.set_remote_addr(src_addr) {
                    if old != src_addr {
                        self.shared.sessions_by_addr.remove(&old);
                    }
                }
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
            PacketSource::Quic(link) => {
                if !link.claim(session_id) {
                    return Err(AurixError::AuthenticationFailed(
                        "QUIC connection already owned by another session".into(),
                    ));
                }
                if let Some(old) = session.set_quic(link.clone()) {
                    self.shared.sessions_by_addr.remove(&old);
                }
                MediaTransportKind::Quic
            }
            PacketSource::Tls(link) => {
                if !link.claim(session_id) {
                    return Err(AurixError::AuthenticationFailed(
                        "TLS tunnel connection already owned by another session".into(),
                    ));
                }
                if let Some(old) = session.set_tls(link.clone()) {
                    self.shared.sessions_by_addr.remove(&old);
                }
                MediaTransportKind::Tls
            }
            PacketSource::WebTransport(link) => {
                if !link.claim(session_id) {
                    return Err(AurixError::AuthenticationFailed(
                        "WebTransport session already owned by another session".into(),
                    ));
                }
                if let Some(old) = session.set_webtransport(link.clone()) {
                    self.shared.sessions_by_addr.remove(&old);
                }
                MediaTransportKind::WebTransport
            }
        };
        session.update_heartbeat();

        // Observers learn of the bind no later than the client does.
        let _ = self.events.send(MediaEvent::SessionBound {
            session_id,
            transport,
        });
        let ack =
            AurixPacket::session_bind_ack(session.ssrc, session.next_downlink_sequence(), now)
                .seal(&session.keys);
        self.reply(source, &ack).await;
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
            PacketSource::Quic(link) => {
                link.send(Bytes::copy_from_slice(bytes));
            }
            PacketSource::Tls(link) => {
                link.send(bytes.to_vec());
            }
            PacketSource::WebTransport(link) => {
                link.send(bytes);
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
        if channel.app_id != sender.app_id {
            return Err(AurixError::AuthorizationDenied(
                "Sender may not transmit in this channel".into(),
            ));
        }
        if !channel.can_transmit(&sender.user_id) {
            channel.note_speak_attempt(&sender.user_id);
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

        // Node-side denoising: an encrypted frame cannot be, a frame into a stereo channel is
        // not (the model is mono speech); a G.711 frame is cleaned inside its transcode below.
        let e2ee = packet.header.has_flag(PacketFlags::E2ee);
        let denoise = !e2ee
            && !channel.is_stereo()
            && self.arm_denoiser(sender, channel.requires_noise_suppression());

        // A plaintext G.711 uplink enters the channel as Opus so recording, transcription,
        // cascade and Opus receivers never see μ-law / A-law; G.711 receivers get it re-encoded
        // in `deliver`. A sealed G.711 frame is as opaque as a sealed Opus one: it is relayed
        // with its codec flag so the receivers pick the right decoder after opening it.
        let frame_codec = packet.header.audio_codec();
        let transcoded;
        let packet = if frame_codec.is_g711() {
            if sender.codec() != frame_codec {
                aurix_metrics::PACKETS_DROPPED.inc();
                return Err(AurixError::AuthorizationDenied(format!(
                    "{frame_codec:?} frame from a session that did not negotiate it"
                )));
            }
            if e2ee {
                packet
            } else {
                transcoded = self.transcode_g711_uplink(sender, packet, denoise)?;
                &transcoded
            }
        } else if let Some(cleaned) = denoise
            .then(|| self.denoise_opus(sender, packet.header.timestamp, &packet.payload))
            .flatten()
        {
            transcoded = AurixPacket::new(packet.header.clone(), cleaned);
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
        // Cleaned once, before the per-channel copies; never when the frame is encrypted
        // end-to-end or any target channel is stereo (the model is mono speech).
        let (stereo, required) = channels.iter().fold((false, false), |(s, r), c| {
            match self.shared.channels.get(c) {
                Some(ch) => (s || ch.is_stereo(), r || ch.requires_noise_suppression()),
                None => (s, r),
            }
        });
        let payload = if !e2ee && !stereo && self.arm_denoiser(sender, required) {
            self.denoise_opus(sender, rtp_time, &payload)
                .map_or(payload, |cleaned| cleaned.to_vec())
        } else {
            payload
        };
        for channel_id in channels {
            let channel = match self.shared.channels.get(&channel_id) {
                Some(c) => c.value().clone(),
                None => continue,
            };
            if !channel.can_transmit(&sender.user_id) {
                channel.note_speak_attempt(&sender.user_id);
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
                channel.note_speak_attempt(&sender.user_id);
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

    /// Makes sure `sender` holds denoiser state when its uplink is to be cleaned — because the
    /// client asked or `required` by the channel — and drops it once neither the client nor
    /// any of the sender's channels wants it any more. `false` when the frame is to be
    /// forwarded as it came (including a channel requirement the node cannot honour right
    /// now: denoiser disabled or `max_sessions` busy).
    fn arm_denoiser(&self, sender: &Arc<MediaSession>, required: bool) -> bool {
        let requested = sender.noise_suppression();
        if !requested && !required {
            let mut denoiser = sender.denoiser.lock();
            if denoiser.is_some() && !self.any_channel_requires_denoise(sender) {
                *denoiser = None;
            }
            return false;
        }
        let mut denoiser = sender.denoiser.lock();
        if denoiser.is_some() {
            return true;
        }
        match UplinkDenoiser::acquire(&self.denoise) {
            Some(d) => {
                *denoiser = Some(d);
                true
            }
            None => {
                let path = if sender.codec().is_g711() {
                    "g711"
                } else {
                    "opus"
                };
                aurix_metrics::NOISE_SUPPRESSION_FRAMES
                    .with_label_values(&[path, "skipped"])
                    .inc();
                false
            }
        }
    }

    fn any_channel_requires_denoise(&self, sender: &Arc<MediaSession>) -> bool {
        sender.channels.read().iter().any(|c| {
            self.shared
                .channels
                .get(c)
                .is_some_and(|ch| ch.value().requires_noise_suppression())
        })
    }

    /// The cleaned copy of an Opus uplink frame; `None` when it is forwarded as it came — the
    /// cleaner cannot take it (see [`Cleaned`]) or failed on it. `timestamp` lets the copies a
    /// native client sends to each of its channels share one pass.
    fn denoise_opus(
        &self,
        sender: &Arc<MediaSession>,
        timestamp: u32,
        payload: &[u8],
    ) -> Option<Bytes> {
        let mut denoiser = sender.denoiser.lock();
        let denoiser = denoiser.as_mut()?;
        let (outcome, cleaned) = match denoiser.clean_opus(timestamp, payload) {
            Ok(Cleaned::Opus(bytes)) => ("ok", Some(bytes)),
            Ok(Cleaned::Passthrough) => ("passthrough", None),
            Ok(Cleaned::Repeated(bytes)) => ("repeated", bytes),
            Err(e) => {
                debug!(session = %sender.session_id, error = %e, "uplink denoise failed");
                ("error", None)
            }
        };
        aurix_metrics::NOISE_SUPPRESSION_FRAMES
            .with_label_values(&["opus", outcome])
            .inc();
        cleaned
    }

    fn transcode_g711_uplink(
        &self,
        sender: &Arc<MediaSession>,
        packet: &AurixPacket,
        denoise: bool,
    ) -> Result<AurixPacket> {
        let codec = packet.header.audio_codec();
        let law = codec
            .g711_law()
            .ok_or_else(|| AurixError::Validation("g711 transcode of an Opus frame".into()))?;
        let metric_codec = g711_metric_label(codec).unwrap_or("pcmu");
        let mut slot = sender.pcmu_uplink.lock();
        let uplink = match slot.as_mut() {
            Some(u) if u.law() == law => u,
            _ => slot.insert(G711Uplink::new(law)?),
        };
        let mut denoiser = sender.denoiser.lock();
        let narrowband = denoise
            .then(|| denoiser.as_mut())
            .flatten()
            .map(|d| d.narrowband());
        let cleaned = narrowband.is_some();
        let (opus, outcome) =
            match uplink.transcode(packet.header.timestamp, &packet.payload, narrowband) {
                Ok(Transcoded::Fresh(o)) => (o, "ok"),
                Ok(Transcoded::Repeated(o)) => (o, "repeated"),
                Err(e) => {
                    if cleaned {
                        aurix_metrics::NOISE_SUPPRESSION_FRAMES
                            .with_label_values(&["g711", "error"])
                            .inc();
                    }
                    aurix_metrics::G711_FRAMES
                        .with_label_values(&[metric_codec, "uplink", "error"])
                        .inc();
                    aurix_metrics::PACKETS_DROPPED.inc();
                    return Err(e);
                }
            };
        if cleaned {
            aurix_metrics::NOISE_SUPPRESSION_FRAMES
                .with_label_values(&["g711", outcome])
                .inc();
        }
        aurix_metrics::G711_FRAMES
            .with_label_values(&[metric_codec, "uplink", "ok"])
            .inc();
        let mut header = packet.header.clone();
        header.set_audio_codec(AudioCodec::Opus);
        Ok(AurixPacket::new(header, opus))
    }

    /// G.711 copy of an Opus frame for PCMU / PCMA receivers; `None` when it cannot be decoded.
    fn g711_downlink_frame(&self, packet: &AurixPacket, codec: AudioCodec) -> Option<Bytes> {
        let law = codec.g711_law()?;
        let metric_codec = g711_metric_label(codec)?;
        let key = (packet.header.ssrc, packet.header.channel_id_hash, law);
        if !self.g711_downlinks.contains_key(&key) {
            if self.g711_downlinks.len() >= G711_DOWNLINK_PRUNE_AT {
                self.g711_downlinks
                    .retain(|_, d| d.lock().last_used.elapsed() < G711_DOWNLINK_IDLE);
            }
            match G711Downlink::new(law) {
                Ok(d) => {
                    self.g711_downlinks
                        .entry(key)
                        .or_insert_with(|| Mutex::new(d));
                }
                Err(e) => {
                    warn!("G.711 downlink decoder: {e}");
                    return None;
                }
            }
        }
        let entry = self.g711_downlinks.get(&key)?;
        let result = entry.lock().transcode(&packet.payload);
        drop(entry);
        match result {
            Ok(coded) => {
                aurix_metrics::G711_FRAMES
                    .with_label_values(&[metric_codec, "downlink", "ok"])
                    .inc();
                Some(coded)
            }
            Err(e) => {
                aurix_metrics::G711_FRAMES
                    .with_label_values(&[metric_codec, "downlink", "error"])
                    .inc();
                debug!("G.711 downlink transcode failed: {e}");
                None
            }
        }
    }

    /// Forget the G.711 decoders of a sender that left (or of a whole channel).
    pub fn forget_g711_downlinks(&self, sender_ssrc: Option<u32>, channel_hash: Option<u32>) {
        self.g711_downlinks.retain(|(ssrc, hash, _), _| {
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
        // Computed once per packet and law, on the first PCMU / PCMA receiver.
        let mut g711_frames: [Option<Option<Bytes>>; 2] = [None, None];
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
                    } else if let Some(law) = receiver.codec().g711_law() {
                        let codec = receiver.codec();
                        let frame = g711_frames[law as usize]
                            .get_or_insert_with(|| self.g711_downlink_frame(packet, codec))
                            .clone();
                        let Some(frame) = frame else {
                            aurix_metrics::PACKETS_DROPPED.inc();
                            continue;
                        };
                        let (mut header, body) =
                            packet.downlink_parts_with(&frame, mix.volume, mix.direction.as_ref());
                        header.set_audio_codec(codec);
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
                        MediaEndpoint::Quic(link) => {
                            let n = out.len() as u64;
                            if link.send(out.freeze()) {
                                receiver.record_packet_sent(n);
                                aurix_metrics::PACKETS_SENT.inc();
                                aurix_metrics::BYTES_SENT.inc_by(n);
                            } else {
                                aurix_metrics::PACKETS_DROPPED.inc();
                            }
                        }
                        MediaEndpoint::Tls(link) => {
                            let n = out.len() as u64;
                            if link.send(out.to_vec()) {
                                receiver.record_packet_sent(n);
                                aurix_metrics::PACKETS_SENT.inc();
                                aurix_metrics::BYTES_SENT.inc_by(n);
                            } else {
                                aurix_metrics::PACKETS_DROPPED.inc();
                            }
                        }
                        MediaEndpoint::WebTransport(link) => {
                            let n = out.len() as u64;
                            if link.send(&out) {
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
