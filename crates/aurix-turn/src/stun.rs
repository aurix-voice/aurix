use bytes::{BufMut, BytesMut};
use aurix_common::error::{AurixError, Result};
use std::net::SocketAddr;

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

    pub fn add_xor_mapped_address(&mut self, addr: SocketAddr) {
        let mut buf = Vec::new();
        buf.push(0x00);
        match addr {
            SocketAddr::V4(v4) => {
                buf.push(0x01);
                let port = v4.port() ^ (STUN_MAGIC_COOKIE >> 16) as u16;
                buf.extend_from_slice(&port.to_be_bytes());
                let ip_bytes = v4.ip().octets();
                let cookie_bytes = STUN_MAGIC_COOKIE.to_be_bytes();
                for i in 0..4 {
                    buf.push(ip_bytes[i] ^ cookie_bytes[i]);
                }
            }
            SocketAddr::V6(v6) => {
                buf.push(0x02);
                let port = v6.port() ^ (STUN_MAGIC_COOKIE >> 16) as u16;
                buf.extend_from_slice(&port.to_be_bytes());
                let ip_bytes = v6.ip().octets();
                let mut xor_bytes = STUN_MAGIC_COOKIE.to_be_bytes().to_vec();
                xor_bytes.extend_from_slice(&self.transaction_id);
                for i in 0..16 {
                    buf.push(ip_bytes[i] ^ xor_bytes[i]);
                }
            }
        }
        self.add_attribute(StunAttributeType::XorMappedAddress, buf);
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

    pub fn encode(&self) -> BytesMut {
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

        let mut buf = BytesMut::with_capacity(STUN_HEADER_SIZE + attrs_buf.len());
        buf.put_u16(self.msg_type.to_u16());
        buf.put_u16(attrs_buf.len() as u16);
        buf.put_u32(STUN_MAGIC_COOKIE);
        buf.put_slice(&self.transaction_id);
        buf.put_slice(&attrs_buf);

        // Add fingerprint
        let crc = crc32fast::hash(&buf) ^ STUN_FINGERPRINT_XOR;
        let fp_type = StunAttributeType::Fingerprint.to_u16();
        // Update length to include fingerprint
        let new_len = (buf.len() - STUN_HEADER_SIZE + 8) as u16;
        buf[2] = (new_len >> 8) as u8;
        buf[3] = new_len as u8;
        buf.put_u16(fp_type);
        buf.put_u16(4);
        buf.put_u32(crc);

        buf
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

        if data.len() < STUN_HEADER_SIZE + msg_len {
            return Err(AurixError::StunTurn("Message truncated".into()));
        }

        let mut attributes = Vec::new();
        let mut offset = STUN_HEADER_SIZE;

        while offset + 4 <= STUN_HEADER_SIZE + msg_len {
            let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4;

            if offset + attr_len > data.len() {
                break;
            }

            let value = data[offset..offset + attr_len].to_vec();
            attributes.push(StunAttribute { attr_type, value });

            offset += attr_len;
            let padding = (4 - (attr_len % 4)) % 4;
            offset += padding;
        }

        Ok(Self {
            msg_type,
            transaction_id,
            attributes,
        })
    }

    pub fn is_stun(data: &[u8]) -> bool {
        if data.len() < STUN_HEADER_SIZE {
            return false;
        }
        let first_byte = data[0];
        if first_byte & 0xC0 != 0x00 {
            return false;
        }
        let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        magic == STUN_MAGIC_COOKIE
    }
}