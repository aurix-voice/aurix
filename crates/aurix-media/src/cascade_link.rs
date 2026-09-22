//! Cascade link probing and the TCP fallback transport.
//!
//! Nodes measure each other continuously: every probe interval a node sends a sealed `Ping`
//! envelope to each allowed peer over the cascade UDP socket and records the round-trip time
//! of the `Pong`. When a peer stops answering over UDP (a firewall between two regions, a
//! provider that drops UDP), the node opens a TCP connection to the *same* cascade port and
//! probes there; while TCP answers and UDP does not, that peer's envelopes travel over the
//! TCP link instead. Frames on the TCP link are exactly the sealed envelopes UDP carries,
//! length-prefixed, so confidentiality, authentication and anti-replay do not depend on the
//! transport (no TLS layer is needed on top: the payload is already encrypted and tagged with
//! keys derived from `media.cascade_secret`). The first frame of a TCP connection is a
//! `Hello` naming the connecting node's own cascade address; it must be an allowed peer and
//! its IP must match the connection's remote IP, after which frames on that connection are
//! attributed to that peer (same replay window as its UDP traffic).
//!
//! The measured links (transport + RTT) are published to the node registry by
//! `CascadeTopology`, so hubs are elected by real latency and the inter-regional relay tree
//! avoids links that are blocked.

use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{AurixPacket, PacketHeader, PacketType, MAX_RELAY_PACKET_SIZE};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Length prefix of a TCP frame (big-endian, bytes of the sealed envelope that follows).
pub const FRAME_LEN_BYTES: usize = 2;
/// Largest frame accepted on a TCP link (a sealed relay envelope).
pub const MAX_FRAME_BYTES: usize = MAX_RELAY_PACKET_SIZE;

/// Which transport currently carries envelopes to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LinkTransport {
    Udp,
    Tcp,
}

impl LinkTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

/// What this node measured towards one peer, as published to the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkReport {
    pub peer: SocketAddr,
    /// `None` when the peer answered on neither transport recently (unconfirmed link).
    pub transport: Option<LinkTransport>,
    /// Smoothed round-trip time of the transport above.
    pub rtt_ms: Option<u32>,
}

/// Node-to-node control envelopes: the inner packet is a `Heartbeat` whose payload starts
/// with a kind byte, so a node that only knows audio envelopes rejects it harmlessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeControl {
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// First frame of a TCP link: the connecting node's cascade address.
    Hello {
        addr: SocketAddr,
    },
}

const KIND_PING: u8 = 1;
const KIND_PONG: u8 = 2;
const KIND_HELLO: u8 = 3;

impl CascadeControl {
    pub fn to_packet(&self) -> AurixPacket {
        let mut p = BytesMut::with_capacity(24);
        match self {
            Self::Ping { nonce } => {
                p.put_u8(KIND_PING);
                p.put_u64(*nonce);
            }
            Self::Pong { nonce } => {
                p.put_u8(KIND_PONG);
                p.put_u64(*nonce);
            }
            Self::Hello { addr } => {
                p.put_u8(KIND_HELLO);
                match addr.ip() {
                    IpAddr::V4(ip) => {
                        p.put_u8(4);
                        p.put_slice(&ip.octets());
                    }
                    IpAddr::V6(ip) => {
                        p.put_u8(6);
                        p.put_slice(&ip.octets());
                    }
                }
                p.put_u16(addr.port());
            }
        }
        AurixPacket::new(
            PacketHeader::new(PacketType::Heartbeat, 0, 0, 0),
            p.freeze(),
        )
    }

    /// Parse the inner packet of an opened envelope; `None` when it is not a control packet
    /// (i.e. it is audio).
    pub fn from_packet(pkt: &AurixPacket) -> Result<Option<Self>> {
        if pkt.header.packet_type != PacketType::Heartbeat {
            return Ok(None);
        }
        let mut p = pkt.payload.clone();
        if p.remaining() < 1 {
            return Err(AurixError::Transport("Empty cascade control".into()));
        }
        match p.get_u8() {
            KIND_PING | KIND_PONG if p.remaining() != 8 => {
                Err(AurixError::Transport("Malformed cascade probe".into()))
            }
            KIND_PING => Ok(Some(Self::Ping { nonce: p.get_u64() })),
            KIND_PONG => Ok(Some(Self::Pong { nonce: p.get_u64() })),
            KIND_HELLO => {
                if p.remaining() < 1 {
                    return Err(AurixError::Transport("Malformed cascade hello".into()));
                }
                let ip = match p.get_u8() {
                    4 if p.remaining() == 6 => {
                        let mut o = [0u8; 4];
                        p.copy_to_slice(&mut o);
                        IpAddr::V4(Ipv4Addr::from(o))
                    }
                    6 if p.remaining() == 18 => {
                        let mut o = [0u8; 16];
                        p.copy_to_slice(&mut o);
                        IpAddr::V6(Ipv6Addr::from(o))
                    }
                    _ => return Err(AurixError::Transport("Malformed cascade hello".into())),
                };
                let port = p.get_u16();
                Ok(Some(Self::Hello {
                    addr: SocketAddr::new(ip, port),
                }))
            }
            k => Err(AurixError::Transport(format!(
                "Unknown cascade control kind {k}"
            ))),
        }
    }
}

/// Per-peer measurements owned by the relay (one entry per allowed peer).
#[derive(Debug, Clone, Default)]
pub struct LinkState {
    /// Last UDP round trip: smoothed RTT and when the pong arrived.
    pub udp: Option<(u32, Instant)>,
    pub tcp: Option<(u32, Instant)>,
    /// UDP pings sent since the last UDP pong.
    pub udp_unanswered: u32,
    /// Last time a TCP connect was attempted (backoff for peers without a TCP listener).
    pub tcp_attempt: Option<Instant>,
    /// Last time an envelope was sent over TCP (idle TCP links are closed once UDP is back).
    pub tcp_used: Option<Instant>,
}

impl LinkState {
    fn smooth(prev: Option<(u32, Instant)>, sample_ms: u32, now: Instant) -> (u32, Instant) {
        match prev {
            // EWMA (7/8 old, 1/8 new) like TCP's SRTT; a fresh link takes the first sample.
            Some((old, at)) if now.duration_since(at) < Duration::from_secs(10) => {
                ((old * 7 + sample_ms).div_ceil(8), now)
            }
            _ => (sample_ms, now),
        }
    }

    pub fn record_udp(&mut self, sample_ms: u32, now: Instant) {
        self.udp = Some(Self::smooth(self.udp, sample_ms, now));
        self.udp_unanswered = 0;
    }

    pub fn record_tcp(&mut self, sample_ms: u32, now: Instant) {
        self.tcp = Some(Self::smooth(self.tcp, sample_ms, now));
    }

    fn fresh(entry: Option<(u32, Instant)>, now: Instant, max_age: Duration) -> Option<u32> {
        entry
            .filter(|(_, at)| now.duration_since(*at) <= max_age)
            .map(|(rtt, _)| rtt)
    }

    /// The transport envelopes to this peer should use right now: UDP whenever it answered
    /// recently, TCP when only TCP did, and UDP (best effort — a peer that predates probes
    /// still receives audio) when neither did.
    pub fn transport(&self, now: Instant, max_age: Duration) -> LinkTransport {
        if Self::fresh(self.udp, now, max_age).is_some() {
            LinkTransport::Udp
        } else if Self::fresh(self.tcp, now, max_age).is_some() {
            LinkTransport::Tcp
        } else {
            LinkTransport::Udp
        }
    }

    /// The confirmed transport and its RTT, or `None` when nothing answered recently.
    pub fn confirmed(&self, now: Instant, max_age: Duration) -> Option<(LinkTransport, u32)> {
        if let Some(rtt) = Self::fresh(self.udp, now, max_age) {
            Some((LinkTransport::Udp, rtt))
        } else {
            Self::fresh(self.tcp, now, max_age).map(|rtt| (LinkTransport::Tcp, rtt))
        }
    }
}

/// Encode one TCP frame: length prefix + sealed envelope.
pub fn frame(data: &[u8]) -> Bytes {
    debug_assert!(data.len() <= MAX_FRAME_BYTES);
    let mut out = BytesMut::with_capacity(FRAME_LEN_BYTES + data.len());
    out.put_u16(data.len() as u16);
    out.put_slice(data);
    out.freeze()
}

/// Read one frame from a TCP link. `Ok(None)` on a clean EOF before a frame started.
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<usize>> {
    let mut len = [0u8; FRAME_LEN_BYTES];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u16::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cascade frame length {len} out of range"),
        ));
    }
    if buf.len() < len {
        buf.resize(len, 0);
    }
    reader.read_exact(&mut buf[..len]).await?;
    Ok(Some(len))
}

/// Write one frame; a full socket buffer is a backpressure signal handled by the caller's
/// bounded queue, not here.
pub async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
) -> std::io::Result<()> {
    writer.write_all(&frame(data)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trips_and_rejects_garbage() {
        for c in [
            CascadeControl::Ping { nonce: 7 },
            CascadeControl::Pong { nonce: u64::MAX },
            CascadeControl::Hello {
                addr: "10.1.2.3:9001".parse().unwrap(),
            },
            CascadeControl::Hello {
                addr: "[2001:db8::7]:9001".parse().unwrap(),
            },
        ] {
            let pkt = c.to_packet();
            let decoded = AurixPacket::decode(&pkt.encode()).unwrap();
            assert_eq!(CascadeControl::from_packet(&decoded).unwrap(), Some(c));
        }
        let audio = AurixPacket::audio(1, 0, 1, 1, Bytes::from_static(b"x"));
        assert_eq!(CascadeControl::from_packet(&audio).unwrap(), None);
        let bad = AurixPacket::new(
            PacketHeader::new(PacketType::Heartbeat, 0, 0, 0),
            Bytes::from_static(&[KIND_PING, 1, 2]),
        );
        assert!(CascadeControl::from_packet(&bad).is_err());
        let bad = AurixPacket::new(
            PacketHeader::new(PacketType::Heartbeat, 0, 0, 0),
            Bytes::from_static(&[9]),
        );
        assert!(CascadeControl::from_packet(&bad).is_err());
    }

    #[test]
    fn link_state_prefers_fresh_udp_then_tcp() {
        let mut s = LinkState::default();
        let t0 = Instant::now();
        let max_age = Duration::from_secs(3);
        assert_eq!(s.transport(t0, max_age), LinkTransport::Udp);
        assert_eq!(s.confirmed(t0, max_age), None);
        s.record_tcp(40, t0);
        assert_eq!(s.transport(t0, max_age), LinkTransport::Tcp);
        assert_eq!(s.confirmed(t0, max_age), Some((LinkTransport::Tcp, 40)));
        s.record_udp(20, t0 + Duration::from_millis(500));
        assert_eq!(
            s.transport(t0 + Duration::from_secs(1), max_age),
            LinkTransport::Udp
        );
        assert_eq!(s.udp_unanswered, 0);
        // UDP goes quiet while TCP keeps answering: after max_age the TCP measurement is
        // used, and once both expire the state falls back to best-effort UDP.
        s.record_tcp(40, t0 + Duration::from_secs(2));
        assert_eq!(
            s.transport(t0 + Duration::from_millis(3600), max_age),
            LinkTransport::Tcp
        );
        assert_eq!(
            s.transport(t0 + Duration::from_secs(10), max_age),
            LinkTransport::Udp
        );
        assert_eq!(s.confirmed(t0 + Duration::from_secs(10), max_age), None);
        // Smoothing: 7/8 old + 1/8 new.
        s.record_udp(20, t0 + Duration::from_secs(1));
        s.record_udp(100, t0 + Duration::from_secs(2));
        assert_eq!(s.udp.unwrap().0, 30);
    }

    #[tokio::test]
    async fn frames_round_trip_and_bad_lengths_are_rejected() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frame(&mut a, b"hello").await.unwrap();
        write_frame(&mut a, &[7u8; 100]).await.unwrap();
        let mut buf = Vec::new();
        assert_eq!(read_frame(&mut b, &mut buf).await.unwrap(), Some(5));
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(read_frame(&mut b, &mut buf).await.unwrap(), Some(100));
        drop(a);
        assert_eq!(read_frame(&mut b, &mut buf).await.unwrap(), None);

        let (mut a, mut b) = tokio::io::duplex(4096);
        a.write_all(&[0xFF, 0xFF]).await.unwrap();
        assert!(read_frame(&mut b, &mut buf).await.is_err());
        let (mut a, mut b) = tokio::io::duplex(4096);
        a.write_all(&[0, 0]).await.unwrap();
        assert!(read_frame(&mut b, &mut buf).await.is_err());
    }
}
