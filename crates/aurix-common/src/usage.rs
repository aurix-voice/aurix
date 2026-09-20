//! Tenant usage metering: per-node counters that the control plane flushes into
//! time-bucketed rows (`usage_counters`). Hot paths call [`UsageMeter::record`] with a
//! metric and a delta; the meter keys the delta by application, optional channel and the
//! bucket the wall clock falls into, so a flush can arrive late without moving usage
//! between buckets.

use crate::types::{AppId, ChannelId};
use chrono::{DateTime, TimeZone, Utc};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Width of a metered bucket (seconds). Application series are stored at this
/// resolution; channel series at the hourly one.
pub const APP_BUCKET_SECS: i64 = 300;
pub const CHANNEL_BUCKET_SECS: i64 = 3600;

/// Counters a node meters itself (as opposed to the session/membership intervals the
/// aggregator derives from the database).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UsageMetric {
    /// Authenticated media bytes received from clients (AURX/UDP, tunnel, WebRTC RTP).
    MediaBytesIn,
    /// Media bytes sent to clients.
    MediaBytesOut,
    /// Chat messages accepted (channel and direct).
    ChatMessages,
    /// Text-to-speech requests accepted.
    TtsRequests,
    /// Characters submitted to the TTS provider.
    TtsCharacters,
    /// Milliseconds of audio submitted to the STT provider.
    SttAudioMs,
}

impl UsageMetric {
    pub const ALL: [UsageMetric; 6] = [
        UsageMetric::MediaBytesIn,
        UsageMetric::MediaBytesOut,
        UsageMetric::ChatMessages,
        UsageMetric::TtsRequests,
        UsageMetric::TtsCharacters,
        UsageMetric::SttAudioMs,
    ];

    /// Column/wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            UsageMetric::MediaBytesIn => "media_bytes_in",
            UsageMetric::MediaBytesOut => "media_bytes_out",
            UsageMetric::ChatMessages => "chat_messages",
            UsageMetric::TtsRequests => "tts_requests",
            UsageMetric::TtsCharacters => "tts_characters",
            UsageMetric::SttAudioMs => "stt_audio_ms",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

/// Start of the bucket of width `secs` that contains `at`.
pub fn bucket_start(at: DateTime<Utc>, secs: i64) -> DateTime<Utc> {
    let ts = at.timestamp();
    let start = ts - ts.rem_euclid(secs.max(1));
    Utc.timestamp_opt(start, 0).single().unwrap_or(at)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UsageKey {
    pub app_id: AppId,
    pub channel_id: Option<ChannelId>,
    pub bucket: DateTime<Utc>,
    pub metric: UsageMetric,
}

/// One flushed delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageDelta {
    pub key: UsageKey,
    pub value: u64,
}

/// Lock-free accumulator shared by the media, chat, speech and recording paths. Cheap
/// enough for per-message calls; the media path batches bytes per session and records
/// them on a timer instead of per packet.
#[derive(Default)]
pub struct UsageMeter {
    counters: DashMap<UsageKey, AtomicU64>,
}

impl UsageMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `value` to `metric` for the application (and channel when known) in the bucket
    /// the current time falls into. A channel-scoped call also counts at application level
    /// at flush time, so callers record once.
    pub fn record(
        &self,
        app_id: AppId,
        channel_id: Option<ChannelId>,
        metric: UsageMetric,
        value: u64,
    ) {
        if value == 0 {
            return;
        }
        self.record_at(app_id, channel_id, metric, value, Utc::now());
    }

    pub fn record_at(
        &self,
        app_id: AppId,
        channel_id: Option<ChannelId>,
        metric: UsageMetric,
        value: u64,
        at: DateTime<Utc>,
    ) {
        if value == 0 {
            return;
        }
        let key = UsageKey {
            app_id,
            channel_id,
            bucket: bucket_start(at, APP_BUCKET_SECS),
            metric,
        };
        if let Some(counter) = self.counters.get(&key) {
            counter.fetch_add(value, Ordering::Relaxed);
            return;
        }
        self.counters
            .entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(value, Ordering::Relaxed);
    }

    /// Takes every non-zero counter, leaving the meter empty. Deltas are independent, so a
    /// failed flush can simply be re-recorded with [`UsageMeter::restore`].
    pub fn drain(&self) -> Vec<UsageDelta> {
        let keys: Vec<UsageKey> = self.counters.iter().map(|e| *e.key()).collect();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some((_, counter)) = self.counters.remove(&key) {
                let value = counter.load(Ordering::Relaxed);
                if value > 0 {
                    out.push(UsageDelta { key, value });
                }
            }
        }
        out
    }

    /// Puts drained deltas back (after a failed flush) so they are retried next time.
    pub fn restore(&self, deltas: &[UsageDelta]) {
        for d in deltas {
            self.counters
                .entry(d.key)
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(d.value, Ordering::Relaxed);
        }
    }

    pub fn pending(&self) -> usize {
        self.counters.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_aligned_to_their_width() {
        let at = Utc.with_ymd_and_hms(2026, 3, 4, 10, 17, 43).unwrap();
        assert_eq!(
            bucket_start(at, APP_BUCKET_SECS),
            Utc.with_ymd_and_hms(2026, 3, 4, 10, 15, 0).unwrap()
        );
        assert_eq!(
            bucket_start(at, CHANNEL_BUCKET_SECS),
            Utc.with_ymd_and_hms(2026, 3, 4, 10, 0, 0).unwrap()
        );
        assert_eq!(
            bucket_start(at, 86_400),
            Utc.with_ymd_and_hms(2026, 3, 4, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn meter_accumulates_per_key_and_drains_once() {
        let meter = UsageMeter::new();
        let app = AppId::new();
        let ch = ChannelId::new();
        let at = Utc.with_ymd_and_hms(2026, 3, 4, 10, 17, 43).unwrap();
        meter.record_at(app, Some(ch), UsageMetric::ChatMessages, 1, at);
        meter.record_at(app, Some(ch), UsageMetric::ChatMessages, 2, at);
        meter.record_at(app, None, UsageMetric::MediaBytesIn, 500, at);
        meter.record_at(app, None, UsageMetric::MediaBytesIn, 0, at);
        let mut drained = meter.drain();
        drained.sort_by_key(|d| d.key.metric);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].key.metric, UsageMetric::MediaBytesIn);
        assert_eq!(drained[0].value, 500);
        assert_eq!(drained[0].key.channel_id, None);
        assert_eq!(drained[1].key.metric, UsageMetric::ChatMessages);
        assert_eq!(drained[1].value, 3);
        assert_eq!(drained[1].key.channel_id, Some(ch));
        assert_eq!(drained[1].key.bucket, bucket_start(at, APP_BUCKET_SECS));
        assert!(meter.drain().is_empty());
        meter.restore(&drained);
        assert_eq!(meter.pending(), 2);
    }

    #[test]
    fn metric_names_round_trip() {
        for m in UsageMetric::ALL {
            assert_eq!(UsageMetric::parse(m.as_str()), Some(m));
        }
        assert_eq!(UsageMetric::parse("bogus"), None);
    }
}
