use crate::error::{AurixError, Result};
use crate::types::{
    ChannelId, ChannelRole, Orientation3D, Position3D, ReverbDescriptor, SessionId, UserId,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAGIC_BYTES: [u8; 4] = [0x41, 0x55, 0x52, 0x58];
pub const MAX_PACKET_SIZE: usize = 1400;
pub const HEADER_SIZE: usize = 30;
/// Length of the truncated HMAC-SHA256 authentication tag appended to authenticated packets.
pub const AUTH_TAG_SIZE: usize = 16;
/// Payload layout of `SessionBind`: session_id (16) | unix_ms (8) | nonce (8).
pub const SESSION_BIND_PAYLOAD_SIZE: usize = 32;
/// Maximum clock skew accepted for a `SessionBind` timestamp.
pub const SESSION_BIND_MAX_SKEW_MS: i64 = 30_000;
pub const RTP_VERSION: u8 = 2;
pub const RTP_HEADER_MIN_SIZE: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Audio = 0x01,
    AudioFec = 0x02,
    Control = 0x10,
    Heartbeat = 0x20,
    HeartbeatAck = 0x21,
    SessionInit = 0x30,
    SessionInitAck = 0x31,
    SessionClose = 0x32,
    SessionBind = 0x33,
    SessionBindAck = 0x34,
    ChannelJoin = 0x40,
    ChannelJoinAck = 0x41,
    ChannelLeave = 0x42,
    PositionUpdate = 0x50,
    MuteState = 0x60,
    SpeakingState = 0x61,
    QualityReport = 0x70,
    BitrateCommand = 0x71,
    Error = 0xFF,
}

impl PacketType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::Audio),
            0x02 => Some(Self::AudioFec),
            0x10 => Some(Self::Control),
            0x20 => Some(Self::Heartbeat),
            0x21 => Some(Self::HeartbeatAck),
            0x30 => Some(Self::SessionInit),
            0x31 => Some(Self::SessionInitAck),
            0x32 => Some(Self::SessionClose),
            0x33 => Some(Self::SessionBind),
            0x34 => Some(Self::SessionBindAck),
            0x40 => Some(Self::ChannelJoin),
            0x41 => Some(Self::ChannelJoinAck),
            0x42 => Some(Self::ChannelLeave),
            0x50 => Some(Self::PositionUpdate),
            0x60 => Some(Self::MuteState),
            0x61 => Some(Self::SpeakingState),
            0x70 => Some(Self::QualityReport),
            0x71 => Some(Self::BitrateCommand),
            0xFF => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PacketHeader {
    pub version: u8,
    pub packet_type: PacketType,
    pub flags: u16,
    pub sequence: u32,
    pub timestamp: u32,
    pub ssrc: u32,
    pub channel_id_hash: u32,
    pub payload_length: u16,
    pub checksum: u32,
}

impl PacketHeader {
    pub fn new(packet_type: PacketType, sequence: u32, timestamp: u32, ssrc: u32) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            packet_type,
            flags: 0,
            sequence,
            timestamp,
            ssrc,
            channel_id_hash: 0,
            payload_length: 0,
            checksum: 0,
        }
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_slice(&MAGIC_BYTES);
        buf.put_u8(self.version);
        buf.put_u8(self.packet_type as u8);
        buf.put_u16(self.flags);
        buf.put_u32(self.sequence);
        buf.put_u32(self.timestamp);
        buf.put_u32(self.ssrc);
        buf.put_u32(self.channel_id_hash);
        buf.put_u16(self.payload_length);
        buf.put_u32(self.checksum);
    }

    pub fn decode(buf: &mut Bytes) -> Result<Self> {
        if buf.remaining() < HEADER_SIZE {
            return Err(AurixError::Transport("Packet too short for header".into()));
        }
        let magic = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
        if magic != MAGIC_BYTES {
            return Err(AurixError::Transport("Invalid magic bytes".into()));
        }
        let version = buf.get_u8();
        if version != PROTOCOL_VERSION {
            return Err(AurixError::Transport(format!(
                "Unsupported protocol version: {version}"
            )));
        }
        let ptype_raw = buf.get_u8();
        let packet_type = PacketType::from_u8(ptype_raw)
            .ok_or_else(|| AurixError::Transport(format!("Unknown packet type: {ptype_raw}")))?;
        Ok(Self {
            version,
            packet_type,
            flags: buf.get_u16(),
            sequence: buf.get_u32(),
            timestamp: buf.get_u32(),
            ssrc: buf.get_u32(),
            channel_id_hash: buf.get_u32(),
            payload_length: buf.get_u16(),
            checksum: buf.get_u32(),
        })
    }

    pub fn set_flag(&mut self, flag: PacketFlags) {
        self.flags |= flag as u16;
    }

    pub fn has_flag(&self, flag: PacketFlags) -> bool {
        self.flags & (flag as u16) != 0
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u16)]
pub enum PacketFlags {
    Encrypted = 0x0001,
    Compressed = 0x0002,
    Dtx = 0x0004,
    Fec = 0x0008,
    KeyFrame = 0x0010,
    Priority = 0x0020,
    Relay = 0x0040,
    VolumeAttenuated = 0x0080,
    E2ee = 0x0100,
    Rtp = 0x0200,
    /// Packet carries a trailing `AUTH_TAG_SIZE` HMAC tag over header + payload.
    Authenticated = 0x0400,
}

#[derive(Debug, Clone)]
pub struct AurixPacket {
    pub header: PacketHeader,
    pub payload: Bytes,
    /// Authentication tag received on the wire (present when `PacketFlags::Authenticated` is set).
    pub auth_tag: Option<[u8; AUTH_TAG_SIZE]>,
}

impl AurixPacket {
    pub fn new(header: PacketHeader, payload: Bytes) -> Self {
        Self {
            header,
            payload,
            auth_tag: None,
        }
    }

    fn finalized_header(&self) -> PacketHeader {
        let mut header = self.header.clone();
        header.payload_length = self.payload.len() as u16;
        header.checksum = crc32fast::hash(&self.payload);
        header
    }

    /// Encode without an authentication tag. The `Authenticated` flag is cleared.
    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        let mut header = self.finalized_header();
        header.flags &= !(PacketFlags::Authenticated as u16);
        header.encode(&mut buf);
        buf.put_slice(&self.payload);
        buf
    }

    /// Encode with a trailing HMAC-SHA256 tag (truncated to `AUTH_TAG_SIZE`) computed over the
    /// encoded header and payload using the per-session media key.
    pub fn encode_authenticated(&self, key: &[u8]) -> BytesMut {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len() + AUTH_TAG_SIZE);
        let mut header = self.finalized_header();
        header.flags |= PacketFlags::Authenticated as u16;
        header.encode(&mut buf);
        buf.put_slice(&self.payload);
        let tag = crate::crypto::hmac_sha256(key, &[&buf]);
        buf.put_slice(&tag[..AUTH_TAG_SIZE]);
        buf
    }

    /// Strict decoder: exact length (no trailing bytes), bounded size, verified checksum.
    /// Authentication tags are extracted but NOT verified here — call `verify_auth`.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() > MAX_PACKET_SIZE {
            return Err(AurixError::Transport(
                "Packet exceeds MAX_PACKET_SIZE".into(),
            ));
        }
        let mut bytes = Bytes::copy_from_slice(data);
        let header = PacketHeader::decode(&mut bytes)?;
        let authenticated = header.has_flag(PacketFlags::Authenticated);
        let expected =
            header.payload_length as usize + if authenticated { AUTH_TAG_SIZE } else { 0 };
        if bytes.remaining() < expected {
            return Err(AurixError::Transport("Payload truncated".into()));
        }
        if bytes.remaining() > expected {
            return Err(AurixError::Transport("Trailing bytes after payload".into()));
        }
        let payload = bytes.split_to(header.payload_length as usize);
        let checksum = crc32fast::hash(&payload);
        if checksum != header.checksum {
            return Err(AurixError::Transport("Checksum mismatch".into()));
        }
        let auth_tag = if authenticated {
            let mut tag = [0u8; AUTH_TAG_SIZE];
            tag.copy_from_slice(&bytes[..AUTH_TAG_SIZE]);
            Some(tag)
        } else {
            None
        };
        Ok(Self {
            header,
            payload,
            auth_tag,
        })
    }

    /// Verify the authentication tag against `key`. Returns false for unauthenticated packets.
    pub fn verify_auth(&self, key: &[u8]) -> bool {
        let Some(tag) = self.auth_tag else {
            return false;
        };
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        self.header.encode(&mut buf);
        buf.put_slice(&self.payload);
        let expected = crate::crypto::hmac_sha256(key, &[&buf]);
        crate::crypto::constant_time_eq(&expected[..AUTH_TAG_SIZE], &tag)
    }

    pub fn is_authenticated(&self) -> bool {
        self.auth_tag.is_some()
    }

    pub fn audio(seq: u32, ts: u32, ssrc: u32, ch_hash: u32, data: Bytes) -> Self {
        let mut hdr = PacketHeader::new(PacketType::Audio, seq, ts, ssrc);
        hdr.channel_id_hash = ch_hash;
        Self::new(hdr, data)
    }

    pub fn heartbeat(ssrc: u32, ts: u32) -> Self {
        Self::new(
            PacketHeader::new(PacketType::Heartbeat, 0, ts, ssrc),
            Bytes::new(),
        )
    }

    pub fn bitrate_command(ssrc: u32, target_bitrate: u32) -> Self {
        let payload = target_bitrate.to_be_bytes().to_vec();
        Self::new(
            PacketHeader::new(PacketType::BitrateCommand, 0, 0, ssrc),
            Bytes::from(payload),
        )
    }

    /// Build a `SessionBind` packet. Must be sent with `encode_authenticated(media_key)`.
    pub fn session_bind(session_id: &SessionId, ssrc: u32, unix_ms: i64, nonce: u64) -> Self {
        let mut payload = Vec::with_capacity(SESSION_BIND_PAYLOAD_SIZE);
        payload.extend_from_slice(session_id.0.as_bytes());
        payload.extend_from_slice(&unix_ms.to_be_bytes());
        payload.extend_from_slice(&nonce.to_be_bytes());
        Self::new(
            PacketHeader::new(PacketType::SessionBind, 0, 0, ssrc),
            Bytes::from(payload),
        )
    }

    pub fn session_bind_ack(ssrc: u32, unix_ms: i64) -> Self {
        Self::new(
            PacketHeader::new(PacketType::SessionBindAck, 0, 0, ssrc),
            Bytes::from(unix_ms.to_be_bytes().to_vec()),
        )
    }

    /// Parse a `SessionBind` payload into `(session_id, unix_ms, nonce)`.
    pub fn parse_session_bind(&self) -> Result<(SessionId, i64, u64)> {
        if self.header.packet_type != PacketType::SessionBind {
            return Err(AurixError::Transport("Not a SessionBind packet".into()));
        }
        if self.payload.len() != SESSION_BIND_PAYLOAD_SIZE {
            return Err(AurixError::Transport(
                "Invalid SessionBind payload length".into(),
            ));
        }
        let p = &self.payload;
        let session_id = SessionId(
            uuid::Uuid::from_slice(&p[0..16])
                .map_err(|_| AurixError::Transport("Invalid session id".into()))?,
        );
        let unix_ms = i64::from_be_bytes(p[16..24].try_into().unwrap());
        let nonce = u64::from_be_bytes(p[24..32].try_into().unwrap());
        Ok((session_id, unix_ms, nonce))
    }
}

/// Anti-replay window for 32-bit sequence numbers (RFC 3711 §3.3.2 style, 64-packet window).
#[derive(Debug, Default, Clone)]
pub struct ReplayWindow {
    highest: u32,
    bitmap: u64,
    initialized: bool,
}

impl ReplayWindow {
    pub const WINDOW: u32 = 64;

    /// Returns true and records the sequence if it is fresh; false for replays / too-old packets.
    pub fn check_and_update(&mut self, seq: u32) -> bool {
        if !self.initialized {
            self.initialized = true;
            self.highest = seq;
            self.bitmap = 1;
            return true;
        }
        if seq > self.highest {
            let delta = seq - self.highest;
            if delta >= Self::WINDOW {
                self.bitmap = 1;
            } else {
                self.bitmap = (self.bitmap << delta) | 1;
            }
            self.highest = seq;
            return true;
        }
        let delta = self.highest - seq;
        if delta >= Self::WINDOW {
            return false;
        }
        let mask = 1u64 << delta;
        if self.bitmap & mask != 0 {
            return false;
        }
        self.bitmap |= mask;
        true
    }
}

pub fn channel_id_hash(id: &ChannelId) -> u32 {
    crc32fast::hash(id.0.as_bytes())
}

/// Detect if raw bytes are an RTP packet (version 2, first 2 bits = 10)
pub fn is_rtp_packet(data: &[u8]) -> bool {
    if data.len() < RTP_HEADER_MIN_SIZE {
        return false;
    }
    (data[0] >> 6) & 0x03 == RTP_VERSION
}

/// Detect if raw bytes are our custom Aurix protocol
pub fn is_aurix_packet(data: &[u8]) -> bool {
    if data.len() < HEADER_SIZE {
        return false;
    }
    data[0..4] == MAGIC_BYTES
}

/// Parsed RTP header for WebRTC-compatible clients
#[derive(Debug, Clone)]
pub struct RtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub csrc_count: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub header_size: usize,
}

impl RtpHeader {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < RTP_HEADER_MIN_SIZE {
            return Err(AurixError::Transport("RTP packet too short".into()));
        }
        let version = (data[0] >> 6) & 0x03;
        if version != RTP_VERSION {
            return Err(AurixError::Transport(format!(
                "Invalid RTP version: {version}"
            )));
        }
        let padding = (data[0] >> 5) & 0x01 != 0;
        let extension = (data[0] >> 4) & 0x01 != 0;
        let csrc_count = data[0] & 0x0F;
        let marker = (data[1] >> 7) & 0x01 != 0;
        let payload_type = data[1] & 0x7F;
        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let mut header_size = RTP_HEADER_MIN_SIZE + (csrc_count as usize * 4);
        if extension && header_size + 4 <= data.len() {
            let ext_len =
                u16::from_be_bytes([data[header_size + 2], data[header_size + 3]]) as usize;
            header_size += 4 + ext_len * 4;
        }
        Ok(Self {
            version,
            padding,
            extension,
            csrc_count,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            header_size,
        })
    }

    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        if data.len() > self.header_size {
            &data[self.header_size..]
        } else {
            &[]
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ControlMessage {
    /// Accepted for backwards compatibility only: the WebSocket server authenticates the
    /// bearer token at upgrade time and assigns the session id itself (`SessionInitAck`).
    SessionInit {
        token: String,
        session_id: SessionId,
    },
    /// `media_key` is a base64 per-session key used to authenticate AURX UDP packets
    /// (`SessionBind` first, then every audio/control packet).
    SessionInitAck {
        session_id: SessionId,
        ssrc: u32,
        media_addr: String,
        media_key: String,
    },
    /// Sent by the server once the UDP source address has been authenticated via `SessionBind`.
    MediaBound {
        session_id: SessionId,
    },
    SessionClose {
        session_id: SessionId,
        reason: String,
    },
    ChannelJoin {
        channel_id: ChannelId,
        token: String,
    },
    ChannelJoinAck {
        channel_id: ChannelId,
        participants: Vec<ParticipantBrief>,
    },
    ChannelLeave {
        channel_id: ChannelId,
    },
    ParticipantJoined {
        channel_id: ChannelId,
        user_id: UserId,
        display_name: String,
        ssrc: u32,
    },
    ParticipantLeft {
        channel_id: ChannelId,
        user_id: UserId,
    },
    MuteStateChanged {
        channel_id: ChannelId,
        user_id: UserId,
        muted: bool,
        server_muted: bool,
    },
    SpeakingStateChanged {
        channel_id: ChannelId,
        user_id: UserId,
        speaking: bool,
    },
    PositionUpdate {
        channel_id: ChannelId,
        positions: Vec<UserPosition>,
    },
    OcclusionUpdate {
        channel_id: ChannelId,
        source_user_id: UserId,
        occlusion_factor: f32,
    },
    ReverbZoneUpdate {
        channel_id: ChannelId,
        reverb: ReverbDescriptor,
    },
    QualityReport {
        rtt_ms: f32,
        jitter_ms: f32,
        packet_loss: f32,
    },
    BitrateCommand {
        target_bitrate_kbps: u32,
        reason: String,
    },
    RecordingNotification {
        channel_id: ChannelId,
        recording_id: uuid::Uuid,
        active: bool,
        initiated_by: UserId,
    },
    RecordingConsentResponse {
        recording_id: uuid::Uuid,
        consent: crate::types::RecordingConsent,
    },
    Error {
        code: String,
        message: String,
    },
    Kick {
        channel_id: ChannelId,
        user_id: UserId,
        reason: String,
    },
    /// Browser clients: SDP offer for this session; the server replies with `WebRtcAnswer`.
    WebRtcOffer {
        sdp: String,
    },
    WebRtcAnswer {
        sdp: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantBrief {
    pub user_id: UserId,
    pub display_name: String,
    pub ssrc: u32,
    pub role: ChannelRole,
    pub is_muted: bool,
    pub is_speaking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPosition {
    pub user_id: UserId,
    pub position: Position3D,
    pub orientation: Orientation3D,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_trailing_bytes_and_oversize() {
        let pkt = AurixPacket::audio(1, 2, 3, 4, Bytes::from_static(b"hello"));
        let mut wire = pkt.encode().to_vec();
        assert!(AurixPacket::decode(&wire).is_ok());
        wire.push(0);
        assert!(AurixPacket::decode(&wire).is_err());
        let big = vec![0u8; MAX_PACKET_SIZE + 1];
        assert!(AurixPacket::decode(&big).is_err());
    }

    #[test]
    fn decode_rejects_checksum_mismatch() {
        let pkt = AurixPacket::audio(1, 2, 3, 4, Bytes::from_static(b"hello"));
        let mut wire = pkt.encode().to_vec();
        wire[HEADER_SIZE] ^= 0xFF;
        assert!(AurixPacket::decode(&wire).is_err());
    }

    #[test]
    fn authenticated_roundtrip_and_tamper_detection() {
        let key = [7u8; 32];
        let pkt = AurixPacket::audio(10, 20, 30, 40, Bytes::from_static(b"opus-frame"));
        let wire = pkt.encode_authenticated(&key);
        let plen = pkt.payload.len();
        assert_eq!(wire.len(), HEADER_SIZE + plen + AUTH_TAG_SIZE);
        let decoded = AurixPacket::decode(&wire).unwrap();
        assert!(decoded.is_authenticated());
        assert!(decoded.verify_auth(&key));
        assert!(!decoded.verify_auth(&[8u8; 32]));

        // Flip a payload byte and fix the CRC so only the HMAC catches it.
        let mut wire2 = wire.to_vec();
        wire2[HEADER_SIZE] ^= 0x01;
        let crc = crc32fast::hash(&wire2[HEADER_SIZE..HEADER_SIZE + plen]);
        wire2[26..30].copy_from_slice(&crc.to_be_bytes());
        let d2 = AurixPacket::decode(&wire2).unwrap();
        assert!(!d2.verify_auth(&key));

        // Unauthenticated encoding of the same packet never verifies.
        let plain = AurixPacket::decode(&pkt.encode()).unwrap();
        assert!(!plain.is_authenticated());
        assert!(!plain.verify_auth(&key));
    }

    #[test]
    fn session_bind_roundtrip() {
        let sid = SessionId::new();
        let pkt = AurixPacket::session_bind(&sid, 99, 1_700_000_000_000, 42);
        let key = [1u8; 32];
        let decoded = AurixPacket::decode(&pkt.encode_authenticated(&key)).unwrap();
        assert!(decoded.verify_auth(&key));
        let (s, ts, nonce) = decoded.parse_session_bind().unwrap();
        assert_eq!(s, sid);
        assert_eq!(ts, 1_700_000_000_000);
        assert_eq!(nonce, 42);
    }

    #[test]
    fn replay_window_rejects_duplicates_and_old_packets() {
        let mut w = ReplayWindow::default();
        assert!(w.check_and_update(100));
        assert!(!w.check_and_update(100));
        assert!(w.check_and_update(101));
        assert!(w.check_and_update(99));
        assert!(!w.check_and_update(99));
        assert!(w.check_and_update(200));
        assert!(!w.check_and_update(100));
        assert!(w.check_and_update(150));
        assert!(!w.check_and_update(150));
    }
}
