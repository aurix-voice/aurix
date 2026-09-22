//! Length-prefixed framing of sealed AURX packets on byte streams (the dedicated TLS media
//! tunnel). One frame is `u16 big-endian length | packet`, where `1 ≤ length ≤
//! MAX_PACKET_SIZE`. The decoder is synchronous and allocation-bounded so the same code
//! sits behind a tokio stream on the node and a blocking or async stream in clients.
//!
//! Everything inside a frame is a sealed AURX packet: the stream's TLS layer only hides the
//! packets from on-path observers, ownership and integrity come from the AURX layer exactly
//! as on UDP (`SessionBind` first, per-session HMAC/AEAD, replay window).

use crate::protocol::MAX_PACKET_SIZE;

/// Bytes of the length prefix.
pub const FRAME_PREFIX: usize = 2;
/// Largest packet a frame may carry.
pub const MAX_FRAME_PAYLOAD: usize = MAX_PACKET_SIZE;
/// Largest encoded frame (`FRAME_PREFIX + MAX_FRAME_PAYLOAD`).
pub const MAX_FRAME: usize = FRAME_PREFIX + MAX_FRAME_PAYLOAD;

/// Why a frame was refused; the stream must be closed, the peer is not speaking the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Length prefix of zero.
    Empty,
    /// Length prefix above [`MAX_FRAME_PAYLOAD`].
    Oversized(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Empty => write!(f, "empty frame"),
            FrameError::Oversized(len) => {
                write!(f, "frame of {len} bytes exceeds {MAX_FRAME_PAYLOAD}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Encodes one packet as a frame. `None` when the packet cannot be framed (empty or larger
/// than [`MAX_FRAME_PAYLOAD`]).
pub fn encode_frame(packet: &[u8]) -> Option<Vec<u8>> {
    if packet.is_empty() || packet.len() > MAX_FRAME_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(FRAME_PREFIX + packet.len());
    out.extend_from_slice(&(packet.len() as u16).to_be_bytes());
    out.extend_from_slice(packet);
    Some(out)
}

/// Incremental frame decoder: feed whatever the stream delivered, pull complete packets.
/// Never buffers more than one maximal frame beyond what [`push`](Self::push) received.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    read: usize,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends stream bytes. Compacts the buffer once the consumed prefix outgrows a frame.
    pub fn push(&mut self, data: &[u8]) {
        if self.read > 0 && (self.read >= MAX_FRAME || self.read == self.buf.len()) {
            self.buf.drain(..self.read);
            self.read = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// Bytes received but not yet returned as complete frames.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.read
    }

    /// The next complete packet, `Ok(None)` when more bytes are needed. After an `Err` the
    /// decoder state is unspecified and the stream must be dropped.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let rest = &self.buf[self.read..];
        if rest.len() < FRAME_PREFIX {
            return Ok(None);
        }
        let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
        if len == 0 {
            return Err(FrameError::Empty);
        }
        if len > MAX_FRAME_PAYLOAD {
            return Err(FrameError::Oversized(len));
        }
        if rest.len() < FRAME_PREFIX + len {
            return Ok(None);
        }
        let packet = rest[FRAME_PREFIX..FRAME_PREFIX + len].to_vec();
        self.read += FRAME_PREFIX + len;
        Ok(Some(packet))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_in_arbitrary_chunks() {
        let a = vec![1u8; 30];
        let b = vec![2u8; MAX_FRAME_PAYLOAD];
        let c = vec![3u8; 1];
        let mut stream = encode_frame(&a).unwrap();
        stream.extend(encode_frame(&b).unwrap());
        stream.extend(encode_frame(&c).unwrap());
        for chunk in [1usize, 2, 3, 7, 64, 1000, stream.len()] {
            let mut dec = FrameDecoder::new();
            let mut out = Vec::new();
            for piece in stream.chunks(chunk) {
                dec.push(piece);
                while let Some(p) = dec.next_frame().unwrap() {
                    out.push(p);
                }
            }
            assert_eq!(out, vec![a.clone(), b.clone(), c.clone()], "chunk {chunk}");
            assert_eq!(dec.pending(), 0);
        }
    }

    #[test]
    fn bad_lengths_are_rejected_and_partials_wait() {
        assert!(encode_frame(&[]).is_none());
        assert!(encode_frame(&vec![0u8; MAX_FRAME_PAYLOAD + 1]).is_none());

        let mut dec = FrameDecoder::new();
        dec.push(&[0, 0]);
        assert_eq!(dec.next_frame(), Err(FrameError::Empty));

        let mut dec = FrameDecoder::new();
        dec.push(&((MAX_FRAME_PAYLOAD as u16 + 1).to_be_bytes()));
        assert_eq!(
            dec.next_frame(),
            Err(FrameError::Oversized(MAX_FRAME_PAYLOAD + 1))
        );

        let mut dec = FrameDecoder::new();
        dec.push(&[0xff]);
        assert_eq!(dec.next_frame(), Ok(None));
        dec.push(&[0xff]);
        assert_eq!(dec.next_frame(), Err(FrameError::Oversized(0xffff)));

        let mut dec = FrameDecoder::new();
        dec.push(&[0, 5, 1, 2]);
        assert_eq!(dec.next_frame(), Ok(None));
        assert_eq!(dec.pending(), 4);
        dec.push(&[3, 4, 5]);
        assert_eq!(dec.next_frame(), Ok(Some(vec![1, 2, 3, 4, 5])));
        assert_eq!(dec.next_frame(), Ok(None));
    }

    #[test]
    fn buffer_is_compacted_after_a_full_frame_of_consumed_bytes() {
        let mut dec = FrameDecoder::new();
        let frame = encode_frame(&vec![9u8; MAX_FRAME_PAYLOAD]).unwrap();
        for _ in 0..3 {
            dec.push(&frame);
            assert!(dec.next_frame().unwrap().is_some());
            assert!(dec.buf.len() <= 2 * MAX_FRAME);
        }
    }
}
