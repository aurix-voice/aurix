use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use crate::types::{ChannelId, ChannelRole, Orientation3D, Position3D, SessionId, UserId, ReverbDescriptor};
use crate::error::{AurixError, Result};

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAGIC_BYTES: [u8; 4] = [0x41, 0x55, 0x52, 0x58];
pub const MAX_PACKET_SIZE: usize = 1400;
pub const HEADER_SIZE: usize = 28;
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
            return Err(AurixError::Transport(format!("Unsupported protocol version: {version}")));
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
}

#[derive(Debug, Clone)]
pub struct AurixPacket {
    pub header: PacketHeader,
    pub payload: Bytes,
}

impl AurixPacket {
    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        let mut header = self.header.clone();
        header.payload_length = self.payload.len() as u16;
        header.checksum = crc32fast::hash(&self.payload);
        header.encode(&mut buf);
        buf.put_slice(&self.payload);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut bytes = Bytes::copy_from_slice(data);
        let header = PacketHeader::decode(&mut bytes)?;
        if bytes.remaining() < header.payload_length as usize {
            return Err(AurixError::Transport("Payload truncated".into()));
        }
        let payload = bytes.split_to(header.payload_length as usize);
        let checksum = crc32fast::hash(&payload);
        if checksum != header.checksum {
            return Err(AurixError::Transport("Checksum mismatch".into()));
        }
        Ok(Self { header, payload })
    }

    pub fn audio(seq: u32, ts: u32, ssrc: u32, ch_hash: u32, data: Bytes) -> Self {
        let mut hdr = PacketHeader::new(PacketType::Audio, seq, ts, ssrc);
        hdr.channel_id_hash = ch_hash;
        Self { header: hdr, payload: data }
    }

    pub fn heartbeat(ssrc: u32, ts: u32) -> Self {
        Self { header: PacketHeader::new(PacketType::Heartbeat, 0, ts, ssrc), payload: Bytes::new() }
    }

    pub fn bitrate_command(ssrc: u32, target_bitrate: u32) -> Self {
        let payload = target_bitrate.to_be_bytes().to_vec();
        Self {
            header: PacketHeader::new(PacketType::BitrateCommand, 0, 0, ssrc),
            payload: Bytes::from(payload),
        }
    }
}

pub fn channel_id_hash(id: &ChannelId) -> u32 {
    crc32fast::hash(id.0.as_bytes())
}

/// Detect if raw bytes are an RTP packet (version 2, first 2 bits = 10)
pub fn is_rtp_packet(data: &[u8]) -> bool {
    if data.len() < RTP_HEADER_MIN_SIZE { return false; }
    (data[0] >> 6) & 0x03 == RTP_VERSION
}

/// Detect if raw bytes are our custom Aurix protocol
pub fn is_aurix_packet(data: &[u8]) -> bool {
    if data.len() < HEADER_SIZE { return false; }
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
            return Err(AurixError::Transport(format!("Invalid RTP version: {version}")));
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
            let ext_len = u16::from_be_bytes([data[header_size + 2], data[header_size + 3]]) as usize;
            header_size += 4 + ext_len * 4;
        }
        Ok(Self {
            version, padding, extension, csrc_count,
            marker, payload_type, sequence_number,
            timestamp, ssrc, header_size,
        })
    }

    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        if data.len() > self.header_size { &data[self.header_size..] } else { &[] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ControlMessage {
    SessionInit { token: String, session_id: SessionId },
    SessionInitAck { session_id: SessionId, ssrc: u32, media_addr: String },
    SessionClose { session_id: SessionId, reason: String },
    ChannelJoin { channel_id: ChannelId, token: String },
    ChannelJoinAck { channel_id: ChannelId, participants: Vec<ParticipantBrief> },
    ChannelLeave { channel_id: ChannelId },
    ParticipantJoined { channel_id: ChannelId, user_id: UserId, display_name: String, ssrc: u32 },
    ParticipantLeft { channel_id: ChannelId, user_id: UserId },
    MuteStateChanged { channel_id: ChannelId, user_id: UserId, muted: bool, server_muted: bool },
    SpeakingStateChanged { channel_id: ChannelId, user_id: UserId, speaking: bool },
    PositionUpdate { channel_id: ChannelId, positions: Vec<UserPosition> },
    OcclusionUpdate { channel_id: ChannelId, source_user_id: UserId, occlusion_factor: f32 },
    ReverbZoneUpdate { channel_id: ChannelId, reverb: ReverbDescriptor },
    QualityReport { rtt_ms: f32, jitter_ms: f32, packet_loss: f32 },
    BitrateCommand { target_bitrate_kbps: u32, reason: String },
    RecordingNotification { channel_id: ChannelId, recording_id: uuid::Uuid, active: bool, initiated_by: UserId },
    RecordingConsentResponse { recording_id: uuid::Uuid, consent: crate::types::RecordingConsent },
    Error { code: String, message: String },
    Kick { channel_id: ChannelId, user_id: UserId, reason: String },
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