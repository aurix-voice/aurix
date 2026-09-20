use crate::crypto::MediaKeys;
use crate::error::{AurixError, Result};
use crate::types::{
    ActionKind, AudioCodec, AudioPolicy, ChannelId, ChannelRole, Direction, DownlinkMode,
    MediaTransportKind, Orientation3D, Position3D, ReverbDescriptor, SessionId, UserId,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

/// AURX wire protocol version. v2 encrypts every authenticated payload (AES-256-CTR with keys
/// derived from the session media key, see `MediaKeys`) in addition to the HMAC tag of v1.
pub const PROTOCOL_VERSION: u8 = 2;
pub const MAGIC_BYTES: [u8; 4] = [0x41, 0x55, 0x52, 0x58];
pub const MAX_PACKET_SIZE: usize = 1400;
/// Node-to-node relay datagrams wrap a complete client packet, so they may exceed
/// `MAX_PACKET_SIZE` by one header and one tag.
pub const MAX_RELAY_PACKET_SIZE: usize = MAX_PACKET_SIZE + HEADER_SIZE + AUTH_TAG_SIZE;
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
    /// Cascade envelope: payload is a complete (plaintext) client packet.
    Relay = 0x80,
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
            0x80 => Some(Self::Relay),
            0xFF => Some(Self::Error),
            _ => None,
        }
    }
}

/// Gain factor carried by a `VolumeAttenuated` payload: fixed point with `VOLUME_UNITY` = 1.0,
/// so 0 is silence, 128 is unchanged and 255 is ~2.0 (+6 dB).
pub const VOLUME_UNITY: f32 = 128.0;

pub fn encode_volume_byte(volume: f32) -> u8 {
    (volume.clamp(0.0, 255.0 / VOLUME_UNITY) * VOLUME_UNITY).round() as u8
}

pub fn decode_volume_byte(byte: u8) -> f32 {
    byte as f32 / VOLUME_UNITY
}

/// Two signed bytes carried by a `Directional` downlink payload: azimuth in units of `π/127`
/// (positive = listener's right) and elevation in units of `π/254` (positive = above).
pub const DIRECTION_SIZE: usize = 2;
const AZIMUTH_SCALE: f32 = 127.0 / std::f32::consts::PI;
const ELEVATION_SCALE: f32 = 127.0 / std::f32::consts::FRAC_PI_2;

pub fn encode_direction(direction: &Direction) -> [u8; DIRECTION_SIZE] {
    let az = (direction.azimuth * AZIMUTH_SCALE)
        .round()
        .clamp(-127.0, 127.0) as i8;
    let el = (direction.elevation * ELEVATION_SCALE)
        .round()
        .clamp(-127.0, 127.0) as i8;
    [az as u8, el as u8]
}

pub fn decode_direction(bytes: [u8; DIRECTION_SIZE]) -> Direction {
    Direction {
        azimuth: (bytes[0] as i8) as f32 / AZIMUTH_SCALE,
        elevation: (bytes[1] as i8) as f32 / ELEVATION_SCALE,
    }
}

/// Audio level byte carried by an `Energy` uplink payload: `-dBov` as in RFC 6464, so 0 is a
/// full-scale signal and `AUDIO_LEVEL_SILENCE` (127) means no signal at all.
pub const AUDIO_LEVEL_SILENCE: u8 = 127;

/// Convert a linear energy (RMS of PCM in `-1.0..=1.0`) to the wire level byte.
pub fn encode_audio_level(energy: f32) -> u8 {
    if energy.is_nan() || energy <= 0.0 {
        return AUDIO_LEVEL_SILENCE;
    }
    let dbov = -(20.0 * energy.log10());
    dbov.round().clamp(0.0, AUDIO_LEVEL_SILENCE as f32) as u8
}

/// Inverse of `encode_audio_level`: linear energy in `0.0..=1.0` (`0.0` for silence).
pub fn decode_audio_level(level: u8) -> f32 {
    if level >= AUDIO_LEVEL_SILENCE {
        return 0.0;
    }
    10f32.powf(-(level as f32) / 20.0)
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
    /// Payload is AES-256-CTR encrypted with the session `MediaKeys` (always set by `seal`).
    Encrypted = 0x0001,
    Compressed = 0x0002,
    Dtx = 0x0004,
    Fec = 0x0008,
    KeyFrame = 0x0010,
    Priority = 0x0020,
    Relay = 0x0040,
    /// Payload starts with one gain byte (see `encode_volume_byte`) followed by the Opus frame.
    VolumeAttenuated = 0x0080,
    E2ee = 0x0100,
    Rtp = 0x0200,
    /// Packet carries a trailing `AUTH_TAG_SIZE` HMAC tag over header + payload.
    Authenticated = 0x0400,
    /// Uplink payload starts with one audio level byte (see `encode_audio_level`) followed by
    /// the Opus frame. The server strips it before fan-out.
    Energy = 0x0800,
    /// Downlink payload carries `DIRECTION_SIZE` bytes (see `encode_direction`) placing the
    /// speaker relative to this receiver, after the `VolumeAttenuated` gain byte when present
    /// and before the Opus frame.
    Directional = 0x1000,
    /// The audio frame is G.711 μ-law (`g711`) instead of Opus. Set by a client that negotiated
    /// `AudioCodec::Pcmu` with `SetAudioCodec`; the server transcodes so everybody else still
    /// receives Opus, and sets it on the downlink copies sent to PCMU sessions.
    Pcmu = 0x2000,
    /// Downlink only: the frame is the server's mix of a whole channel for this receiver
    /// (`DownlinkMode::Mixed`) — stereo Opus under the channel's system-voice SSRC
    /// (`system_voice_ssrc`), with the receiver's mutes, volumes, focus and positional /
    /// ambient gains already applied. Never combined with `E2ee` or `Directional`.
    Mixed = 0x4000,
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

    /// Append the authentication tag without encrypting the payload. Only for `SessionBind`,
    /// whose payload the server must read to look up the session key; everything else uses `seal`.
    pub fn encode_authenticated(&self, keys: &MediaKeys) -> BytesMut {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len() + AUTH_TAG_SIZE);
        let mut header = self.finalized_header();
        header.flags |= PacketFlags::Authenticated as u16;
        header.flags &= !(PacketFlags::Encrypted as u16);
        header.encode(&mut buf);
        buf.put_slice(&self.payload);
        let tag = crate::crypto::hmac_sha256(keys.auth_key(), &[&buf]);
        buf.put_slice(&tag[..AUTH_TAG_SIZE]);
        buf
    }

    /// Encrypt the payload and append the authentication tag ("seal").
    ///
    /// Wire layout: `header(30) | AES-256-CTR(payload) | HMAC-SHA256(auth_key, header|ciphertext)[..16]`.
    /// The header CRC covers the ciphertext so a plain `decode` still validates integrity
    /// before any key material is touched. Sets `Encrypted | Authenticated`.
    pub fn seal(&self, keys: &MediaKeys) -> BytesMut {
        Self::seal_parts(&self.header, &self.payload, keys)
    }

    /// `seal` for callers that already hold the header and payload separately (hot path in
    /// the router, which re-seals one plaintext packet for many receivers).
    pub fn seal_parts(header: &PacketHeader, payload: &[u8], keys: &MediaKeys) -> BytesMut {
        let mut header = header.clone();
        header.flags |= (PacketFlags::Authenticated as u16) | (PacketFlags::Encrypted as u16);
        header.payload_length = payload.len() as u16;
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + payload.len() + AUTH_TAG_SIZE);
        buf.resize(HEADER_SIZE, 0);
        buf.put_slice(payload);
        let iv = keys.iv(
            header.packet_type as u8,
            header.ssrc,
            header.sequence,
            header.timestamp,
        );
        keys.apply_ctr(&iv, &mut buf[HEADER_SIZE..]);
        header.checksum = crc32fast::hash(&buf[HEADER_SIZE..]);
        let mut hdr = BytesMut::with_capacity(HEADER_SIZE);
        header.encode(&mut hdr);
        buf[..HEADER_SIZE].copy_from_slice(&hdr);
        let tag = crate::crypto::hmac_sha256(keys.auth_key(), &[&buf]);
        buf.put_slice(&tag[..AUTH_TAG_SIZE]);
        buf
    }

    /// Strict decoder: exact length (no trailing bytes), bounded size, verified checksum.
    /// Authentication tags are extracted but NOT verified here — call `verify_auth`.
    pub fn decode(data: &[u8]) -> Result<Self> {
        Self::decode_bounded(data, MAX_PACKET_SIZE)
    }

    /// `decode` with an explicit size bound (cascade envelopes use `MAX_RELAY_PACKET_SIZE`).
    pub fn decode_bounded(data: &[u8], max_size: usize) -> Result<Self> {
        if data.len() > max_size {
            return Err(AurixError::Transport("Packet exceeds maximum size".into()));
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

    /// Verify the authentication tag against `keys`. Returns false for unauthenticated packets.
    /// Does not decrypt; use `open` for the full verify-then-decrypt step.
    pub fn verify_auth(&self, keys: &MediaKeys) -> bool {
        let Some(tag) = self.auth_tag else {
            return false;
        };
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        self.header.encode(&mut buf);
        buf.put_slice(&self.payload);
        let expected = crate::crypto::hmac_sha256(keys.auth_key(), &[&buf]);
        crate::crypto::constant_time_eq(&expected[..AUTH_TAG_SIZE], &tag)
    }

    /// Verify the tag and, if `Encrypted` is set, decrypt the payload in place. On success the
    /// packet holds the plaintext and the `Encrypted` flag is cleared. Returns false (packet
    /// untouched) when the packet is unauthenticated or the tag does not match.
    pub fn open(&mut self, keys: &MediaKeys) -> bool {
        if !self.verify_auth(keys) {
            return false;
        }
        if self.header.has_flag(PacketFlags::Encrypted) {
            let iv = keys.iv(
                self.header.packet_type as u8,
                self.header.ssrc,
                self.header.sequence,
                self.header.timestamp,
            );
            let mut plain = self.payload.to_vec();
            keys.apply_ctr(&iv, &mut plain);
            self.payload = Bytes::from(plain);
            self.header.flags &= !(PacketFlags::Encrypted as u16);
            self.header.checksum = crc32fast::hash(&self.payload);
        }
        true
    }

    pub fn is_authenticated(&self) -> bool {
        self.auth_tag.is_some()
    }

    pub fn audio(seq: u32, ts: u32, ssrc: u32, ch_hash: u32, data: Bytes) -> Self {
        let mut hdr = PacketHeader::new(PacketType::Audio, seq, ts, ssrc);
        hdr.channel_id_hash = ch_hash;
        Self::new(hdr, data)
    }

    /// Uplink audio packet carrying the sender-measured level of this frame.
    pub fn audio_with_level(
        seq: u32,
        ts: u32,
        ssrc: u32,
        ch_hash: u32,
        level: u8,
        data: &[u8],
    ) -> Self {
        let mut hdr = PacketHeader::new(PacketType::Audio, seq, ts, ssrc);
        hdr.channel_id_hash = ch_hash;
        hdr.flags |= PacketFlags::Energy as u16;
        let mut payload = BytesMut::with_capacity(1 + data.len());
        payload.put_u8(level.min(AUDIO_LEVEL_SILENCE));
        payload.put_slice(data);
        Self::new(hdr, payload.freeze())
    }

    /// Remove the leading audio level byte of an `Energy` payload, returning it (`None` when
    /// the flag is not set). Leaves a plain audio packet behind.
    pub fn take_audio_level(&mut self) -> Option<u8> {
        if !self.header.has_flag(PacketFlags::Energy) {
            return None;
        }
        self.header.flags &= !(PacketFlags::Energy as u16);
        if self.payload.is_empty() {
            return Some(AUDIO_LEVEL_SILENCE);
        }
        let level = self.payload[0].min(AUDIO_LEVEL_SILENCE);
        self.payload = self.payload.slice(1..);
        self.header.payload_length = self.payload.len() as u16;
        Some(level)
    }

    /// Copy of this plain audio packet with the `Energy` flag and `level` byte re-attached
    /// (the inverse of [`Self::take_audio_level`]), for hops that rank speakers by loudness.
    pub fn with_audio_level(&self, level: u8) -> Self {
        let mut header = self.header.clone();
        header.flags |= PacketFlags::Energy as u16;
        let mut payload = BytesMut::with_capacity(1 + self.payload.len());
        payload.put_u8(level.min(AUDIO_LEVEL_SILENCE));
        payload.put_slice(&self.payload);
        Self::new(header, payload.freeze())
    }

    /// Header and payload of the downlink copy of this audio packet for one receiver: the gain
    /// byte is prepended when `volume` is not unity, the direction bytes when `direction` is
    /// given (`VolumeAttenuated` / `Directional` flags set accordingly).
    pub fn downlink_parts(
        &self,
        volume: f32,
        direction: Option<&Direction>,
    ) -> (PacketHeader, Bytes) {
        self.downlink_parts_with(&self.payload, volume, direction)
    }

    /// [`Self::downlink_parts`] with the audio frame replaced by `frame` (server-side
    /// transcoding); the header flags are copied from this packet, so callers set
    /// `PacketFlags::Pcmu` on the result themselves.
    pub fn downlink_parts_with(
        &self,
        frame: &Bytes,
        volume: f32,
        direction: Option<&Direction>,
    ) -> (PacketHeader, Bytes) {
        let attenuated = (volume - 1.0).abs() > 0.01;
        if !attenuated && direction.is_none() {
            let mut header = self.header.clone();
            header.payload_length = frame.len() as u16;
            return (header, frame.clone());
        }
        let mut header = self.header.clone();
        let mut body = BytesMut::with_capacity(1 + DIRECTION_SIZE + frame.len());
        if attenuated {
            header.flags |= PacketFlags::VolumeAttenuated as u16;
            body.put_u8(encode_volume_byte(volume));
        }
        if let Some(direction) = direction {
            header.flags |= PacketFlags::Directional as u16;
            body.put_slice(&encode_direction(direction));
        }
        body.put_slice(frame);
        header.payload_length = body.len() as u16;
        (header, body.freeze())
    }

    /// Strip the receiver-specific metadata of a downlink audio packet, returning the gain
    /// (`1.0` when absent) and direction (`None` when absent). Leaves the bare Opus frame.
    pub fn take_downlink_meta(&mut self) -> (f32, Option<Direction>) {
        let mut volume = 1.0;
        if self.header.has_flag(PacketFlags::VolumeAttenuated) {
            self.header.flags &= !(PacketFlags::VolumeAttenuated as u16);
            if !self.payload.is_empty() {
                volume = decode_volume_byte(self.payload[0]);
                self.payload = self.payload.slice(1..);
            }
        }
        let mut direction = None;
        if self.header.has_flag(PacketFlags::Directional) {
            self.header.flags &= !(PacketFlags::Directional as u16);
            if self.payload.len() >= DIRECTION_SIZE {
                direction = Some(decode_direction([self.payload[0], self.payload[1]]));
                self.payload = self.payload.slice(DIRECTION_SIZE..);
            }
        }
        self.header.payload_length = self.payload.len() as u16;
        (volume, direction)
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

    /// Build a `SessionBind` packet. Must be sent with `encode_authenticated(keys)` (signed,
    /// not encrypted: the server needs the session id to find the key). The nonce and
    /// timestamp are mirrored into the header.
    pub fn session_bind(session_id: &SessionId, ssrc: u32, unix_ms: i64, nonce: u64) -> Self {
        let mut payload = Vec::with_capacity(SESSION_BIND_PAYLOAD_SIZE);
        payload.extend_from_slice(session_id.0.as_bytes());
        payload.extend_from_slice(&unix_ms.to_be_bytes());
        payload.extend_from_slice(&nonce.to_be_bytes());
        Self::new(
            PacketHeader::new(PacketType::SessionBind, nonce as u32, unix_ms as u32, ssrc),
            Bytes::from(payload),
        )
    }

    pub fn session_bind_ack(ssrc: u32, seq: u32, unix_ms: i64) -> Self {
        Self::new(
            PacketHeader::new(PacketType::SessionBindAck, seq, unix_ms as u32, ssrc),
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

    /// Wrap a plaintext client packet into a cascade `Relay` envelope. `relay_ssrc` identifies
    /// the sending node (random per process) and `counter` is its 64-bit send counter, split
    /// across the sequence (low) and timestamp (high) fields so envelope IVs never repeat.
    /// The payload is `sender` (16 bytes) followed by the encoded client packet, so the
    /// receiving node can apply receiver preferences without knowing the remote SSRC map.
    pub fn relay_envelope(
        inner: &AurixPacket,
        relay_ssrc: u32,
        counter: u64,
        sender: &UserId,
    ) -> Self {
        let mut hdr = PacketHeader::new(
            PacketType::Relay,
            counter as u32,
            (counter >> 32) as u32,
            relay_ssrc,
        );
        hdr.set_flag(PacketFlags::Relay);
        hdr.channel_id_hash = inner.header.channel_id_hash;
        let encoded = inner.encode();
        let mut payload = BytesMut::with_capacity(16 + encoded.len());
        payload.put_slice(sender.0.as_bytes());
        payload.put_slice(&encoded);
        Self::new(hdr, payload.freeze())
    }

    /// Extract the sending user and the client packet carried by an opened `Relay` envelope.
    pub fn relay_inner(&self) -> Result<(UserId, AurixPacket)> {
        if self.header.packet_type != PacketType::Relay {
            return Err(AurixError::Transport("Not a Relay packet".into()));
        }
        if self.payload.len() < 16 + HEADER_SIZE {
            return Err(AurixError::Transport("Relay envelope too short".into()));
        }
        let sender = UserId(
            uuid::Uuid::from_slice(&self.payload[..16])
                .map_err(|_| AurixError::Transport("Invalid relay sender id".into()))?,
        );
        Ok((sender, AurixPacket::decode(&self.payload[16..])?))
    }
}

/// Anti-replay window over a monotonic sequence (RFC 3711 §3.3.2 style, 64-packet window).
/// Works on 32-bit client sequences and on the 64-bit cascade envelope counter.
#[derive(Debug, Default, Clone)]
pub struct ReplayWindow {
    highest: u64,
    bitmap: u64,
    initialized: bool,
}

impl ReplayWindow {
    pub const WINDOW: u64 = 64;

    /// Returns true and records the sequence if it is fresh; false for replays / too-old packets.
    pub fn check_and_update(&mut self, seq: u32) -> bool {
        self.check_and_update_u64(seq as u64)
    }

    pub fn check_and_update_u64(&mut self, seq: u64) -> bool {
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

/// Whether an Opus packet was encoded with two channels: bit 2 of the TOC byte (RFC 6716
/// §3.1). Receivers use it to pick a stereo decoder for a stereo uplink; an empty packet
/// (DTX) is neither.
pub fn opus_packet_is_stereo(packet: &[u8]) -> bool {
    packet.first().is_some_and(|toc| toc & 0x04 != 0)
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
    ///
    /// `resume_token` is a one-time secret: if the WebSocket drops, the client may reconnect
    /// within `resume_grace_ms` presenting `session_id` + `resume_token` (see `aurix-ws`) and
    /// gets the same session, SSRC, media key and channel memberships back (`resumed: true`,
    /// followed by one `ChannelJoinAck` per channel still joined). Every ack rotates the token.
    ///
    /// `media_tunnel: true` means this very WebSocket also accepts AURX media as binary frames
    /// (one sealed packet per frame, same `SessionBind` handshake and per-packet
    /// authentication as UDP) — the fallback native clients use when UDP is blocked.
    SessionInitAck {
        session_id: SessionId,
        ssrc: u32,
        media_addr: String,
        media_key: String,
        #[serde(default)]
        resume_token: String,
        #[serde(default)]
        resume_grace_ms: u64,
        #[serde(default)]
        resumed: bool,
        #[serde(default)]
        media_tunnel: bool,
        /// The node can serve native sessions one server-mixed stream per channel
        /// (`SetDownlinkMode { mode: "mixed" }`, `ChannelConfig.audience.mix_for_listeners`).
        #[serde(default)]
        downlink_mix: bool,
        /// The session was resumed on a different node than the one that opened it: same
        /// session id and SSRC, new `media_addr` and `media_key`. Its downlink audio sequence
        /// jumps forward (never back) on the new node, so peers' anti-replay windows keep
        /// accepting it and their jitter buffers resynchronise on the gap.
        #[serde(default)]
        migrated: bool,
        /// Public WebSocket URLs of other healthy nodes (same region first). When this node
        /// stops answering, reconnect with the same resume credential to one of them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        failover: Vec<String>,
    },
    /// Sent by the server once a media path has been authenticated via `SessionBind`:
    /// `transport` is `udp`, `tunnel` (AURX over this WebSocket) or `webrtc`.
    MediaBound {
        session_id: SessionId,
        #[serde(default)]
        transport: MediaTransportKind,
    },
    SessionClose {
        session_id: SessionId,
        reason: String,
    },
    ChannelJoin {
        channel_id: ChannelId,
        /// Session JWT (legacy, ignored) or a one-time `join` action token for this channel.
        token: String,
    },
    ChannelJoinAck {
        channel_id: ChannelId,
        participants: Vec<ParticipantBrief>,
        /// The channel transcribes speech and delivers `Transcript` events.
        #[serde(default)]
        transcription: bool,
        /// Speech in this channel is transcribed and classified by the operator's content
        /// safety pipeline (`ChannelConfig.safety_voice`); games should disclose this to players.
        #[serde(default)]
        safety_voice: bool,
        /// Encoder settings this channel requires; merge over all joined channels.
        #[serde(default)]
        audio: AudioPolicy,
        /// Presence is radius-scoped (`PositionalConfig.roster_radius`): `participants` and
        /// later `ParticipantJoined`/`ParticipantLeft` reflect who is within this distance of
        /// you, and members appear only once both of you reported a position.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        roster_radius: Option<f32>,
        /// Channel text (chat, typing, transcripts) only reaches members within this distance
        /// of the sender (`PositionalConfig.text_radius`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_radius: Option<f32>,
        /// Your role in this channel; `listener` means receive-only (the grant had
        /// `speak: false`) and applies to every channel type.
        #[serde(default = "default_participant_role")]
        role: ChannelRole,
        /// Members in the channel across all nodes, including listeners hidden from
        /// `participants` by `ChannelConfig.audience.hide_listeners`.
        #[serde(default)]
        participant_count: u32,
        /// Listeners are hidden from presence in this channel (`audience.hide_listeners`).
        #[serde(default)]
        hidden_listeners: bool,
    },
    /// Server→client: an operator changed the channel's audio settings while you are in it.
    ChannelAudioPolicy {
        channel_id: ChannelId,
        audio: AudioPolicy,
    },
    ChannelLeave {
        channel_id: ChannelId,
    },
    ParticipantJoined {
        channel_id: ChannelId,
        user_id: UserId,
        display_name: String,
        ssrc: u32,
        #[serde(default = "default_participant_role")]
        role: ChannelRole,
        #[serde(default)]
        is_muted: bool,
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
    /// Client→server: stop hearing `user_id` — in one channel or, with `channel_id: None`,
    /// everywhere. Affects only this session; the sender is not told.
    SetParticipantMute {
        user_id: UserId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_id: Option<ChannelId>,
        muted: bool,
    },
    /// Client→server: gain applied to `user_id`'s audio for this session only
    /// (0.0 = silent, 1.0 = as sent, up to 2.0 = +6 dB; combined with positional attenuation).
    SetParticipantVolume {
        user_id: UserId,
        volume: f32,
    },
    /// Client→server: persistent, mutual cross-mute with `user_id` (survives sessions).
    SetUserBlock {
        user_id: UserId,
        blocked: bool,
    },
    /// Server→client: a block placed by this user changed (ack, or sync from another session /
    /// the REST API).
    UserBlockChanged {
        user_id: UserId,
        blocked: bool,
    },
    /// Server→client, after `SessionInitAck`: everything this session is currently not hearing
    /// or hearing at a non-default gain, plus its transmission mode and focused channel. Local
    /// mutes, volumes, transmission and focus survive a resume only.
    ReceiverPreferences {
        blocked_users: Vec<UserId>,
        local_mutes: Vec<LocalMute>,
        volumes: Vec<ParticipantVolume>,
        #[serde(default)]
        transmission: TransmissionMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus_channel: Option<ChannelId>,
        /// Uplink/downlink codec of this session (`opus` unless negotiated otherwise).
        #[serde(default)]
        codec: AudioCodec,
        /// How channel audio reaches this session (`streams` unless `SetDownlinkMode` said
        /// otherwise).
        #[serde(default)]
        downlink: DownlinkMode,
    },
    /// Client→server (native AURX only): switch this session's own audio frames to `codec`.
    /// With `pcmu` the client sends G.711 μ-law frames flagged `PacketFlags::Pcmu` and receives
    /// its downlink as PCMU; the server transcodes to/from the Opus the rest of the channel
    /// uses. Rejected when the node disables `media.pcmu_fallback` or the session is WebRTC.
    SetAudioCodec {
        codec: AudioCodec,
    },
    /// Server→client: ack of `SetAudioCodec`; frames sent from now on must use `codec`.
    AudioCodecChanged {
        codec: AudioCodec,
    },
    /// Client→server (native AURX only): receive channel audio as one server-mixed stream per
    /// channel (`mixed`, see `PacketFlags::Mixed`) or as one stream per speaker (`streams`,
    /// the default). Rejected when the node disables `media.downlink_mix` or the session is
    /// WebRTC (browsers are always mixed).
    SetDownlinkMode {
        mode: DownlinkMode,
    },
    /// Server→client: ack of `SetDownlinkMode`. Frames already in flight may still be of the
    /// previous kind.
    DownlinkModeChanged {
        mode: DownlinkMode,
    },
    /// Client→server: which of the joined channels receive this session's microphone.
    /// `single` must name a joined channel; leaving that channel switches to `none`.
    SetTransmission {
        mode: TransmissionMode,
    },
    /// Server→client: ack of `SetTransmission`, or an automatic change (target channel left).
    TransmissionChanged {
        mode: TransmissionMode,
    },
    /// Client→server: hear `channel_id` at full volume and every other joined channel
    /// attenuated by `media.unfocused_channel_gain`; `None` restores equal volume.
    SetChannelFocus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_id: Option<ChannelId>,
    },
    /// Server→client: ack of `SetChannelFocus`, or an automatic reset (focused channel left).
    ChannelFocusChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_id: Option<ChannelId>,
    },
    SpeakingStateChanged {
        channel_id: ChannelId,
        user_id: UserId,
        speaking: bool,
    },
    /// Server→client, periodic (`media.energy_interval_ms`): audio level of every channel
    /// member whose level changed since the last report; `energy` is linear `0.0..=1.0`
    /// (see `decode_audio_level`). A participant that went quiet is reported once with `0.0`.
    ChannelEnergy {
        channel_id: ChannelId,
        levels: Vec<ParticipantEnergy>,
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
    /// Client→server, periodic: what the client sees on its downlink. `packet_loss` is a
    /// percentage (`0..=100`).
    QualityReport {
        rtt_ms: f32,
        jitter_ms: f32,
        packet_loss: f32,
    },
    /// Server→client: bitrate adaptation for the whole uplink, bounded by the merged channel
    /// policy (`min_bitrate_bps..=bitrate_bps`). `expected_loss_percent` is the loss the server
    /// currently sees, for the encoder's FEC tuning (`OPUS_SET_PACKET_LOSS_PERC`).
    BitrateCommand {
        target_bitrate_kbps: u32,
        reason: String,
        #[serde(default)]
        expected_loss_percent: u8,
    },
    /// Server→client, periodic (`media.quality_interval_ms`): the server's view of this
    /// session's link — client-reported downlink merged with the uplink it measures itself.
    /// Sent when the bar count changes and at least every fifth interval.
    NetworkQuality {
        quality: crate::types::NetworkQuality,
    },
    /// `live` marks a real-time stream to an operator service (as opposed to a stored file);
    /// `initiated_by` is the nil user id when an operator started it via the REST API.
    RecordingNotification {
        channel_id: ChannelId,
        recording_id: uuid::Uuid,
        active: bool,
        initiated_by: UserId,
        #[serde(default)]
        live: bool,
    },
    RecordingConsentResponse {
        recording_id: uuid::Uuid,
        consent: crate::types::RecordingConsent,
    },
    /// `client_ref` is set when the error answers a `ChatSend`/`ChatSendDirect` that carried
    /// one, so the client can mark that message as failed.
    Error {
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_ref: Option<String>,
    },
    Kick {
        channel_id: ChannelId,
        user_id: UserId,
        reason: String,
    },
    /// Client-initiated moderation backed by a one-time `kick` / `mute` / `unmute` action
    /// token minted by the game backend for this actor, channel and target.
    ModerateParticipant {
        channel_id: ChannelId,
        user_id: UserId,
        action: ActionKind,
        token: String,
        #[serde(default)]
        reason: Option<String>,
    },
    ModerateParticipantAck {
        channel_id: ChannelId,
        user_id: UserId,
        action: ActionKind,
    },
    /// Text chat: send to every participant of a joined channel. `client_ref` is echoed back
    /// in the sender's own `ChatMessageReceived` so the client can correlate the ack.
    ChatSend {
        channel_id: ChannelId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_ref: Option<String>,
    },
    /// Text chat: directed message to one online user of the same app (party invite, whisper,
    /// ping). Not stored for offline users.
    ChatSendDirect {
        user_id: UserId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_ref: Option<String>,
    },
    ChatMessageReceived {
        message: ChatMessage,
    },
    /// Client → server: this session is (no longer) composing a message in `channel_id`.
    ChatTyping {
        channel_id: ChannelId,
        typing: bool,
    },
    /// Server → other channel members.
    ParticipantTyping {
        channel_id: ChannelId,
        user_id: UserId,
        typing: bool,
    },
    /// Server → channel members: a speech-to-text segment of `user_id` in a channel with
    /// `transcription: true`. Ephemeral; not stored server-side.
    Transcript {
        transcript: Transcript,
    },
    /// Client → server: opt out of (or back into) receiving `Transcript` events on this
    /// session. Receiving is on by default; the speaker side is governed by the channel.
    SetTranscripts {
        enabled: bool,
    },
    /// Client → server: synthesize `text` server-side and play it as this participant's
    /// voice. `channel_id` = `None` plays into every channel the session transmits to;
    /// `destination` selects who hears it. `client_ref` is echoed in `TtsStatus`.
    TtsSpeak {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_id: Option<ChannelId>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        voice: Option<String>,
        #[serde(default)]
        destination: TtsDestination,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_ref: Option<String>,
    },
    /// Client → server: drop this session's pending utterances and stop the current one.
    TtsCancel,
    /// Server → requesting client: lifecycle of a `TtsSpeak` request.
    TtsStatus {
        request_id: uuid::Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_ref: Option<String>,
        state: TtsState,
        /// Length of the synthesized audio (set from `Playing` onwards).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalMute {
    pub user_id: UserId,
    /// `None` = muted in every channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<ChannelId>,
}

/// One transcribed speech segment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Transcript {
    pub id: uuid::Uuid,
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Start of the segment, server clock.
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: u64,
    /// Word timings relative to `started_at` (only when the node has `stt.include_words`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<TranscriptWordTiming>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranscriptWordTiming {
    pub word: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Who hears a TTS utterance requested by a participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TtsDestination {
    /// Everyone the participant's microphone would reach (their own voice).
    #[default]
    Channel,
    /// Only the requesting session (preview / accessibility read-out).
    Local,
    /// Both of the above.
    Both,
}

impl TtsDestination {
    pub fn to_channel(self) -> bool {
        matches!(self, Self::Channel | Self::Both)
    }

    pub fn to_self(self) -> bool {
        matches!(self, Self::Local | Self::Both)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsState {
    Queued,
    Playing,
    Finished,
    Cancelled,
    Failed,
}

/// Where a session's microphone goes when it is a member of several channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TransmissionMode {
    /// Audio is accepted by the server but forwarded nowhere.
    None,
    /// Only `channel_id` receives audio; frames addressed elsewhere are dropped.
    Single { channel_id: ChannelId },
    /// Every joined channel (default).
    #[default]
    All,
}

impl TransmissionMode {
    pub fn allows(&self, channel: &ChannelId) -> bool {
        match self {
            Self::None => false,
            Self::Single { channel_id } => channel_id == channel,
            Self::All => true,
        }
    }
}

/// A delivered text message. `channel_id` is `None` for directed messages; `from_user_id` is
/// the nil UUID for server/system messages posted via the REST API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub id: uuid::Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<ChannelId>,
    pub from_user_id: UserId,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_user_id: Option<UserId>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub sent_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParticipantVolume {
    pub user_id: UserId,
    pub volume: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParticipantEnergy {
    pub user_id: UserId,
    pub energy: f32,
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

fn default_participant_role() -> ChannelRole {
    ChannelRole::Speaker
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Pinned wire vectors shared with the C# SDK tests (sdk/unity/.../ProtocolTests.cs):
    /// any change here is a protocol change and must be mirrored in every client.
    #[test]
    fn v2_wire_vectors_are_stable() {
        let keys = MediaKeys::derive(&[7u8; 32]);
        let pkt = AurixPacket::audio(10, 20, 30, 40, Bytes::from_static(b"opus-frame"));
        assert_eq!(
            hex(&pkt.seal(&keys)),
            "41555258020104010000000a000000140000001e00000028000afa37ba15f071d27d551588a11d2e7f3dcaf9bf4f86642b9ed1df5aee423d"
        );
        let sid = SessionId(uuid::Uuid::from_bytes([0x11; 16]));
        let bind = AurixPacket::session_bind(&sid, 30, 1_700_000_000_000, 0x0102030405060708);
        assert_eq!(
            hex(&bind.encode_authenticated(&keys)),
            "415552580233040005060708cfe568000000001e0000000000200e1d7cbb111111111111111111111111111111110000018bcfe5680001020304050607086b0c72c29a059fa0e0627df1d5942167"
        );
    }

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
    fn sealed_roundtrip_and_tamper_detection() {
        let keys = MediaKeys::derive(&[7u8; 32]);
        let other = MediaKeys::derive(&[8u8; 32]);
        let pkt = AurixPacket::audio(10, 20, 30, 40, Bytes::from_static(b"opus-frame"));
        let wire = pkt.seal(&keys);
        let plen = pkt.payload.len();
        assert_eq!(wire.len(), HEADER_SIZE + plen + AUTH_TAG_SIZE);
        let mut decoded = AurixPacket::decode(&wire).unwrap();
        assert!(decoded.is_authenticated());
        assert!(decoded.header.has_flag(PacketFlags::Encrypted));
        // Ciphertext on the wire differs from the plaintext.
        assert_ne!(&decoded.payload[..], b"opus-frame");
        assert!(decoded.verify_auth(&keys));
        assert!(!decoded.verify_auth(&other));
        let mut wrong = decoded.clone();
        assert!(!wrong.open(&other));
        assert!(decoded.open(&keys));
        assert_eq!(&decoded.payload[..], b"opus-frame");
        assert!(!decoded.header.has_flag(PacketFlags::Encrypted));

        // Flip a payload byte and fix the CRC so only the HMAC catches it.
        let mut wire2 = wire.to_vec();
        wire2[HEADER_SIZE] ^= 0x01;
        let crc = crc32fast::hash(&wire2[HEADER_SIZE..HEADER_SIZE + plen]);
        wire2[26..30].copy_from_slice(&crc.to_be_bytes());
        let mut d2 = AurixPacket::decode(&wire2).unwrap();
        assert!(!d2.verify_auth(&keys));
        assert!(!d2.open(&keys));

        // Unauthenticated encoding of the same packet never verifies.
        let mut plain = AurixPacket::decode(&pkt.encode()).unwrap();
        assert!(!plain.is_authenticated());
        assert!(!plain.verify_auth(&keys));
        assert!(!plain.open(&keys));
    }

    #[test]
    fn seal_ivs_differ_per_sequence_and_key() {
        let keys = MediaKeys::derive(&[7u8; 32]);
        let a = AurixPacket::audio(1, 0, 30, 40, Bytes::from_static(b"same-frame")).seal(&keys);
        let b = AurixPacket::audio(2, 0, 30, 40, Bytes::from_static(b"same-frame")).seal(&keys);
        assert_ne!(
            &a[HEADER_SIZE..HEADER_SIZE + 10],
            &b[HEADER_SIZE..HEADER_SIZE + 10]
        );
        let c = AurixPacket::audio(1, 0, 30, 40, Bytes::from_static(b"same-frame"))
            .seal(&MediaKeys::derive(&[9u8; 32]));
        assert_ne!(
            &a[HEADER_SIZE..HEADER_SIZE + 10],
            &c[HEADER_SIZE..HEADER_SIZE + 10]
        );
        // Deterministic for identical inputs (no hidden randomness in the format).
        let a2 = AurixPacket::audio(1, 0, 30, 40, Bytes::from_static(b"same-frame")).seal(&keys);
        assert_eq!(a, a2);
    }

    #[test]
    fn volume_byte_is_fixed_point_with_unity_at_128() {
        assert_eq!(encode_volume_byte(1.0), 128);
        assert_eq!(encode_volume_byte(0.5), 64);
        assert_eq!(encode_volume_byte(0.0), 0);
        assert_eq!(encode_volume_byte(2.0), 255);
        assert_eq!(encode_volume_byte(7.0), 255);
        assert_eq!(decode_volume_byte(128), 1.0);
        assert_eq!(decode_volume_byte(64), 0.5);
        assert!((decode_volume_byte(255) - 1.992).abs() < 0.001);
    }

    #[test]
    fn audio_level_byte_is_negative_dbov() {
        assert_eq!(encode_audio_level(1.0), 0);
        assert_eq!(encode_audio_level(0.1), 20);
        assert_eq!(encode_audio_level(0.01), 40);
        assert_eq!(encode_audio_level(0.0), AUDIO_LEVEL_SILENCE);
        assert_eq!(encode_audio_level(-1.0), AUDIO_LEVEL_SILENCE);
        assert_eq!(encode_audio_level(f32::NAN), AUDIO_LEVEL_SILENCE);
        assert_eq!(encode_audio_level(1e-9), AUDIO_LEVEL_SILENCE);
        assert_eq!(decode_audio_level(0), 1.0);
        assert!((decode_audio_level(20) - 0.1).abs() < 1e-6);
        assert_eq!(decode_audio_level(AUDIO_LEVEL_SILENCE), 0.0);
        assert_eq!(decode_audio_level(200), 0.0);
    }

    #[test]
    fn direction_bytes_roundtrip_with_fixed_wire_values() {
        use std::f32::consts::{FRAC_PI_2, PI};
        assert_eq!(encode_direction(&Direction::AHEAD), [0, 0]);
        assert_eq!(
            encode_direction(&Direction {
                azimuth: FRAC_PI_2,
                elevation: 0.0
            }),
            [64, 0]
        );
        assert_eq!(
            encode_direction(&Direction {
                azimuth: -FRAC_PI_2,
                elevation: -FRAC_PI_2
            }),
            [0xC0, 0x81]
        );
        assert_eq!(
            encode_direction(&Direction {
                azimuth: PI,
                elevation: FRAC_PI_2
            }),
            [127, 127]
        );
        // Out-of-range values saturate instead of wrapping to the other side.
        assert_eq!(
            encode_direction(&Direction {
                azimuth: -4.0,
                elevation: 9.0
            }),
            [0x81, 127]
        );
        let d = decode_direction([64, 0xC0]);
        assert!((d.azimuth - FRAC_PI_2).abs() < 0.02);
        // -64/127 of π/2 ≈ -45°
        assert!((d.elevation + std::f32::consts::FRAC_PI_4).abs() < 0.02);
    }

    #[test]
    fn downlink_parts_carry_volume_and_direction_in_order() {
        let keys = MediaKeys::derive(b"directional");
        let packet = AurixPacket::audio(3, 1920, 7, 8, Bytes::from_static(b"opus"));
        let dir = Direction {
            azimuth: std::f32::consts::FRAC_PI_2,
            elevation: 0.0,
        };

        let (header, body) = packet.downlink_parts(1.0, None);
        assert_eq!(header.flags, packet.header.flags);
        assert_eq!(&body[..], b"opus");

        let (header, body) = packet.downlink_parts(1.0, Some(&dir));
        assert!(header.has_flag(PacketFlags::Directional));
        assert!(!header.has_flag(PacketFlags::VolumeAttenuated));
        assert_eq!(&body[..], &[64, 0, b'o', b'p', b'u', b's']);

        let (header, body) = packet.downlink_parts(0.5, Some(&dir));
        assert!(header.has_flag(PacketFlags::Directional));
        assert!(header.has_flag(PacketFlags::VolumeAttenuated));
        assert_eq!(&body[..], &[64, 64, 0, b'o', b'p', b'u', b's']);

        let wire = AurixPacket::seal_parts(&header, &body, &keys);
        let mut decoded = AurixPacket::decode(&wire).unwrap();
        assert!(decoded.open(&keys));
        let (volume, direction) = decoded.take_downlink_meta();
        assert_eq!(volume, 0.5);
        let direction = direction.expect("direction");
        assert!((direction.azimuth - dir.azimuth).abs() < 0.02);
        assert_eq!(&decoded.payload[..], b"opus");
        assert_eq!(decoded.header.payload_length, 4);
        assert!(!decoded.header.has_flag(PacketFlags::Directional));
        assert!(!decoded.header.has_flag(PacketFlags::VolumeAttenuated));

        let mut plain = AurixPacket::audio(1, 0, 7, 8, Bytes::from_static(b"x"));
        assert_eq!(plain.take_downlink_meta(), (1.0, None));
        assert_eq!(&plain.payload[..], b"x");
    }

    #[test]
    fn energy_packet_roundtrip_and_strip() {
        let keys = MediaKeys::derive(b"energy-test");
        let packet = AurixPacket::audio_with_level(7, 960, 42, 99, 23, b"opus");
        assert!(packet.header.has_flag(PacketFlags::Energy));
        assert_eq!(&packet.payload[..], &[23, b'o', b'p', b'u', b's']);

        let wire = packet.seal(&keys);
        let mut decoded = AurixPacket::decode(&wire).unwrap();
        assert!(decoded.open(&keys));
        assert_eq!(decoded.take_audio_level(), Some(23));
        assert!(!decoded.header.has_flag(PacketFlags::Energy));
        assert_eq!(&decoded.payload[..], b"opus");
        assert_eq!(decoded.header.payload_length, 4);
        assert_eq!(decoded.take_audio_level(), None);

        let mut plain = AurixPacket::audio(1, 0, 42, 99, Bytes::from_static(b"x"));
        assert_eq!(plain.take_audio_level(), None);
        assert_eq!(&plain.payload[..], b"x");

        let mut empty = AurixPacket::audio_with_level(1, 0, 42, 99, 200, b"");
        assert_eq!(&empty.payload[..], &[AUDIO_LEVEL_SILENCE]);
        assert_eq!(empty.take_audio_level(), Some(AUDIO_LEVEL_SILENCE));
        assert!(empty.payload.is_empty());
    }

    #[test]
    fn relay_envelope_roundtrip() {
        let keys = MediaKeys::derive(b"cluster-cascade-secret");
        let inner = AurixPacket::audio(5, 960, 77, 88, Bytes::from_static(b"frame"));
        let sender = UserId::new();
        let env = AurixPacket::relay_envelope(&inner, 0xDEAD_BEEF, (3u64 << 32) | 9, &sender);
        assert_eq!(env.header.sequence, 9);
        assert_eq!(env.header.timestamp, 3);
        assert!(env.header.has_flag(PacketFlags::Relay));
        let wire = env.seal(&keys);
        let mut got = AurixPacket::decode_bounded(&wire, MAX_RELAY_PACKET_SIZE).unwrap();
        assert!(got.open(&keys));
        let (from, back) = got.relay_inner().unwrap();
        assert_eq!(from, sender);
        assert_eq!(back.header.sequence, 5);
        assert_eq!(back.header.ssrc, 77);
        assert_eq!(back.header.channel_id_hash, 88);
        assert_eq!(&back.payload[..], b"frame");
        assert!(!back.is_authenticated());
    }

    #[test]
    fn session_bind_roundtrip() {
        let sid = SessionId::new();
        let pkt = AurixPacket::session_bind(&sid, 99, 1_700_000_000_000, 42);
        let keys = MediaKeys::derive(&[1u8; 32]);
        let mut decoded = AurixPacket::decode(&pkt.encode_authenticated(&keys)).unwrap();
        assert!(!decoded.header.has_flag(PacketFlags::Encrypted));
        // Parseable before the key is known …
        assert_eq!(decoded.parse_session_bind().unwrap().0, sid);
        // … and verifiable once it is.
        assert!(decoded.open(&keys));
        let (s, ts, nonce) = decoded.parse_session_bind().unwrap();
        assert_eq!(s, sid);
        assert_eq!(ts, 1_700_000_000_000);
        assert_eq!(nonce, 42);
        assert_eq!(decoded.header.sequence, 42);
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

    #[test]
    fn chat_messages_serialize_with_optional_fields_omitted() {
        let send: ControlMessage = serde_json::from_str(
            r#"{"type":"ChatSend","data":{"channel_id":"11111111-1111-1111-1111-111111111111","text":"gg"}}"#,
        )
        .unwrap();
        match send {
            ControlMessage::ChatSend {
                text,
                metadata,
                client_ref,
                ..
            } => {
                assert_eq!(text, "gg");
                assert!(metadata.is_none() && client_ref.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }

        let msg = ChatMessage {
            id: uuid::Uuid::nil(),
            channel_id: None,
            from_user_id: UserId(uuid::Uuid::nil()),
            display_name: "p".into(),
            to_user_id: Some(UserId(uuid::Uuid::nil())),
            text: "ping".into(),
            metadata: None,
            sent_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            client_ref: None,
        };
        let json = serde_json::to_value(ControlMessage::ChatMessageReceived {
            message: msg.clone(),
        })
        .unwrap();
        assert_eq!(json["type"], "ChatMessageReceived");
        let m = &json["data"]["message"];
        assert!(m.get("channel_id").is_none() && m.get("metadata").is_none());
        assert!(m.get("client_ref").is_none());
        assert_eq!(m["to_user_id"], "00000000-0000-0000-0000-000000000000");
        let back: ControlMessage = serde_json::from_value(json).unwrap();
        match back {
            ControlMessage::ChatMessageReceived { message } => assert_eq!(message, msg),
            other => panic!("unexpected {other:?}"),
        }

        let typing = serde_json::to_string(&ControlMessage::ChatTyping {
            channel_id: ChannelId(uuid::Uuid::nil()),
            typing: true,
        })
        .unwrap();
        assert!(typing.contains(r#""type":"ChatTyping""#));
    }

    /// Wire shape shared with the Web/Unity SDKs: `{"mode":"single","channel_id":…}`.
    #[test]
    fn transmission_mode_wire_shape() {
        let ch = ChannelId(uuid::Uuid::nil());
        let single = serde_json::to_string(&ControlMessage::SetTransmission {
            mode: TransmissionMode::Single { channel_id: ch },
        })
        .unwrap();
        assert_eq!(
            single,
            r#"{"type":"SetTransmission","data":{"mode":{"mode":"single","channel_id":"00000000-0000-0000-0000-000000000000"}}}"#
        );
        let none: ControlMessage =
            serde_json::from_str(r#"{"type":"SetTransmission","data":{"mode":{"mode":"none"}}}"#)
                .unwrap();
        assert!(matches!(
            none,
            ControlMessage::SetTransmission {
                mode: TransmissionMode::None
            }
        ));
        assert!(TransmissionMode::All.allows(&ch));
        assert!(!TransmissionMode::None.allows(&ch));
        assert!(TransmissionMode::Single { channel_id: ch }.allows(&ch));
        assert!(!TransmissionMode::Single { channel_id: ch }.allows(&ChannelId::new()));

        // Older servers/clients omit the new fields; they default to `all` / no focus.
        let prefs: ControlMessage = serde_json::from_str(
            r#"{"type":"ReceiverPreferences","data":{"blocked_users":[],"local_mutes":[],"volumes":[]}}"#,
        )
        .unwrap();
        match prefs {
            ControlMessage::ReceiverPreferences {
                transmission,
                focus_channel,
                ..
            } => {
                assert_eq!(transmission, TransmissionMode::All);
                assert!(focus_channel.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        let focus =
            serde_json::to_string(&ControlMessage::SetChannelFocus { channel_id: None }).unwrap();
        assert_eq!(focus, r#"{"type":"SetChannelFocus","data":{}}"#);
    }

    #[test]
    fn transcript_and_tts_messages_json_shape() {
        let ch = ChannelId::new();
        // Speak with defaults: destination falls back to `channel`.
        let speak: ControlMessage = serde_json::from_str(&format!(
            r#"{{"type":"TtsSpeak","data":{{"channel_id":"{}","text":"go go go"}}}}"#,
            ch.0
        ))
        .unwrap();
        match speak {
            ControlMessage::TtsSpeak {
                channel_id,
                text,
                voice,
                destination,
                client_ref,
            } => {
                assert_eq!(channel_id, Some(ch));
                assert_eq!(text, "go go go");
                assert!(voice.is_none() && client_ref.is_none());
                assert_eq!(destination, TtsDestination::Channel);
            }
            other => panic!("unexpected {other:?}"),
        }
        // Unit variant carries no `data`.
        let cancel = serde_json::to_string(&ControlMessage::TtsCancel).unwrap();
        assert_eq!(cancel, r#"{"type":"TtsCancel"}"#);
        let parsed: ControlMessage = serde_json::from_str(r#"{"type":"TtsCancel"}"#).unwrap();
        assert!(matches!(parsed, ControlMessage::TtsCancel));

        let status = serde_json::to_string(&ControlMessage::TtsStatus {
            request_id: uuid::Uuid::nil(),
            client_ref: None,
            state: TtsState::Playing,
            duration_ms: Some(1200),
            message: None,
        })
        .unwrap();
        assert!(status.contains(r#""state":"playing""#));
        assert!(!status.contains("client_ref"));

        // Old join acks without `transcription` still parse.
        let ack: ControlMessage = serde_json::from_str(&format!(
            r#"{{"type":"ChannelJoinAck","data":{{"channel_id":"{}","participants":[]}}}}"#,
            ch.0
        ))
        .unwrap();
        assert!(matches!(
            ack,
            ControlMessage::ChannelJoinAck {
                transcription: false,
                ..
            }
        ));

        let transcript = ControlMessage::Transcript {
            transcript: Transcript {
                id: uuid::Uuid::nil(),
                channel_id: ch,
                user_id: UserId::new(),
                text: "hello".into(),
                language: Some("en".into()),
                started_at: chrono::Utc::now(),
                duration_ms: 900,
                words: Vec::new(),
            },
        };
        let json = serde_json::to_string(&transcript).unwrap();
        assert!(!json.contains("\"words\""));
        assert!(json.contains(r#""type":"Transcript""#));
    }

    /// Wire shape of the audio policy shared with the Web/Unity SDKs: snake_case enums, `complexity`
    /// null when the operator left no hint, and every key optional on the way in.
    #[test]
    fn audio_policy_wire_shape() {
        use crate::types::{AudioPolicy, OpusBandwidth, OpusSignal};
        let ch = ChannelId(uuid::Uuid::nil());
        let msg = ControlMessage::ChannelAudioPolicy {
            channel_id: ch,
            audio: AudioPolicy {
                bitrate_bps: 24_000,
                min_bitrate_bps: 8_000,
                fec: true,
                dtx: false,
                max_bandwidth: OpusBandwidth::Wideband,
                complexity: Some(5),
                signal: OpusSignal::Voice,
                stereo: false,
            },
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"ChannelAudioPolicy","data":{"channel_id":"00000000-0000-0000-0000-000000000000","audio":{"bitrate_bps":24000,"min_bitrate_bps":8000,"fec":true,"dtx":false,"max_bandwidth":"wideband","complexity":5,"signal":"voice","stereo":false}}}"#
        );
        let no_hint = serde_json::to_string(&AudioPolicy::default()).unwrap();
        assert_eq!(
            no_hint,
            r#"{"bitrate_bps":48000,"min_bitrate_bps":12000,"fec":true,"dtx":true,"max_bandwidth":"fullband","complexity":null,"signal":"voice","stereo":false}"#
        );
        let partial: AudioPolicy =
            serde_json::from_str(r#"{"bitrate_bps":16000,"signal":"music"}"#).unwrap();
        assert_eq!(partial.bitrate_bps, 16_000);
        assert_eq!(partial.signal, OpusSignal::Music);
        assert_eq!(partial.max_bandwidth, OpusBandwidth::Fullband);
        assert!(partial.dtx);
        assert!(!partial.stereo);
    }
}
