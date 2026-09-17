use aurix_common::error::{AurixError, Result};
use bytes::{BufMut, BytesMut};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const STUN_MAGIC_COOKIE: u32 = 0x2112A442;
pub const STUN_HEADER_SIZE: usize = 20;
pub const STUN_FINGERPRINT_XOR: u32 = 0x5354554E;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StunMessageType {
    BindingRequest,
    BindingResponse,
    BindingErrorResponse,
    AllocateRequest,
    AllocateResponse,
    AllocateErrorResponse,
    RefreshRequest,
    RefreshResponse,
    SendIndication,
    DataIndication,
    CreatePermissionRequest,
    CreatePermissionResponse,
    ChannelBindRequest,
    ChannelBindResponse,
    RefreshErrorResponse,
    CreatePermissionErrorResponse,
    ChannelBindErrorResponse,
}

impl StunMessageType {
    pub fn to_u16(self) -> u16 {
        match self {
            Self::BindingRequest => 0x0001,
            Self::BindingResponse => 0x0101,
            Self::BindingErrorResponse => 0x0111,
            Self::AllocateRequest => 0x0003,
            Self::AllocateResponse => 0x0103,
            Self::AllocateErrorResponse => 0x0113,
            Self::RefreshRequest => 0x0004,
            Self::RefreshResponse => 0x0104,
            Self::SendIndication => 0x0016,
            Self::DataIndication => 0x0017,
            Self::CreatePermissionRequest => 0x0008,
            Self::CreatePermissionResponse => 0x0108,
            Self::ChannelBindRequest => 0x0009,
            Self::ChannelBindResponse => 0x0109,
            Self::RefreshErrorResponse => 0x0114,
            Self::CreatePermissionErrorResponse => 0x0118,
            Self::ChannelBindErrorResponse => 0x0119,
        }
    }

    /// The error-response type paired with this request type.
    pub fn error_response(self) -> Self {
        match self {
            Self::BindingRequest => Self::BindingErrorResponse,
            Self::AllocateRequest => Self::AllocateErrorResponse,
            Self::RefreshRequest => Self::RefreshErrorResponse,
            Self::CreatePermissionRequest => Self::CreatePermissionErrorResponse,
            Self::ChannelBindRequest => Self::ChannelBindErrorResponse,
            other => other,
        }
    }

    /// The success-response type paired with this request type.
    pub fn success_response(self) -> Self {
        match self {
            Self::BindingRequest => Self::BindingResponse,
            Self::AllocateRequest => Self::AllocateResponse,
            Self::RefreshRequest => Self::RefreshResponse,
            Self::CreatePermissionRequest => Self::CreatePermissionResponse,
            Self::ChannelBindRequest => Self::ChannelBindResponse,
            other => other,
        }
    }

    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0x0001 => Some(Self::BindingRequest),
            0x0101 => Some(Self::BindingResponse),
            0x0111 => Some(Self::BindingErrorResponse),
            0x0003 => Some(Self::AllocateRequest),
            0x0103 => Some(Self::AllocateResponse),
            0x0113 => Some(Self::AllocateErrorResponse),
            0x0004 => Some(Self::RefreshRequest),
            0x0104 => Some(Self::RefreshResponse),
            0x0016 => Some(Self::SendIndication),
            0x0017 => Some(Self::DataIndication),
            0x0008 => Some(Self::CreatePermissionRequest),
            0x0108 => Some(Self::CreatePermissionResponse),
            0x0009 => Some(Self::ChannelBindRequest),
            0x0109 => Some(Self::ChannelBindResponse),
            0x0114 => Some(Self::RefreshErrorResponse),
            0x0118 => Some(Self::CreatePermissionErrorResponse),
            0x0119 => Some(Self::ChannelBindErrorResponse),
            _ => None,
        }
    }

    pub fn is_request(self) -> bool {
        matches!(
            self,
            Self::BindingRequest
                | Self::AllocateRequest
                | Self::RefreshRequest
                | Self::CreatePermissionRequest
                | Self::ChannelBindRequest
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StunAttributeType {
    MappedAddress,
    Username,
    MessageIntegrity,
    ErrorCode,
    UnknownAttributes,
    Realm,
    Nonce,
    XorMappedAddress,
    Software,
    Fingerprint,
    // TURN-specific
    ChannelNumber,
    Lifetime,
    XorPeerAddress,
    Data,
    XorRelayedAddress,
    RequestedTransport,
    DontFragment,
    ReservationToken,
    EvenPort,
    RequestedAddressFamily,
}

impl StunAttributeType {
    pub fn to_u16(self) -> u16 {
        match self {
            Self::MappedAddress => 0x0001,
            Self::Username => 0x0006,
            Self::MessageIntegrity => 0x0008,
            Self::ErrorCode => 0x0009,
            Self::UnknownAttributes => 0x000A,
            Self::Realm => 0x0014,
            Self::Nonce => 0x0015,
            Self::XorMappedAddress => 0x0020,
            Self::Software => 0x8022,
            Self::Fingerprint => 0x8028,
            Self::ChannelNumber => 0x000C,
            Self::Lifetime => 0x000D,
            Self::XorPeerAddress => 0x0012,
            Self::Data => 0x0013,
            Self::XorRelayedAddress => 0x0016,
            Self::RequestedTransport => 0x0019,
            Self::DontFragment => 0x001A,
            Self::ReservationToken => 0x0022,
            Self::EvenPort => 0x0018,
            Self::RequestedAddressFamily => 0x0017,
        }
    }

    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0x0001 => Some(Self::MappedAddress),
            0x0006 => Some(Self::Username),
            0x0008 => Some(Self::MessageIntegrity),
            0x0009 => Some(Self::ErrorCode),
            0x000A => Some(Self::UnknownAttributes),
            0x0014 => Some(Self::Realm),
            0x0015 => Some(Self::Nonce),
            0x0020 => Some(Self::XorMappedAddress),
            0x8022 => Some(Self::Software),
            0x8028 => Some(Self::Fingerprint),
            0x000C => Some(Self::ChannelNumber),
            0x000D => Some(Self::Lifetime),
            0x0012 => Some(Self::XorPeerAddress),
            0x0013 => Some(Self::Data),
            0x0016 => Some(Self::XorRelayedAddress),
            0x0019 => Some(Self::RequestedTransport),
            0x001A => Some(Self::DontFragment),
            0x0022 => Some(Self::ReservationToken),
            0x0018 => Some(Self::EvenPort),
            0x0017 => Some(Self::RequestedAddressFamily),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StunAttribute {
    pub attr_type: u16,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct StunMessage {
    pub msg_type: StunMessageType,
    pub transaction_id: [u8; 12],
    pub attributes: Vec<StunAttribute>,
}

impl StunMessage {
    pub fn new(msg_type: StunMessageType, transaction_id: [u8; 12]) -> Self {
        Self {
            msg_type,
            transaction_id,
            attributes: Vec::new(),
        }
    }

    pub fn add_attribute(&mut self, attr_type: StunAttributeType, value: Vec<u8>) {
        self.attributes.push(StunAttribute {
            attr_type: attr_type.to_u16(),
            value,
        });
    }

    pub fn get_attribute(&self, attr_type: StunAttributeType) -> Option<&StunAttribute> {
        let type_val = attr_type.to_u16();
        self.attributes.iter().find(|a| a.attr_type == type_val)
    }

    /// Encode an XOR-*-ADDRESS value (RFC 5389 §15.2) for this message's transaction id.
    pub fn encode_xor_address(&self, addr: SocketAddr) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20);
        buf.push(0x00);
        let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
        match addr {
            SocketAddr::V4(v4) => {
                buf.push(0x01);
                let port = v4.port() ^ (STUN_MAGIC_COOKIE >> 16) as u16;
                buf.extend_from_slice(&port.to_be_bytes());
                for (b, c) in v4.ip().octets().iter().zip(cookie.iter()) {
                    buf.push(b ^ c);
                }
            }
            SocketAddr::V6(v6) => {
                buf.push(0x02);
                let port = v6.port() ^ (STUN_MAGIC_COOKIE >> 16) as u16;
                buf.extend_from_slice(&port.to_be_bytes());
                let mut xor = [0u8; 16];
                xor[..4].copy_from_slice(&cookie);
                xor[4..].copy_from_slice(&self.transaction_id);
                for (b, x) in v6.ip().octets().iter().zip(xor.iter()) {
                    buf.push(b ^ x);
                }
            }
        }
        buf
    }

    /// Decode an XOR-*-ADDRESS value using this message's transaction id.
    pub fn decode_xor_address(&self, value: &[u8]) -> Option<SocketAddr> {
        if value.len() < 8 {
            return None;
        }
        let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
        let port = u16::from_be_bytes([value[2], value[3]]) ^ (STUN_MAGIC_COOKIE >> 16) as u16;
        match value[1] {
            0x01 => {
                let ip = Ipv4Addr::new(value[4] ^ cookie[0], value[5] ^ cookie[1], value[6] ^ cookie[2], value[7] ^ cookie[3]);
                Some(SocketAddr::new(IpAddr::V4(ip), port))
            }
            0x02 if value.len() >= 20 => {
                let mut xor = [0u8; 16];
                xor[..4].copy_from_slice(&cookie);
                xor[4..].copy_from_slice(&self.transaction_id);
                let mut octets = [0u8; 16];
                for i in 0..16 {
                    octets[i] = value[4 + i] ^ xor[i];
                }
                Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
            }
            _ => None,
        }
    }

    pub fn add_xor_address(&mut self, attr_type: StunAttributeType, addr: SocketAddr) {
        let v = self.encode_xor_address(addr);
        self.add_attribute(attr_type, v);
    }

    pub fn add_xor_mapped_address(&mut self, addr: SocketAddr) {
        self.add_xor_address(StunAttributeType::XorMappedAddress, addr);
    }

    pub fn get_xor_address(&self, attr_type: StunAttributeType) -> Option<SocketAddr> {
        self.get_attribute(attr_type).and_then(|a| self.decode_xor_address(&a.value))
    }

    /// All values of a repeated attribute (e.g. XOR-PEER-ADDRESS in CreatePermission).
    pub fn get_all_xor_addresses(&self, attr_type: StunAttributeType) -> Vec<SocketAddr> {
        let t = attr_type.to_u16();
        self.attributes.iter().filter(|a| a.attr_type == t).filter_map(|a| self.decode_xor_address(&a.value)).collect()
    }

    pub fn get_u32(&self, attr_type: StunAttributeType) -> Option<u32> {
        self.get_attribute(attr_type).and_then(|a| a.value.get(..4)).map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
    }

    pub fn get_string(&self, attr_type: StunAttributeType) -> Option<String> {
        self.get_attribute(attr_type).and_then(|a| String::from_utf8(a.value.clone()).ok())
    }

    pub fn add_error_code(&mut self, code: u16, reason: &str) {
        let class = (code / 100) as u8;
        let number = (code % 100) as u8;
        let mut buf = vec![0u8, 0u8, class, number];
        buf.extend_from_slice(reason.as_bytes());
        self.add_attribute(StunAttributeType::ErrorCode, buf);
    }

    pub fn add_software(&mut self, name: &str) {
        self.add_attribute(StunAttributeType::Software, name.as_bytes().to_vec());
    }

    fn encode_base(&self) -> BytesMut {
        let mut attrs_buf = BytesMut::new();
        for attr in &self.attributes {
            attrs_buf.put_u16(attr.attr_type);
            attrs_buf.put_u16(attr.value.len() as u16);
            attrs_buf.put_slice(&attr.value);
            let padding = (4 - (attr.value.len() % 4)) % 4;
            for _ in 0..padding {
                attrs_buf.put_u8(0x00);
            }
        }
        let mut buf = BytesMut::with_capacity(STUN_HEADER_SIZE + attrs_buf.len() + 32);
        buf.put_u16(self.msg_type.to_u16());
        buf.put_u16(attrs_buf.len() as u16);
        buf.put_u32(STUN_MAGIC_COOKIE);
        buf.put_slice(&self.transaction_id);
        buf.put_slice(&attrs_buf);
        buf
    }

    fn set_length(buf: &mut BytesMut, len: usize) {
        let len = len as u16;
        buf[2] = (len >> 8) as u8;
        buf[3] = len as u8;
    }

    fn append_fingerprint(buf: &mut BytesMut) {
        let len_with_fp = buf.len() - STUN_HEADER_SIZE + 8;
        Self::set_length(buf, len_with_fp);
        let crc = crc32fast::hash(buf) ^ STUN_FINGERPRINT_XOR;
        buf.put_u16(StunAttributeType::Fingerprint.to_u16());
        buf.put_u16(4);
        buf.put_u32(crc);
    }

    /// Encode with a trailing FINGERPRINT attribute.
    pub fn encode(&self) -> BytesMut {
        let mut buf = self.encode_base();
        Self::append_fingerprint(&mut buf);
        buf
    }

    /// Encode with MESSAGE-INTEGRITY (HMAC-SHA1 over the message with the length field covering
    /// up to and including MESSAGE-INTEGRITY, RFC 5389 §15.4) followed by FINGERPRINT.
    pub fn encode_with_integrity(&self, key: &[u8]) -> BytesMut {
        let mut buf = self.encode_base();
        let len_with_integrity = buf.len() - STUN_HEADER_SIZE + 24;
        Self::set_length(&mut buf, len_with_integrity);
        let tag = aurix_common::crypto::hmac_sha1(key, &buf);
        buf.put_u16(StunAttributeType::MessageIntegrity.to_u16());
        buf.put_u16(20);
        buf.put_slice(&tag);
        Self::append_fingerprint(&mut buf);
        buf
    }

    /// Verify MESSAGE-INTEGRITY on a raw message with the given long-term key.
    pub fn verify_integrity(raw: &[u8], key: &[u8]) -> bool {
        let Some(mi_offset) = find_attribute_offset(raw, StunAttributeType::MessageIntegrity.to_u16()) else {
            return false;
        };
        if raw.len() < mi_offset + 24 {
            return false;
        }
        let adjusted_len = (mi_offset - STUN_HEADER_SIZE + 24) as u16;
        let mut input = Vec::with_capacity(mi_offset);
        input.extend_from_slice(&raw[..2]);
        input.extend_from_slice(&adjusted_len.to_be_bytes());
        input.extend_from_slice(&raw[4..mi_offset]);
        let expected = aurix_common::crypto::hmac_sha1(key, &input);
        aurix_common::crypto::constant_time_eq(&expected, &raw[mi_offset + 4..mi_offset + 24])
    }

    /// Verify the FINGERPRINT attribute if present. Returns `true` when absent.
    pub fn verify_fingerprint(raw: &[u8]) -> bool {
        let Some(off) = find_attribute_offset(raw, StunAttributeType::Fingerprint.to_u16()) else {
            return true;
        };
        if raw.len() < off + 8 {
            return false;
        }
        let stored = u32::from_be_bytes([raw[off + 4], raw[off + 5], raw[off + 6], raw[off + 7]]);
        let mut head = raw[..off].to_vec();
        let len = (off - STUN_HEADER_SIZE + 8) as u16;
        head[2..4].copy_from_slice(&len.to_be_bytes());
        crc32fast::hash(&head) ^ STUN_FINGERPRINT_XOR == stored
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < STUN_HEADER_SIZE {
            return Err(AurixError::StunTurn("Message too short".into()));
        }

        let msg_type_raw = u16::from_be_bytes([data[0], data[1]]);
        let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
        let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

        if magic != STUN_MAGIC_COOKIE {
            return Err(AurixError::StunTurn("Invalid STUN magic cookie".into()));
        }

        let msg_type = StunMessageType::from_u16(msg_type_raw)
            .ok_or_else(|| AurixError::StunTurn(format!("Unknown message type: {msg_type_raw:#06x}")))?;

        let mut transaction_id = [0u8; 12];
        transaction_id.copy_from_slice(&data[8..20]);

        if msg_len % 4 != 0 {
            return Err(AurixError::StunTurn("Message length not 4-byte aligned".into()));
        }
        if data.len() != STUN_HEADER_SIZE + msg_len {
            return Err(AurixError::StunTurn("Message length mismatch".into()));
        }

        let mut attributes = Vec::new();
        let mut offset = STUN_HEADER_SIZE;
        let end = STUN_HEADER_SIZE + msg_len;
        let mut seen_integrity = false;

        while offset + 4 <= end {
            let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4;
            if offset + attr_len > end {
                return Err(AurixError::StunTurn("Attribute exceeds message".into()));
            }
            // Only FINGERPRINT may follow MESSAGE-INTEGRITY; anything else is not covered by
            // the HMAC and must be ignored (RFC 5389 §15.4).
            let ignored = seen_integrity && attr_type != StunAttributeType::Fingerprint.to_u16();
            if !ignored {
                attributes.push(StunAttribute { attr_type, value: data[offset..offset + attr_len].to_vec() });
            }
            if attr_type == StunAttributeType::MessageIntegrity.to_u16() {
                seen_integrity = true;
            }
            offset += attr_len + (4 - (attr_len % 4)) % 4;
        }

        Ok(Self {
            msg_type,
            transaction_id,
            attributes,
        })
    }

    pub fn is_stun(data: &[u8]) -> bool {
        Self::stun_frame_len(data).is_some()
    }

    /// Total framed length of a STUN message starting at `data[0]`, if the header is valid.
    pub fn stun_frame_len(data: &[u8]) -> Option<usize> {
        if data.len() < STUN_HEADER_SIZE {
            return None;
        }
        if data[0] & 0xC0 != 0x00 {
            return None;
        }
        let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if magic != STUN_MAGIC_COOKIE {
            return None;
        }
        Some(STUN_HEADER_SIZE + u16::from_be_bytes([data[2], data[3]]) as usize)
    }
}

/// Byte offset of the first attribute of type `wanted` in a raw STUN message.
pub fn find_attribute_offset(raw: &[u8], wanted: u16) -> Option<usize> {
    let mut i = STUN_HEADER_SIZE;
    while i + 4 <= raw.len() {
        let at = u16::from_be_bytes([raw[i], raw[i + 1]]);
        let al = u16::from_be_bytes([raw[i + 2], raw[i + 3]]) as usize;
        if at == wanted {
            return Some(i);
        }
        i += 4 + al + (4 - (al % 4)) % 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_roundtrip_and_tamper() {
        let mut m = StunMessage::new(StunMessageType::AllocateRequest, [9u8; 12]);
        m.add_attribute(StunAttributeType::Username, b"1700000000:alice".to_vec());
        m.add_attribute(StunAttributeType::Realm, b"aurix".to_vec());
        let key = aurix_common::crypto::stun_long_term_key("1700000000:alice", "aurix", "pw");
        let raw = m.encode_with_integrity(&key);
        assert!(StunMessage::verify_integrity(&raw, &key));
        assert!(StunMessage::verify_fingerprint(&raw));
        assert!(!StunMessage::verify_integrity(&raw, b"wrong"));
        let mut tampered = raw.to_vec();
        tampered[30] ^= 0xFF;
        assert!(!StunMessage::verify_integrity(&tampered, &key));
        let decoded = StunMessage::decode(&raw).unwrap();
        assert!(decoded.get_attribute(StunAttributeType::MessageIntegrity).is_some());
        assert_eq!(decoded.get_string(StunAttributeType::Realm).unwrap(), "aurix");
    }

    #[test]
    fn xor_address_roundtrip_v4_v6() {
        let m = StunMessage::new(StunMessageType::BindingRequest, [3u8; 12]);
        for addr in ["192.0.2.1:3478", "[2001:db8::1]:65000"] {
            let a: SocketAddr = addr.parse().unwrap();
            let enc = m.encode_xor_address(a);
            assert_eq!(m.decode_xor_address(&enc), Some(a));
        }
    }

    #[test]
    fn decode_rejects_bad_lengths() {
        let m = StunMessage::new(StunMessageType::BindingRequest, [0u8; 12]).encode();
        assert!(StunMessage::decode(&m[..m.len() - 1]).is_err());
        let mut extra = m.to_vec();
        extra.push(0);
        assert!(StunMessage::decode(&extra).is_err());
    }
}