//! Native AURX v2 media transport: signed `SessionBind`, AES-CTR + HMAC on every packet,
//! per-sender replay windows, heartbeats with RTT measurement — over one of two links:
//!
//! * **UDP** (default): one socket per session; the receive loop runs on a dedicated OS thread
//!   (no runtime hop between the socket and the mixer).
//! * **Tunnel**: the same sealed packets as binary frames on the control WebSocket, for
//!   networks that block UDP. Uplink packets are queued to the control task (bounded, drop on
//!   overflow), downlink frames are handed in by the control task through
//!   [`MediaTransport::handle_frame`]. TCP head-of-line blocking applies.
//!
//! Both links share the uplink sequence counter so a mid-session switch keeps the server's
//! replay window happy.

use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::{
    AurixPacket, PacketFlags, PacketHeader, PacketType, ReplayWindow, MAX_PACKET_SIZE,
};
use aurix_common::types::{AudioCodec, Direction, SessionId};
use bytes::Bytes;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::error::ClientError;

/// Which link carries the session's media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaPath {
    /// Native AURX over its own UDP socket.
    Udp,
    /// Native AURX as binary frames on the control WebSocket (UDP blocked).
    Tunnel,
}

/// Link selection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaPathPolicy {
    /// UDP first; fall back to the tunnel when the UDP bind fails or heartbeats stop being
    /// acknowledged, and re-probe UDP periodically while tunnelled.
    #[default]
    Auto,
    /// UDP only; a blocked UDP path fails the connection as before.
    UdpOnly,
    /// Tunnel only (testing, or hosts known to block UDP).
    TunnelOnly,
}

/// Shared uplink sequence counter: one per session, handed to every link the session uses.
pub type SequenceCounter = Arc<AtomicU32>;

/// Downlink frames the control task hands to a tunnelled transport are classified so the
/// bind handshake can wait for its ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    BindAck,
    Audio,
    HeartbeatAck,
    Other,
    Rejected,
}

/// One verified downlink audio frame.
#[derive(Debug, Clone)]
pub struct IncomingAudio {
    pub sender_ssrc: u32,
    pub sequence: u32,
    pub timestamp: u32,
    pub channel_hash: u32,
    /// Server-applied gain (positional attenuation × participant volume × focus), `1.0` = as sent.
    pub volume: f32,
    /// Speaker bearing relative to this listener in directional channels.
    pub direction: Option<Direction>,
    /// Codec of `payload`: Opus, or PCMU when the packet carries `PacketFlags::Pcmu`.
    pub codec: AudioCodec,
    /// Server-mixed channel downlink (`PacketFlags::Mixed`): stereo Opus with every
    /// receiver-specific gain already applied; `sender_ssrc` is the channel's mix SSRC.
    pub mixed: bool,
    /// `PacketFlags::E2ee`: `payload` is an `aurix_common::e2ee` frame sealed by the sender,
    /// opaque to the server; open it with the sender's key before decoding.
    pub e2ee: bool,
    pub payload: Bytes,
}

/// Counters of the media path since bind.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MediaStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub audio_frames_received: u64,
    /// Packets that failed authentication or decryption.
    pub bad_auth: u64,
    /// Packets rejected by a replay window.
    pub replayed: u64,
    /// Uplink packets dropped because the tunnel queue to the control task was full (0 on UDP).
    pub uplink_dropped: u64,
    /// Last heartbeat round-trip in milliseconds (0 until the first ack).
    pub rtt_ms: f32,
    /// Round-trip extremes and running average over the whole session (0 until the first ack).
    pub rtt_min_ms: f32,
    pub rtt_max_ms: f32,
    pub rtt_avg_ms: f32,
    pub rtt_samples: u64,
    /// Heartbeats sent without an ack arriving before the next one.
    pub heartbeats_lost: u64,
    /// Heartbeats lost in a row; reset by every ack.
    pub heartbeats_lost_consecutive: u32,
}

impl MediaStats {
    pub fn record_rtt(&mut self, rtt_ms: f32) {
        self.rtt_ms = rtt_ms;
        if self.rtt_samples == 0 {
            self.rtt_min_ms = rtt_ms;
            self.rtt_max_ms = rtt_ms;
            self.rtt_avg_ms = rtt_ms;
        } else {
            self.rtt_min_ms = self.rtt_min_ms.min(rtt_ms);
            self.rtt_max_ms = self.rtt_max_ms.max(rtt_ms);
            self.rtt_avg_ms += (rtt_ms - self.rtt_avg_ms) / (self.rtt_samples + 1) as f32;
        }
        self.rtt_samples += 1;
    }
}

/// Resolve `host:port` from `SessionInitAck.media_addr`; an unspecified host (`0.0.0.0`,
/// `::`) means "same host as the control connection".
pub fn resolve_media_addr(
    media_addr: &str,
    fallback_host: &str,
) -> Result<SocketAddr, ClientError> {
    resolve_media_addr_all(media_addr, fallback_host)?
        .into_iter()
        .next()
        .ok_or_else(|| ClientError::Transport(format!("no address for `{media_addr}`")))
}

/// Every socket address one `host:port` endpoint resolves to, IPv4 first.
fn resolve_media_addr_all(
    media_addr: &str,
    fallback_host: &str,
) -> Result<Vec<SocketAddr>, ClientError> {
    let (host, port) = split_host_port(media_addr)
        .ok_or_else(|| ClientError::Transport(format!("invalid media_addr `{media_addr}`")))?;
    let host = if host.is_empty() || host == "0.0.0.0" || host == "::" {
        fallback_host
    } else {
        host
    };
    let mut addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| ClientError::Transport(format!("resolve {host}: {e}")))?
        .collect();
    addrs.sort_by_key(|a| !a.is_ipv4());
    Ok(addrs)
}

/// UDP candidates of a session in bind order: every endpoint of `media_addrs` (the node's
/// IPv4 and IPv6 addresses) or, from an older node, just `media_addr`; duplicates removed,
/// order preserved (IPv4 first as advertised). Fails only when no endpoint resolves at all.
pub fn resolve_media_candidates(
    media_addr: &str,
    media_addrs: &[String],
    fallback_host: &str,
) -> Result<Vec<SocketAddr>, ClientError> {
    let endpoints: Vec<&str> = if media_addrs.is_empty() {
        vec![media_addr]
    } else {
        media_addrs.iter().map(String::as_str).collect()
    };
    let mut out: Vec<SocketAddr> = Vec::new();
    let mut last_err = None;
    for ep in endpoints {
        match resolve_media_addr_all(ep, fallback_host) {
            Ok(addrs) => {
                for a in addrs {
                    if !out.contains(&a) {
                        out.push(a);
                    }
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    if out.is_empty() {
        return Err(last_err
            .unwrap_or_else(|| ClientError::Transport("no media endpoint advertised".into())));
    }
    Ok(out)
}

fn split_host_port(s: &str) -> Option<(&str, u16)> {
    let colon = s.rfind(':')?;
    let host = s[..colon].trim_start_matches('[').trim_end_matches(']');
    let port = s[colon + 1..].parse().ok()?;
    Some((host, port))
}

/// How many uplink packets may wait for the control task before the tunnel drops them
/// (~1.3 s of audio at 20 ms frames).
pub const TUNNEL_UPLINK_QUEUE: usize = 64;

enum Link {
    Udp {
        socket: UdpSocket,
        server: SocketAddr,
    },
    Tunnel {
        uplink: tokio::sync::mpsc::Sender<Vec<u8>>,
    },
}

type Sink = Arc<dyn Fn(IncomingAudio) + Send + Sync>;

pub struct MediaTransport {
    link: Link,
    session_id: SessionId,
    ssrc: u32,
    keys: MediaKeys,
    sequence: SequenceCounter,
    bound: AtomicBool,
    stop: AtomicBool,
    stats: Mutex<MediaStats>,
    replay: Mutex<HashMap<u32, ReplayWindow>>,
    heartbeat_sent: AtomicU64,
    heartbeat_acked: AtomicBool,
    /// Header timestamp and send instant of the heartbeat awaiting its ack.
    heartbeat_pending: Mutex<Option<(u32, Instant)>>,
    started: Instant,
    sink: Mutex<Option<Sink>>,
    recv_thread: Mutex<Option<JoinHandle<()>>>,
}

impl MediaTransport {
    /// Open a UDP socket and authenticate it with the server (`SessionBind` →
    /// `SessionBindAck`), retrying `attempts` times with `timeout` each. Blocking; run it off
    /// the game thread.
    pub fn bind_udp(
        server: SocketAddr,
        session_id: SessionId,
        ssrc: u32,
        media_key: &[u8],
        sequence: SequenceCounter,
        attempts: u32,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let local: SocketAddr = if server.is_ipv4() {
            ([0, 0, 0, 0], 0).into()
        } else {
            ([0u16; 8], 0).into()
        };
        let socket = UdpSocket::bind(local).map_err(|e| ClientError::Transport(e.to_string()))?;
        socket
            .connect(server)
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let keys = MediaKeys::derive(media_key);
        let nonce: u64 = rand::random();
        let bind = AurixPacket::session_bind(
            &session_id,
            ssrc,
            chrono::Utc::now().timestamp_millis(),
            nonce,
        );
        let wire = bind.encode_authenticated(&keys);
        let mut buf = vec![0u8; 2048];
        let mut bound = false;
        'attempts: for _ in 0..attempts.max(1) {
            socket
                .send(&wire)
                .map_err(|e| ClientError::Transport(e.to_string()))?;
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                match socket.recv(&mut buf) {
                    Ok(n) => {
                        if let Ok(mut p) = AurixPacket::decode(&buf[..n]) {
                            if p.header.packet_type == PacketType::SessionBindAck
                                && p.header.ssrc == ssrc
                                && p.open(&keys)
                            {
                                bound = true;
                                break 'attempts;
                            }
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break
                    }
                    Err(e) => return Err(ClientError::Transport(e.to_string())),
                }
            }
        }
        if !bound {
            return Err(ClientError::Transport(
                "no SessionBindAck from media server".into(),
            ));
        }
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let me = Self::new(
            Link::Udp { socket, server },
            session_id,
            ssrc,
            keys,
            sequence,
        );
        me.bound.store(true, Ordering::Relaxed);
        Ok(me)
    }

    /// A transport whose packets travel as binary frames on the control WebSocket. Unbound
    /// until the control task has sent [`Self::bind_packet`] and fed the `SessionBindAck`
    /// back through [`Self::handle_frame`].
    pub fn tunnel(
        session_id: SessionId,
        ssrc: u32,
        media_key: &[u8],
        sequence: SequenceCounter,
        uplink: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> Self {
        Self::new(
            Link::Tunnel { uplink },
            session_id,
            ssrc,
            MediaKeys::derive(media_key),
            sequence,
        )
    }

    fn new(
        link: Link,
        session_id: SessionId,
        ssrc: u32,
        keys: MediaKeys,
        sequence: SequenceCounter,
    ) -> Self {
        Self {
            link,
            session_id,
            ssrc,
            keys,
            sequence,
            bound: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            stats: Mutex::new(MediaStats::default()),
            replay: Mutex::new(HashMap::new()),
            heartbeat_sent: AtomicU64::new(0),
            heartbeat_acked: AtomicBool::new(true),
            heartbeat_pending: Mutex::new(None),
            started: Instant::now(),
            sink: Mutex::new(None),
            recv_thread: Mutex::new(None),
        }
    }

    pub fn path(&self) -> MediaPath {
        match self.link {
            Link::Udp { .. } => MediaPath::Udp,
            Link::Tunnel { .. } => MediaPath::Tunnel,
        }
    }

    /// `SessionBind` acknowledged (immediately true for a UDP transport).
    pub fn is_bound(&self) -> bool {
        self.bound.load(Ordering::Relaxed)
    }

    /// Signed `SessionBind` for the tunnel handshake (a fresh nonce per call).
    pub fn bind_packet(&self) -> Vec<u8> {
        AurixPacket::session_bind(
            &self.session_id,
            self.ssrc,
            chrono::Utc::now().timestamp_millis(),
            rand::random(),
        )
        .encode_authenticated(&self.keys)
        .to_vec()
    }

    /// Server address of the UDP link.
    pub fn server(&self) -> Option<SocketAddr> {
        match &self.link {
            Link::Udp { server, .. } => Some(*server),
            Link::Tunnel { .. } => None,
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.link {
            Link::Udp { socket, .. } => socket.local_addr().ok(),
            Link::Tunnel { .. } => None,
        }
    }

    /// Sequence the next uplink packet will use; carried over a resume so the server's replay
    /// window keeps accepting us.
    pub fn next_sequence(&self) -> u32 {
        self.sequence.load(Ordering::Relaxed)
    }

    /// The counter shared with the other link of this session.
    pub fn sequence_counter(&self) -> SequenceCounter {
        Arc::clone(&self.sequence)
    }

    pub fn stats(&self) -> MediaStats {
        *self.stats.lock()
    }

    /// Start delivering authenticated audio frames to `sink`: spawns the receive thread on
    /// UDP; on a tunnel the control task feeds frames through [`Self::handle_frame`].
    pub fn start(self: &Arc<Self>, sink: Sink) {
        *self.sink.lock() = Some(Arc::clone(&sink));
        if !matches!(self.link, Link::Udp { .. }) {
            return;
        }
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("aurix-media-rx".into())
            .spawn(move || me.recv_loop(sink))
            .expect("spawn media receive thread");
        *self.recv_thread.lock() = Some(handle);
    }

    fn recv_loop(&self, sink: Sink) {
        let Link::Udp { socket, .. } = &self.link else {
            return;
        };
        let mut buf = vec![0u8; MAX_PACKET_SIZE * 2];
        while !self.stop.load(Ordering::Relaxed) {
            let n = match socket.recv(&mut buf) {
                Ok(n) => n,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };
            self.process(&buf[..n], Some(&sink));
        }
    }

    /// Feed one binary WebSocket frame to a tunnelled transport. Ignored after [`Self::stop`].
    pub fn handle_frame(&self, data: &[u8]) -> FrameKind {
        if self.stop.load(Ordering::Relaxed) {
            return FrameKind::Rejected;
        }
        let sink = self.sink.lock().clone();
        self.process(data, sink.as_ref())
    }

    fn process(&self, data: &[u8], sink: Option<&Sink>) -> FrameKind {
        {
            let mut s = self.stats.lock();
            s.packets_received += 1;
            s.bytes_received += data.len() as u64;
        }
        let Ok(mut packet) = AurixPacket::decode(data) else {
            self.stats.lock().bad_auth += 1;
            return FrameKind::Rejected;
        };
        if !packet.open(&self.keys) {
            self.stats.lock().bad_auth += 1;
            return FrameKind::Rejected;
        }
        match packet.header.packet_type {
            PacketType::Audio | PacketType::AudioFec => {
                let sender = packet.header.ssrc;
                let seq = packet.header.sequence;
                if !self
                    .replay
                    .lock()
                    .entry(sender)
                    .or_default()
                    .check_and_update(seq)
                {
                    self.stats.lock().replayed += 1;
                    return FrameKind::Rejected;
                }
                let (volume, direction) = packet.take_downlink_meta();
                self.stats.lock().audio_frames_received += 1;
                let codec = if packet.header.has_flag(PacketFlags::Pcmu) {
                    AudioCodec::Pcmu
                } else {
                    AudioCodec::Opus
                };
                if let Some(sink) = sink {
                    sink(IncomingAudio {
                        sender_ssrc: sender,
                        sequence: seq,
                        timestamp: packet.header.timestamp,
                        channel_hash: packet.header.channel_id_hash,
                        volume,
                        direction,
                        codec,
                        mixed: packet.header.has_flag(PacketFlags::Mixed),
                        e2ee: packet.header.has_flag(PacketFlags::E2ee),
                        payload: packet.payload,
                    });
                }
                FrameKind::Audio
            }
            PacketType::HeartbeatAck => {
                let pending = *self.heartbeat_pending.lock();
                if let Some((ts, sent_at)) = pending {
                    if ts == packet.header.timestamp {
                        let mut s = self.stats.lock();
                        s.record_rtt(sent_at.elapsed().as_secs_f32() * 1000.0);
                        s.heartbeats_lost_consecutive = 0;
                    }
                }
                self.heartbeat_acked.store(true, Ordering::Relaxed);
                FrameKind::HeartbeatAck
            }
            PacketType::SessionBindAck => {
                if packet.header.ssrc == self.ssrc {
                    self.bound.store(true, Ordering::Relaxed);
                    FrameKind::BindAck
                } else {
                    FrameKind::Rejected
                }
            }
            _ => FrameKind::Other,
        }
    }

    fn send_sealed(&self, packet: &AurixPacket) {
        self.send_wire(packet.seal(&self.keys).to_vec());
    }

    /// Re-send the signed `SessionBind` on the current link (re-claims the server endpoint
    /// after a probe on the other link may have moved it). The ack is observed via
    /// [`Self::handle_frame`] / the receive thread.
    pub fn send_bind(&self) {
        self.send_wire(self.bind_packet());
    }

    fn send_wire(&self, wire: Vec<u8>) {
        let len = wire.len() as u64;
        let sent = match &self.link {
            Link::Udp { socket, .. } => socket.send(&wire).is_ok(),
            Link::Tunnel { uplink } => {
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                match uplink.try_send(wire) {
                    Ok(()) => true,
                    Err(_) => {
                        self.stats.lock().uplink_dropped += 1;
                        false
                    }
                }
            }
        };
        if sent {
            let mut s = self.stats.lock();
            s.packets_sent += 1;
            s.bytes_sent += len;
        }
    }

    fn next_seq(&self) -> u32 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    /// Send one Opus frame to `channel_hash`. `level` is the frame's measured wire level.
    pub fn send_audio(&self, channel_hash: u32, timestamp: u32, level: Option<u8>, opus: &[u8]) {
        self.send_audio_frame(channel_hash, timestamp, level, AudioCodec::Opus, opus);
    }

    /// [`Self::send_audio`] for a frame in either codec; PCMU frames are flagged
    /// `PacketFlags::Pcmu` so the server transcodes them.
    pub fn send_audio_frame(
        &self,
        channel_hash: u32,
        timestamp: u32,
        level: Option<u8>,
        codec: AudioCodec,
        data: &[u8],
    ) {
        self.send_audio_packet(channel_hash, timestamp, level, codec, false, data);
    }

    /// [`Self::send_audio`] for an Opus frame already sealed with the group sender key
    /// (`PacketFlags::E2ee`): the server relays it opaque to the channel's capable members.
    pub fn send_audio_e2ee(
        &self,
        channel_hash: u32,
        timestamp: u32,
        level: Option<u8>,
        frame: &[u8],
    ) {
        self.send_audio_packet(
            channel_hash,
            timestamp,
            level,
            AudioCodec::Opus,
            true,
            frame,
        );
    }

    fn send_audio_packet(
        &self,
        channel_hash: u32,
        timestamp: u32,
        level: Option<u8>,
        codec: AudioCodec,
        e2ee: bool,
        data: &[u8],
    ) {
        if data.is_empty() || data.len() > MAX_PACKET_SIZE - 64 {
            return;
        }
        let seq = self.next_seq();
        let mut packet = match level {
            Some(level) => {
                AurixPacket::audio_with_level(seq, timestamp, self.ssrc, channel_hash, level, data)
            }
            None => AurixPacket::audio(
                seq,
                timestamp,
                self.ssrc,
                channel_hash,
                Bytes::copy_from_slice(data),
            ),
        };
        if codec == AudioCodec::Pcmu {
            packet.header.flags |= PacketFlags::Pcmu as u16;
        }
        if e2ee {
            packet.header.flags |= PacketFlags::E2ee as u16;
        }
        self.send_sealed(&packet);
    }

    pub fn send_mute_state(&self, muted: bool) {
        let packet = AurixPacket::new(
            PacketHeader::new(PacketType::MuteState, self.next_seq(), 0, self.ssrc),
            Bytes::from(vec![u8::from(muted)]),
        );
        self.send_sealed(&packet);
    }

    pub fn send_quality_report(&self, rtt_ms: f32, jitter_ms: f32, loss_percent: f32) {
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&rtt_ms.to_be_bytes());
        payload.extend_from_slice(&jitter_ms.to_be_bytes());
        payload.extend_from_slice(&loss_percent.to_be_bytes());
        let packet = AurixPacket::new(
            PacketHeader::new(PacketType::QualityReport, self.next_seq(), 0, self.ssrc),
            Bytes::from(payload),
        );
        self.send_sealed(&packet);
    }

    /// Send a heartbeat (keeps the NAT binding and the server's liveness timer alive).
    pub fn send_heartbeat(&self) {
        if !self.heartbeat_acked.swap(false, Ordering::Relaxed)
            && self.heartbeat_sent.load(Ordering::Relaxed) > 0
        {
            let mut s = self.stats.lock();
            s.heartbeats_lost += 1;
            s.heartbeats_lost_consecutive += 1;
        }
        self.heartbeat_sent.fetch_add(1, Ordering::Relaxed);
        let ts = self.started.elapsed().as_millis() as u32;
        *self.heartbeat_pending.lock() = Some((ts, Instant::now()));
        let mut packet = AurixPacket::heartbeat(self.ssrc, ts);
        packet.header.sequence = self.next_seq();
        self.send_sealed(&packet);
    }

    /// Heartbeats unanswered in a row (the UDP-path health signal behind the tunnel fallback).
    pub fn consecutive_heartbeats_lost(&self) -> u32 {
        self.stats.lock().heartbeats_lost_consecutive
    }

    /// Drop replay state for a sender that left (its SSRC may be reused later).
    pub fn forget_sender(&self, ssrc: u32) {
        self.replay.lock().remove(&ssrc);
    }

    /// Stop receiving: joins the UDP thread; a tunnelled transport ignores further frames and
    /// stops queueing uplink packets.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.bound.store(false, Ordering::Relaxed);
        if let Some(h) = self.recv_thread.lock().take() {
            if std::thread::current().id() != h.thread().id() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for MediaTransport {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurix_common::protocol::channel_id_hash;
    use aurix_common::types::ChannelId;

    #[test]
    fn rtt_min_avg_max_follow_samples() {
        let mut s = MediaStats::default();
        s.record_rtt(40.0);
        assert_eq!(
            (s.rtt_min_ms, s.rtt_avg_ms, s.rtt_max_ms),
            (40.0, 40.0, 40.0)
        );
        s.record_rtt(20.0);
        s.record_rtt(60.0);
        assert_eq!(s.rtt_ms, 60.0);
        assert_eq!(s.rtt_min_ms, 20.0);
        assert_eq!(s.rtt_max_ms, 60.0);
        assert!((s.rtt_avg_ms - 40.0).abs() < 1e-4, "{}", s.rtt_avg_ms);
        assert_eq!(s.rtt_samples, 3);
    }

    /// Minimal fake server: answers `SessionBind`, echoes audio back sealed for the client,
    /// answers heartbeats.
    fn fake_server(
        key: [u8; 32],
        ssrc: u32,
    ) -> (SocketAddr, std::thread::JoinHandle<Vec<AurixPacket>>) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(1500)))
            .unwrap();
        let handle = std::thread::spawn(move || {
            let keys = MediaKeys::derive(&key);
            let mut seen = Vec::new();
            let mut buf = [0u8; 2048];
            let mut down_seq = 100u32;
            while let Ok((n, from)) = socket.recv_from(&mut buf) {
                let mut p = AurixPacket::decode(&buf[..n]).unwrap();
                match p.header.packet_type {
                    PacketType::SessionBind => {
                        assert!(p.verify_auth(&keys));
                        let ack = AurixPacket::session_bind_ack(ssrc, p.header.sequence, 0);
                        socket.send_to(&ack.seal(&keys), from).unwrap();
                    }
                    PacketType::Heartbeat => {
                        assert!(p.open(&keys));
                        let ack = AurixPacket::new(
                            PacketHeader::new(
                                PacketType::HeartbeatAck,
                                down_seq,
                                p.header.timestamp,
                                ssrc,
                            ),
                            Bytes::new(),
                        );
                        down_seq += 1;
                        socket.send_to(&ack.seal(&keys), from).unwrap();
                    }
                    PacketType::Audio => {
                        assert!(p.open(&keys));
                        p.take_audio_level();
                        // Echo as another sender with a volume byte, twice (replay).
                        let mut hdr = p.header.clone();
                        hdr.ssrc = 0x1234;
                        hdr.sequence = down_seq;
                        down_seq += 1;
                        let echo = AurixPacket::new(hdr, p.payload.clone());
                        let (h, body) = echo.downlink_parts(0.5, None);
                        let wire = AurixPacket::seal_parts(&h, &body, &keys);
                        socket.send_to(&wire, from).unwrap();
                        socket.send_to(&wire, from).unwrap();
                        seen.push(p);
                    }
                    PacketType::MuteState | PacketType::QualityReport => {
                        assert!(p.open(&keys));
                        seen.push(p);
                    }
                    _ => {}
                }
            }
            seen
        });
        (addr, handle)
    }

    #[test]
    fn binds_sends_and_receives_authenticated_media() {
        let key = [7u8; 32];
        let ssrc = 0x0badcafe;
        let (addr, server) = fake_server(key, ssrc);
        let transport = Arc::new(
            MediaTransport::bind_udp(
                addr,
                SessionId::new(),
                ssrc,
                &key,
                Arc::new(AtomicU32::new(10)),
                3,
                Duration::from_millis(500),
            )
            .unwrap(),
        );
        assert_eq!(transport.path(), MediaPath::Udp);
        assert!(transport.is_bound());
        let received = Arc::new(Mutex::new(Vec::<IncomingAudio>::new()));
        let sink = {
            let received = received.clone();
            Arc::new(move |a: IncomingAudio| received.lock().push(a))
        };
        transport.start(sink);
        let ch = channel_id_hash(&ChannelId::new());
        for i in 0..5u32 {
            transport.send_audio(ch, i * 960, Some(20), &[1, 2, 3, 4]);
        }
        transport.send_mute_state(true);
        transport.send_quality_report(12.0, 3.0, 0.5);
        transport.send_heartbeat();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && (received.lock().len() < 5
                || transport.stats().rtt_ms == 0.0 && transport.stats().packets_received < 11)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        transport.stop();
        let got = received.lock();
        assert_eq!(got.len(), 5, "replayed copies must be dropped");
        assert!(got.iter().all(|a| a.sender_ssrc == 0x1234
            && a.channel_hash == ch
            && (a.volume - 0.5).abs() < 0.02
            && a.payload.as_ref() == [1, 2, 3, 4]));
        let stats = transport.stats();
        assert_eq!(stats.replayed, 5);
        assert_eq!(stats.bad_auth, 0);
        assert_eq!(transport.next_sequence(), 18);
        drop(transport);
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 7);
        let mute = seen
            .iter()
            .find(|p| p.header.packet_type == PacketType::MuteState)
            .unwrap();
        assert_eq!(mute.payload.as_ref(), [1]);
    }

    #[test]
    fn bind_fails_without_ack() {
        let dead = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = dead.local_addr().unwrap();
        let err = MediaTransport::bind_udp(
            addr,
            SessionId::new(),
            1,
            &[0u8; 32],
            Arc::new(AtomicU32::new(0)),
            2,
            Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("SessionBindAck"));
    }

    #[test]
    fn tunnel_shares_sequence_binds_via_frames_and_drops_on_full_queue() {
        let key = [9u8; 32];
        let ssrc = 0x5151;
        let keys = MediaKeys::derive(&key);
        let seq = Arc::new(AtomicU32::new(40));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let transport = Arc::new(MediaTransport::tunnel(
            SessionId::new(),
            ssrc,
            &key,
            Arc::clone(&seq),
            tx,
        ));
        assert_eq!(transport.path(), MediaPath::Tunnel);
        assert!(!transport.is_bound());
        assert!(transport.server().is_none());

        // Bind handshake: signed SessionBind out, sealed SessionBindAck in.
        let bind = AurixPacket::decode(&transport.bind_packet()).unwrap();
        assert_eq!(bind.header.packet_type, PacketType::SessionBind);
        assert!(bind.verify_auth(&keys));
        let foreign = AurixPacket::session_bind_ack(ssrc + 1, 0, 0).seal(&keys);
        assert_eq!(transport.handle_frame(&foreign), FrameKind::Rejected);
        assert!(!transport.is_bound());
        let ack = AurixPacket::session_bind_ack(ssrc, 0, 0).seal(&keys);
        assert_eq!(transport.handle_frame(&ack), FrameKind::BindAck);
        assert!(transport.is_bound());

        // Downlink audio reaches the sink once; the replayed copy does not.
        let received = Arc::new(Mutex::new(Vec::<IncomingAudio>::new()));
        let sink = {
            let received = received.clone();
            Arc::new(move |a: IncomingAudio| received.lock().push(a))
        };
        transport.start(sink);
        let audio = AurixPacket::audio(7, 960, 0x1234, 77, Bytes::from_static(&[1, 2, 3]));
        let (h, body) = audio.downlink_parts(0.25, None);
        let wire = AurixPacket::seal_parts(&h, &body, &keys);
        assert_eq!(transport.handle_frame(&wire), FrameKind::Audio);
        assert_eq!(transport.handle_frame(&wire), FrameKind::Rejected);
        assert_eq!(transport.handle_frame(b"garbage"), FrameKind::Rejected);
        assert_eq!(received.lock().len(), 1);
        assert!((received.lock()[0].volume - 0.25).abs() < 0.02);

        // Uplink: packets are queued for the control task, sequence continues from 40.
        transport.send_audio(77, 0, Some(10), &[4, 5]);
        transport.send_heartbeat();
        let first = AurixPacket::decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(first.header.sequence, 40);
        assert_eq!(seq.load(Ordering::Relaxed), 42);
        assert_eq!(transport.next_sequence(), 42);
        // Queue capacity 4: one heartbeat is still queued, three more fit, the rest drop.
        for i in 0..6u32 {
            transport.send_audio(77, i * 960, None, &[1]);
        }
        let s = transport.stats();
        assert_eq!(s.uplink_dropped, 3);
        assert_eq!(s.packets_sent, 5);
        assert_eq!(s.replayed, 1);
        assert_eq!(s.bad_auth, 1);

        transport.stop();
        assert!(!transport.is_bound());
        assert_eq!(transport.handle_frame(&wire), FrameKind::Rejected);
        while rx.try_recv().is_ok() {}
        transport.send_audio(77, 0, None, &[1]);
        assert!(rx.try_recv().is_err(), "stopped transports queue nothing");
    }

    #[test]
    fn resolves_media_addr_with_fallback_host() {
        assert_eq!(
            resolve_media_addr("0.0.0.0:9000", "127.0.0.1").unwrap(),
            "127.0.0.1:9000".parse().unwrap()
        );
        assert_eq!(
            resolve_media_addr("[::1]:9001", "x").unwrap(),
            "[::1]:9001".parse().unwrap()
        );
        assert!(resolve_media_addr("nonsense", "x").is_err());
    }

    #[test]
    fn media_candidates_keep_server_order_and_fall_back_to_the_legacy_field() {
        let both = resolve_media_candidates(
            "203.0.113.7:9000",
            &["203.0.113.7:9000".into(), "[2001:db8::7]:9000".into()],
            "x",
        )
        .unwrap();
        assert_eq!(
            both,
            vec![
                "203.0.113.7:9000".parse::<SocketAddr>().unwrap(),
                "[2001:db8::7]:9000".parse().unwrap()
            ]
        );
        // Older node: only media_addr.
        assert_eq!(
            resolve_media_candidates("[::1]:9001", &[], "x").unwrap(),
            vec!["[::1]:9001".parse::<SocketAddr>().unwrap()]
        );
        // A garbage entry is skipped as long as one candidate resolves; duplicates collapse.
        assert_eq!(
            resolve_media_candidates(
                "127.0.0.1:1",
                &[
                    "nonsense".into(),
                    "127.0.0.1:1".into(),
                    "127.0.0.1:1".into()
                ],
                "x"
            )
            .unwrap()
            .len(),
            1
        );
        assert!(resolve_media_candidates("nonsense", &["nonsense".into()], "x").is_err());
    }
}
