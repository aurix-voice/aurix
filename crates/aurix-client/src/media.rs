//! Native AURX v2 media transport: one UDP socket per session, signed `SessionBind`, AES-CTR +
//! HMAC on every packet, per-sender replay windows, heartbeats with RTT measurement. The receive
//! loop runs on a dedicated OS thread (no runtime hop between the socket and the mixer).

use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::{
    AurixPacket, PacketHeader, PacketType, ReplayWindow, MAX_PACKET_SIZE,
};
use aurix_common::types::{Direction, SessionId};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::error::ClientError;

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
    pub opus: Bytes,
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
    /// Last heartbeat round-trip in milliseconds (0 until the first ack).
    pub rtt_ms: f32,
    /// Heartbeats sent without an ack arriving before the next one.
    pub heartbeats_lost: u64,
}

/// Resolve `host:port` from `SessionInitAck.media_addr`; an unspecified host (`0.0.0.0`,
/// `::`) means "same host as the control connection".
pub fn resolve_media_addr(
    media_addr: &str,
    fallback_host: &str,
) -> Result<SocketAddr, ClientError> {
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
    addrs
        .into_iter()
        .next()
        .ok_or_else(|| ClientError::Transport(format!("no address for {host}")))
}

fn split_host_port(s: &str) -> Option<(&str, u16)> {
    let colon = s.rfind(':')?;
    let host = s[..colon].trim_start_matches('[').trim_end_matches(']');
    let port = s[colon + 1..].parse().ok()?;
    Some((host, port))
}

pub struct MediaTransport {
    socket: UdpSocket,
    server: SocketAddr,
    ssrc: u32,
    keys: MediaKeys,
    sequence: AtomicU32,
    stop: AtomicBool,
    stats: Mutex<MediaStats>,
    replay: Mutex<HashMap<u32, ReplayWindow>>,
    heartbeat_sent: AtomicU64,
    heartbeat_acked: AtomicBool,
    /// Header timestamp and send instant of the heartbeat awaiting its ack.
    heartbeat_pending: Mutex<Option<(u32, Instant)>>,
    started: Instant,
    recv_thread: Mutex<Option<JoinHandle<()>>>,
}

impl MediaTransport {
    /// Open a socket and authenticate it with the server (`SessionBind` → `SessionBindAck`),
    /// retrying `attempts` times with `timeout` each. Blocking; run it off the game thread.
    pub fn bind(
        server: SocketAddr,
        session_id: SessionId,
        ssrc: u32,
        media_key: &[u8],
        initial_sequence: u32,
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
        Ok(Self {
            socket,
            server,
            ssrc,
            keys,
            sequence: AtomicU32::new(initial_sequence),
            stop: AtomicBool::new(false),
            stats: Mutex::new(MediaStats::default()),
            replay: Mutex::new(HashMap::new()),
            heartbeat_sent: AtomicU64::new(0),
            heartbeat_acked: AtomicBool::new(true),
            heartbeat_pending: Mutex::new(None),
            started: Instant::now(),
            recv_thread: Mutex::new(None),
        })
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }

    /// Sequence the next uplink packet will use; carried over a resume so the server's replay
    /// window keeps accepting us.
    pub fn next_sequence(&self) -> u32 {
        self.sequence.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> MediaStats {
        *self.stats.lock()
    }

    /// Start the receive thread; every authenticated audio frame is handed to `sink`.
    pub fn start(self: &Arc<Self>, sink: Arc<dyn Fn(IncomingAudio) + Send + Sync>) {
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("aurix-media-rx".into())
            .spawn(move || me.recv_loop(sink))
            .expect("spawn media receive thread");
        *self.recv_thread.lock() = Some(handle);
    }

    fn recv_loop(&self, sink: Arc<dyn Fn(IncomingAudio) + Send + Sync>) {
        let mut buf = vec![0u8; MAX_PACKET_SIZE * 2];
        while !self.stop.load(Ordering::Relaxed) {
            let n = match self.socket.recv(&mut buf) {
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
            {
                let mut s = self.stats.lock();
                s.packets_received += 1;
                s.bytes_received += n as u64;
            }
            let Ok(mut packet) = AurixPacket::decode(&buf[..n]) else {
                self.stats.lock().bad_auth += 1;
                continue;
            };
            if !packet.open(&self.keys) {
                self.stats.lock().bad_auth += 1;
                continue;
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
                        continue;
                    }
                    let (volume, direction) = packet.take_downlink_meta();
                    self.stats.lock().audio_frames_received += 1;
                    sink(IncomingAudio {
                        sender_ssrc: sender,
                        sequence: seq,
                        timestamp: packet.header.timestamp,
                        channel_hash: packet.header.channel_id_hash,
                        volume,
                        direction,
                        opus: packet.payload,
                    });
                }
                PacketType::HeartbeatAck => {
                    let pending = *self.heartbeat_pending.lock();
                    if let Some((ts, sent_at)) = pending {
                        if ts == packet.header.timestamp {
                            self.stats.lock().rtt_ms = sent_at.elapsed().as_secs_f32() * 1000.0;
                        }
                    }
                    self.heartbeat_acked.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
    }

    fn send_sealed(&self, packet: &AurixPacket) {
        let wire = packet.seal(&self.keys);
        if self.socket.send(&wire).is_ok() {
            let mut s = self.stats.lock();
            s.packets_sent += 1;
            s.bytes_sent += wire.len() as u64;
        }
    }

    fn next_seq(&self) -> u32 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    /// Send one Opus frame to `channel_hash`. `level` is the frame's measured wire level.
    pub fn send_audio(&self, channel_hash: u32, timestamp: u32, level: Option<u8>, opus: &[u8]) {
        if opus.is_empty() || opus.len() > MAX_PACKET_SIZE - 64 {
            return;
        }
        let seq = self.next_seq();
        let packet = match level {
            Some(level) => {
                AurixPacket::audio_with_level(seq, timestamp, self.ssrc, channel_hash, level, opus)
            }
            None => AurixPacket::audio(
                seq,
                timestamp,
                self.ssrc,
                channel_hash,
                Bytes::copy_from_slice(opus),
            ),
        };
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
            self.stats.lock().heartbeats_lost += 1;
        }
        self.heartbeat_sent.fetch_add(1, Ordering::Relaxed);
        let ts = self.started.elapsed().as_millis() as u32;
        *self.heartbeat_pending.lock() = Some((ts, Instant::now()));
        let mut packet = AurixPacket::heartbeat(self.ssrc, ts);
        packet.header.sequence = self.next_seq();
        self.send_sealed(&packet);
    }

    /// Drop replay state for a sender that left (its SSRC may be reused later).
    pub fn forget_sender(&self, ssrc: u32) {
        self.replay.lock().remove(&ssrc);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
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
            MediaTransport::bind(
                addr,
                SessionId::new(),
                ssrc,
                &key,
                10,
                3,
                Duration::from_millis(500),
            )
            .unwrap(),
        );
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
            && a.opus.as_ref() == [1, 2, 3, 4]));
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
        let err = MediaTransport::bind(
            addr,
            SessionId::new(),
            1,
            &[0u8; 32],
            0,
            2,
            Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("SessionBindAck"));
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
}
