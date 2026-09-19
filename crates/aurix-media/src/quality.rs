//! Server-side view of one session's uplink: sequence gaps, RFC 3550 inter-arrival jitter and
//! bitrate of the audio the client sends us. Fed from the packet router, sampled periodically
//! by the SFU quality reporter and merged with the client's own `QualityReport` into
//! [`aurix_common::types::NetworkQuality`].

use std::time::Instant;

/// Reordering deeper than this counts as a loss followed by a duplicate, not a late packet.
const MAX_REORDER: u64 = 64;
/// Sequence jumps beyond this are a stream restart (new SSRC / resumed session), not loss.
const MAX_GAP: u64 = 3000;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UplinkSample {
    /// Loss over the interval since the previous sample, `0..=100`.
    pub loss_percent: f32,
    pub jitter_ms: f32,
    pub bitrate_kbps: u32,
    pub packets_received: u64,
    pub packets_lost: u64,
}

#[derive(Debug)]
pub struct UplinkEstimator {
    highest_seq: Option<u64>,
    /// Packets counted (duplicates/late excluded) — totals and since the last sample.
    received_total: u64,
    lost_total: u64,
    received_interval: u64,
    lost_interval: u64,
    bytes_interval: u64,
    late_interval: u64,
    last_sample_at: Instant,
    last_transit: Option<(u32, Instant)>,
    jitter_ms: f32,
}

impl Default for UplinkEstimator {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl UplinkEstimator {
    pub fn new(now: Instant) -> Self {
        Self {
            highest_seq: None,
            received_total: 0,
            lost_total: 0,
            received_interval: 0,
            lost_interval: 0,
            bytes_interval: 0,
            late_interval: 0,
            last_sample_at: now,
            last_transit: None,
            jitter_ms: 0.0,
        }
    }

    /// Account for an authenticated packet with a monotonically increasing `seq` (per
    /// sender). `rtp_ts` (48 kHz) is given for audio frames only; control packets share the
    /// sequence space but carry no timing.
    pub fn record(&mut self, seq: u64, rtp_ts: Option<u32>, bytes: usize, now: Instant) {
        self.bytes_interval += bytes as u64;
        match self.highest_seq {
            None => {
                self.highest_seq = Some(seq);
                self.count_received();
            }
            Some(high) if seq > high => {
                let gap = seq - high - 1;
                if gap < MAX_GAP {
                    self.lost_total += gap;
                    self.lost_interval += gap;
                } else {
                    // Stream restart: forget the reordering history too.
                    self.last_transit = None;
                }
                self.highest_seq = Some(seq);
                self.count_received();
            }
            Some(high) if high - seq < MAX_REORDER => {
                // Late arrival of something previously declared lost.
                self.late_interval += 1;
                self.lost_total = self.lost_total.saturating_sub(1);
                self.lost_interval = self.lost_interval.saturating_sub(1);
                self.count_received();
            }
            Some(_) => {}
        }
        if let Some(ts) = rtp_ts {
            if let Some((last_ts, last_at)) = self.last_transit {
                let expected_ms = ts.wrapping_sub(last_ts) as f32 / 48.0;
                let actual_ms = now.duration_since(last_at).as_secs_f32() * 1000.0;
                if (0.0..1000.0).contains(&expected_ms) {
                    let d = (actual_ms - expected_ms).abs();
                    self.jitter_ms += (d - self.jitter_ms) / 16.0;
                }
            }
            self.last_transit = Some((ts, now));
        }
    }

    fn count_received(&mut self) {
        self.received_total += 1;
        self.received_interval += 1;
    }

    /// Close the current interval and return its figures. Jitter is the running estimate;
    /// loss and bitrate cover only the interval since the previous call.
    pub fn sample(&mut self, now: Instant) -> UplinkSample {
        let secs = now.duration_since(self.last_sample_at).as_secs_f32();
        let expected = self.received_interval + self.lost_interval;
        let sample = UplinkSample {
            loss_percent: if expected == 0 {
                0.0
            } else {
                (self.lost_interval as f32 * 100.0 / expected as f32).clamp(0.0, 100.0)
            },
            jitter_ms: if self.received_interval == 0 {
                0.0
            } else {
                self.jitter_ms
            },
            bitrate_kbps: if secs > 0.0 {
                (self.bytes_interval as f32 * 8.0 / 1000.0 / secs).round() as u32
            } else {
                0
            },
            packets_received: self.received_total,
            packets_lost: self.lost_total,
        };
        self.received_interval = 0;
        self.lost_interval = 0;
        self.bytes_interval = 0;
        self.late_interval = 0;
        self.last_sample_at = now;
        if secs > 2.0 && sample.packets_received == self.received_total && expected == 0 {
            // Silent interval: decay jitter so an idle session reads as clean.
            self.jitter_ms = 0.0;
            self.last_transit = None;
        }
        sample
    }

    pub fn totals(&self) -> (u64, u64) {
        (self.received_total, self.lost_total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn feed(est: &mut UplinkEstimator, t0: Instant, seqs: &[u64]) {
        for &seq in seqs {
            let at = t0 + Duration::from_millis(20 * seq);
            est.record(seq, Some((seq * 960) as u32), 100, at);
        }
    }

    #[test]
    fn gaps_count_as_loss_and_late_arrivals_undo_it() {
        let t0 = Instant::now();
        let mut est = UplinkEstimator::new(t0);
        feed(&mut est, t0, &[1, 2, 3, 5, 6, 7, 8, 9, 10]);
        let s = est.sample(t0 + Duration::from_millis(200));
        assert_eq!((s.packets_received, s.packets_lost), (9, 1));
        assert!((s.loss_percent - 10.0).abs() < 0.01, "{s:?}");
        assert_eq!(s.bitrate_kbps, 36); // 900 bytes over 200 ms

        feed(&mut est, t0, &[11, 4, 12]);
        let s = est.sample(t0 + Duration::from_millis(400));
        assert_eq!((s.packets_received, s.packets_lost), (12, 0));
        assert_eq!(s.loss_percent, 0.0);
    }

    #[test]
    fn restart_is_not_loss_and_control_packets_have_no_jitter() {
        let t0 = Instant::now();
        let mut est = UplinkEstimator::new(t0);
        est.record(1, Some(0), 50, t0);
        est.record(2, None, 30, t0 + Duration::from_millis(20));
        est.record(3, Some(1920), 50, t0 + Duration::from_millis(40));
        est.record(10_000, Some(0), 50, t0 + Duration::from_millis(60));
        let s = est.sample(t0 + Duration::from_millis(100));
        assert_eq!((s.packets_received, s.packets_lost), (4, 0));
        assert!(s.jitter_ms < 0.001, "{s:?}");
        assert_eq!(s.loss_percent, 0.0);
    }

    #[test]
    fn jitter_tracks_irregular_arrivals() {
        let t0 = Instant::now();
        let mut est = UplinkEstimator::new(t0);
        for i in 0..64u64 {
            let wobble = if i % 2 == 0 { 0 } else { 30 };
            est.record(
                i,
                Some((i * 960) as u32),
                100,
                t0 + Duration::from_millis(20 * i + wobble),
            );
        }
        let s = est.sample(t0 + Duration::from_secs(2));
        assert!(s.jitter_ms > 15.0 && s.jitter_ms < 35.0, "{s:?}");
        let quiet = est.sample(t0 + Duration::from_secs(5));
        assert_eq!(quiet.jitter_ms, 0.0);
        assert_eq!(quiet.bitrate_kbps, 0);
    }
}
