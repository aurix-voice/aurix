use std::collections::BTreeMap;
use tracing::warn;

/// Server-side reordering jitter buffer for the recording pipeline.
/// Holds packets indexed by RTP sequence number and drains them
/// in order once the window is full or a gap timeout triggers.
pub struct RecordingJitterBuffer {
    /// seq → opus data
    buffer: BTreeMap<u32, Vec<u8>>,
    /// Last sequence number written to the Ogg file
    last_written_seq: Option<u32>,
    /// Maximum number of packets to hold before force-flushing
    max_depth: u32,
    /// Duplicate tracker (circular window)
    seen_seqs: Vec<u32>,
    seen_head: usize,
}

impl RecordingJitterBuffer {
    pub fn new(max_depth: u32) -> Self {
        Self {
            buffer: BTreeMap::new(),
            last_written_seq: None,
            max_depth,
            seen_seqs: Vec::with_capacity(max_depth as usize * 2),
            seen_head: 0,
        }
    }

    /// Insert a packet. Returns `true` if it was accepted (not a duplicate).
    pub fn insert(&mut self, seq: u32, opus_data: Vec<u8>) -> bool {
        // Duplicate check
        if self.seen_seqs.contains(&seq) {
            return false;
        }
        // Track in circular seen buffer
        if self.seen_seqs.len() < self.max_depth as usize * 2 {
            self.seen_seqs.push(seq);
        } else {
            self.seen_seqs[self.seen_head] = seq;
            self.seen_head = (self.seen_head + 1) % self.seen_seqs.len();
        }

        // Skip packets that are older than what we've already written
        if let Some(last) = self.last_written_seq {
            if Self::seq_before(seq, last) {
                return false; // Too late, already written past this point
            }
        }

        self.buffer.insert(seq, opus_data);
        true
    }

    /// Drain all packets that are ready to be written in order.
    /// Returns them in sequence order. Also force-flushes if the buffer
    /// exceeds max_depth to prevent unbounded memory growth.
    pub fn drain_ready(&mut self) -> Vec<(u32, Vec<u8>)> {
        let mut result = Vec::new();

        if self.buffer.is_empty() {
            return result;
        }

        // Strategy: drain contiguous packets starting from the lowest seq
        // If we have no reference point yet, start from the smallest seq in buffer
        let start_seq = match self.last_written_seq {
            Some(last) => last.wrapping_add(1),
            None => {
                // First drain: use the smallest seq
                match self.buffer.keys().next() {
                    Some(&first) => first,
                    None => return result,
                }
            }
        };

        // Drain contiguous run
        let mut expected = start_seq;
        loop {
            if let Some(data) = self.buffer.remove(&expected) {
                result.push((expected, data));
                self.last_written_seq = Some(expected);
                expected = expected.wrapping_add(1);
            } else {
                break;
            }
        }

        // Force-flush if buffer is too large (gap that will never be filled)
        if self.buffer.len() as u32 > self.max_depth {
            let excess: Vec<u32> = self.buffer.keys().copied().collect();
            for seq in excess {
                if let Some(data) = self.buffer.remove(&seq) {
                    if result.is_empty() || seq > result.last().unwrap().0 {
                        result.push((seq, data));
                        self.last_written_seq = Some(seq);
                    }
                }
            }
            if result.len() > 1 {
                warn!("Jitter buffer force-flushed {} packets (gap detected)", result.len());
            }
        }

        result
    }

    pub fn pending_count(&self) -> usize {
        self.buffer.len()
    }

    /// RFC 3550 sequence comparison: returns true if `a` is before `b`
    /// accounting for 32-bit wrap-around.
    fn seq_before(a: u32, b: u32) -> bool {
        let diff = b.wrapping_sub(a);
        diff > 0 && diff < 0x8000_0000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_order() {
        let mut jb = RecordingJitterBuffer::new(10);
        jb.insert(1, vec![0x01]);
        jb.insert(2, vec![0x02]);
        jb.insert(3, vec![0x03]);
        let drained = jb.drain_ready();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].0, 1);
        assert_eq!(drained[2].0, 3);
    }

    #[test]
    fn test_out_of_order() {
        let mut jb = RecordingJitterBuffer::new(10);
        jb.insert(3, vec![0x03]);
        jb.insert(1, vec![0x01]);
        jb.insert(2, vec![0x02]);
        let drained = jb.drain_ready();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].0, 1);
        assert_eq!(drained[1].0, 2);
        assert_eq!(drained[2].0, 3);
    }

    #[test]
    fn test_duplicate_rejected() {
        let mut jb = RecordingJitterBuffer::new(10);
        assert!(jb.insert(1, vec![0x01]));
        assert!(!jb.insert(1, vec![0x01])); // duplicate
    }

    #[test]
    fn test_force_flush() {
        let mut jb = RecordingJitterBuffer::new(3);
        // Insert with a gap: 1, 2, skip 3, 4, 5, 6
        jb.insert(1, vec![0x01]);
        jb.insert(2, vec![0x02]);
        let d = jb.drain_ready();
        assert_eq!(d.len(), 2);
        // Now insert 4,5,6,7 (skipping 3) — exceeds max_depth
        jb.insert(4, vec![0x04]);
        jb.insert(5, vec![0x05]);
        jb.insert(6, vec![0x06]);
        jb.insert(7, vec![0x07]);
        let d = jb.drain_ready();
        assert!(!d.is_empty()); // force-flushed
    }
}