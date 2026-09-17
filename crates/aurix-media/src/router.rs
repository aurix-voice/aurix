use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::*;
use aurix_common::types::*;
use bytes::{BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

use crate::audio_pipeline::AudioAnalysisPipeline;
use crate::cascade::CascadeRelay;
use crate::channel::MediaChannel;
use crate::session::MediaSession;

pub struct PacketRouter {
    channels: Arc<DashMap<ChannelId, Arc<MediaChannel>>>,
    sessions_by_ssrc: Arc<DashMap<u32, Arc<MediaSession>>>,
    sessions_by_addr: Arc<DashMap<SocketAddr, Arc<MediaSession>>>,
    socket: Arc<UdpSocket>,
    cascade: Option<Arc<CascadeRelay>>,
    audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
}

impl PacketRouter {
    pub fn new(
        channels: Arc<DashMap<ChannelId, Arc<MediaChannel>>>,
        sessions_by_ssrc: Arc<DashMap<u32, Arc<MediaSession>>>,
        sessions_by_addr: Arc<DashMap<SocketAddr, Arc<MediaSession>>>,
        socket: Arc<UdpSocket>,
        cascade: Option<Arc<CascadeRelay>>,
        audio_pipeline: Option<Arc<AudioAnalysisPipeline>>,
    ) -> Self {
        Self { channels, sessions_by_ssrc, sessions_by_addr, socket, cascade, audio_pipeline }
    }

    pub async fn route_packet(&self, data: &[u8], src_addr: SocketAddr) -> Result<()> {
        let packet = AurixPacket::decode(data)?;
        match packet.header.packet_type {
            PacketType::Audio | PacketType::AudioFec => self.route_audio_packet(&packet, src_addr).await,
            PacketType::Heartbeat => self.handle_heartbeat(&packet, src_addr).await,
            PacketType::QualityReport => self.handle_quality_report(&packet, src_addr).await,
            PacketType::MuteState => self.handle_mute_state(&packet, src_addr).await,
            PacketType::SpeakingState => self.handle_speaking_state(&packet, src_addr).await,
            _ => { debug!("Unhandled packet type: {:?}", packet.header.packet_type); Ok(()) }
        }
    }

    /// Route audio from a WebRTC session (already decoded from SRTP by str0m).
    pub async fn route_webrtc_audio(&self, packet: &AurixPacket) -> Result<()> {
        let _sender_session = self.sessions_by_ssrc.get(&packet.header.ssrc)
            .map(|s| s.value().clone())
            .ok_or_else(|| AurixError::SessionNotFound("Unknown WebRTC sender SSRC".into()))?;

        if _sender_session.is_muted.load(std::sync::atomic::Ordering::Relaxed)
            || _sender_session.is_server_muted.load(std::sync::atomic::Ordering::Relaxed)
        { return Ok(()); }

        aurix_metrics::PACKETS_RECEIVED.inc();
        aurix_metrics::BYTES_RECEIVED.inc_by(packet.payload.len() as u64);

        let sender_channels = _sender_session.get_channels();
        for ch_id in &sender_channels {
            self.route_to_channel_receivers(ch_id, &_sender_session, packet).await;
            // Cascade to remote nodes
            if let Some(ref cascade) = self.cascade {
                cascade.forward_to_peers(ch_id, packet).await;
            }
        }
        Ok(())
    }

    async fn route_audio_packet(&self, packet: &AurixPacket, src_addr: SocketAddr) -> Result<()> {
        let sender_session = self.sessions_by_addr.get(&src_addr)
            .map(|s| s.value().clone())
            .or_else(|| self.sessions_by_ssrc.get(&packet.header.ssrc).map(|s| s.value().clone()))
            .ok_or_else(|| AurixError::SessionNotFound("Unknown sender".into()))?;

        if sender_session.get_remote_addr().is_none() {
            sender_session.set_remote_addr(src_addr);
            self.sessions_by_addr.insert(src_addr, sender_session.clone());
        }

        if sender_session.is_muted.load(std::sync::atomic::Ordering::Relaxed)
            || sender_session.is_server_muted.load(std::sync::atomic::Ordering::Relaxed)
        { return Ok(()); }

        sender_session.record_packet_received(packet.payload.len() as u64);
        sender_session.is_speaking.store(true, std::sync::atomic::Ordering::Relaxed);
        aurix_metrics::PACKETS_RECEIVED.inc();
        aurix_metrics::BYTES_RECEIVED.inc_by(packet.payload.len() as u64);

        // Feed to audio analysis pipeline (STT, content analysis)
        if let Some(ref pipeline) = self.audio_pipeline {
            let ch_hash = packet.header.channel_id_hash;
            for entry in self.channels.iter() {
                if aurix_common::protocol::channel_id_hash(entry.key()) == ch_hash
                    && entry.value().has_participant(&sender_session.user_id)
                {
                    pipeline.process_opus_packet(
                        sender_session.user_id,
                        *entry.key(),
                        &packet.payload,
                    );
                    break;
                }
            }
        }

        let ch_hash = packet.header.channel_id_hash;
        for channel_entry in self.channels.iter() {
            let channel = channel_entry.value();
            let this_hash = aurix_common::protocol::channel_id_hash(&channel.channel_id);
            if this_hash != ch_hash { continue; }
            if !channel.has_participant(&sender_session.user_id) { continue; }

            self.route_to_channel_receivers(&channel.channel_id, &sender_session, packet).await;

            // ── Cascade to remote nodes ──
            if let Some(ref cascade) = self.cascade {
                cascade.forward_to_peers(&channel.channel_id, packet).await;
            }
        }
        Ok(())
    }

    /// Common routing: forward `packet` to all eligible receivers in `channel_id`.
    async fn route_to_channel_receivers(
        &self,
        channel_id: &ChannelId,
        _sender_session: &Arc<MediaSession>,
        packet: &AurixPacket,
    ) {
        let channel = match self.channels.get(channel_id) {
            Some(ch) => ch.value().clone(),
            None => return,
        };
        let receivers = channel.get_receivers_for_audio(packet.header.ssrc);

        for (receiver, volume) in receivers {
            if let Some(addr) = receiver.get_remote_addr() {
                let out_bytes = if packet.header.has_flag(PacketFlags::E2ee) {
                    packet.encode().freeze()
                } else if (volume - 1.0).abs() > 0.01 {
                    Self::encode_with_volume(packet, volume)
                } else {
                    packet.encode().freeze()
                };

                match self.socket.send_to(&out_bytes, addr).await {
                    Ok(n) => {
                        receiver.record_packet_sent(n as u64);
                        aurix_metrics::PACKETS_SENT.inc();
                        aurix_metrics::BYTES_SENT.inc_by(n as u64);
                    }
                    Err(e) => {
                        aurix_metrics::PACKETS_DROPPED.inc();
                        warn!("Failed to send audio to {}: {}", receiver.user_id, e);
                    }
                }
            }
        }
    }

    fn encode_with_volume(packet: &AurixPacket, volume: f32) -> Bytes {
        let vol_byte = (volume.clamp(0.0, 1.0) * 255.0) as u8;
        let new_payload_len = 1 + packet.payload.len();
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + new_payload_len);
        let mut header = packet.header.clone();
        header.flags |= PacketFlags::VolumeAttenuated as u16;
        header.payload_length = new_payload_len as u16;
        let mut combined = BytesMut::with_capacity(new_payload_len);
        combined.put_u8(vol_byte);
        combined.put_slice(&packet.payload);
        header.checksum = crc32fast::hash(&combined);
        header.encode(&mut buf);
        buf.put_slice(&combined);
        buf.freeze()
    }

    /// Route a standard RTP packet from a WebRTC-compatible client
    pub async fn route_rtp_packet(&self, data: &[u8], src_addr: SocketAddr) -> Result<()> {
        let rtp = RtpHeader::parse(data)?;
        let sender_session = self.sessions_by_addr.get(&src_addr)
            .map(|s| s.value().clone())
            .or_else(|| self.sessions_by_ssrc.get(&rtp.ssrc).map(|s| s.value().clone()))
            .ok_or_else(|| AurixError::SessionNotFound("Unknown RTP sender".into()))?;

        if sender_session.get_remote_addr().is_none() {
            sender_session.set_remote_addr(src_addr);
            self.sessions_by_addr.insert(src_addr, sender_session.clone());
        }

        if sender_session.is_muted.load(std::sync::atomic::Ordering::Relaxed)
            || sender_session.is_server_muted.load(std::sync::atomic::Ordering::Relaxed)
        { return Ok(()); }

        sender_session.record_packet_received(data.len() as u64);
        sender_session.is_speaking.store(true, std::sync::atomic::Ordering::Relaxed);
        aurix_metrics::PACKETS_RECEIVED.inc();
        aurix_metrics::BYTES_RECEIVED.inc_by(data.len() as u64);

        let sender_channels = sender_session.get_channels();
        for ch_id in &sender_channels {
            if let Some(channel) = self.channels.get(ch_id) {
                let receivers = channel.get_receivers_for_audio(rtp.ssrc);
                for (receiver, _volume) in receivers {
                    if let Some(addr) = receiver.get_remote_addr() {
                        match self.socket.send_to(data, addr).await {
                            Ok(n) => {
                                receiver.record_packet_sent(n as u64);
                                aurix_metrics::PACKETS_SENT.inc();
                                aurix_metrics::BYTES_SENT.inc_by(n as u64);
                            }
                            Err(e) => {
                                aurix_metrics::PACKETS_DROPPED.inc();
                                warn!("Failed to forward RTP to {}: {}", receiver.user_id, e);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_heartbeat(&self, packet: &AurixPacket, src_addr: SocketAddr) -> Result<()> {
        if let Some(session) = self.sessions_by_addr.get(&src_addr) {
            session.update_heartbeat();
            let ack = AurixPacket { header: PacketHeader::new(PacketType::HeartbeatAck, 0, packet.header.timestamp, packet.header.ssrc), payload: Bytes::new() };
            let _ = self.socket.send_to(&ack.encode(), src_addr).await;
        }
        Ok(())
    }

    async fn handle_quality_report(&self, packet: &AurixPacket, src_addr: SocketAddr) -> Result<()> {
        if let Some(session) = self.sessions_by_addr.get(&src_addr) {
            if packet.payload.len() >= 12 {
                let rtt = f32::from_be_bytes([packet.payload[0], packet.payload[1], packet.payload[2], packet.payload[3]]);
                let jitter = f32::from_be_bytes([packet.payload[4], packet.payload[5], packet.payload[6], packet.payload[7]]);
                let loss = f32::from_be_bytes([packet.payload[8], packet.payload[9], packet.payload[10], packet.payload[11]]);
                let mut metrics = QualityMetrics { rtt_ms: rtt, jitter_ms: jitter, packet_loss_percent: loss, bitrate_kbps: 0, mos_score: 0.0 };
                metrics.mos_score = metrics.calculate_mos();
                aurix_metrics::RTT_MS.observe(rtt as f64);
                aurix_metrics::JITTER_MS.observe(jitter as f64);
                aurix_metrics::PACKET_LOSS_RATE.set(loss as f64);
                session.update_quality(metrics);
            }
        }
        Ok(())
    }

    async fn handle_mute_state(&self, packet: &AurixPacket, src_addr: SocketAddr) -> Result<()> {
        if let Some(session) = self.sessions_by_addr.get(&src_addr) {
            if !packet.payload.is_empty() {
                session.is_muted.store(packet.payload[0] != 0, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(())
    }

    async fn handle_speaking_state(&self, packet: &AurixPacket, src_addr: SocketAddr) -> Result<()> {
        if let Some(session) = self.sessions_by_addr.get(&src_addr) {
            if !packet.payload.is_empty() {
                session.is_speaking.store(packet.payload[0] != 0, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(())
    }
}