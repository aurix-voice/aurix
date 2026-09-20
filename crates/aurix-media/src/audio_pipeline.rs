use aurix_common::error::{AurixError, Result};
use aurix_common::tts_stt::{ContentAnalyzer, ContentViolation, SttProvider, TranscriptResult};
use aurix_common::types::{AppId, ChannelId, UserId};
use aurix_common::usage::{UsageMeter, UsageMetric};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::channel::MediaChannel;

/// Largest Opus frame (120 ms @ 48 kHz mono).
const MAX_DECODE_SAMPLES: usize = 5760;

/// Per-(user, channel) PCM accumulation buffer.
struct UserAudioBuffer {
    pcm: Vec<i16>,
    /// Server time of the first sample in `pcm`.
    started_at: DateTime<Utc>,
    last_packet: Instant,
}

/// Tuning for [`AudioAnalysisPipeline`].
#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub sample_rate: u32,
    /// Audio accumulated per speaker before a segment is transcribed.
    pub segment: Duration,
    /// Flush a shorter segment after this much silence (`None` = only on `segment`).
    pub silence_flush: Option<Duration>,
    /// Segments shorter than this are discarded.
    pub min_segment: Duration,
    /// STT requests in flight per node; extra segments are dropped.
    pub max_concurrent_stt: usize,
}

impl PipelineOptions {
    pub fn new(sample_rate: u32, segment_secs: f32) -> Self {
        Self {
            sample_rate,
            segment: Duration::from_secs_f32(segment_secs.max(0.1)),
            silence_flush: Some(Duration::from_millis(700)),
            min_segment: Duration::from_millis(400),
            max_concurrent_stt: 8,
        }
    }

    fn samples(&self, d: Duration) -> usize {
        (d.as_secs_f64() * f64::from(self.sample_rate)) as usize
    }
}

/// One transcribed segment with the tenant/channel it belongs to.
#[derive(Debug, Clone)]
pub struct TranscriptSegment {
    pub app_id: AppId,
    pub channel_id: ChannelId,
    pub user_id: UserId,
    /// Server time of the first sample of the segment.
    pub started_at: DateTime<Utc>,
    /// Length of the audio that was sent to the provider.
    pub audio_ms: u64,
    pub result: TranscriptResult,
    /// The decoded mono audio the transcript was produced from, at `sample_rate`.
    pub pcm: Arc<Vec<i16>>,
    pub sample_rate: u32,
    /// Channel has `transcription: true` — deliver the transcript to participants.
    pub deliver: bool,
    /// Channel has `safety_voice: true` — hand the transcript to the safety pipeline.
    pub safety: bool,
}

/// Pipeline that intercepts Opus packets from the SFU router, decodes them to PCM, accumulates
/// a window per speaker and dispatches to the STT provider (channels with
/// `transcription: true` or `safety_voice: true`) and content analyzers (every channel).
pub struct AudioAnalysisPipeline {
    buffers: Mutex<HashMap<(UserId, ChannelId), UserAudioBuffer>>,
    decoders: Mutex<HashMap<(UserId, ChannelId), opus::Decoder>>,
    /// Language a speaker declared for their own speech; fills `TranscriptResult.language`
    /// when the provider does not detect one.
    spoken_languages: Mutex<HashMap<UserId, String>>,
    stt_provider: Option<Arc<dyn SttProvider>>,
    content_analyzers: Vec<Arc<dyn ContentAnalyzer>>,
    options: PipelineOptions,
    segment_samples: usize,
    min_samples: usize,
    stt_permits: Arc<Semaphore>,
    stt_callback: Option<TranscriptCallback>,
    violation_callback: Option<ViolationCallback>,
    /// Without a safety consumer, `safety_voice` alone does not trigger transcription.
    safety_enabled: bool,
    usage: Option<Arc<UsageMeter>>,
}

pub type TranscriptCallback = Arc<dyn Fn(TranscriptSegment) + Send + Sync>;
pub type ViolationCallback = Arc<dyn Fn(UserId, ChannelId, Vec<ContentViolation>) + Send + Sync>;

impl AudioAnalysisPipeline {
    pub fn new(
        options: PipelineOptions,
        stt_provider: Option<Arc<dyn SttProvider>>,
        content_analyzers: Vec<Arc<dyn ContentAnalyzer>>,
    ) -> Self {
        let segment_samples = options.samples(options.segment).max(1);
        let min_samples = options.samples(options.min_segment).min(segment_samples);
        Self {
            buffers: Mutex::new(HashMap::new()),
            decoders: Mutex::new(HashMap::new()),
            spoken_languages: Mutex::new(HashMap::new()),
            stt_provider,
            content_analyzers,
            stt_permits: Arc::new(Semaphore::new(options.max_concurrent_stt.max(1))),
            options,
            segment_samples,
            min_samples,
            stt_callback: None,
            violation_callback: None,
            safety_enabled: false,
            usage: None,
        }
    }

    /// Counts audio milliseconds sent to the STT provider into the usage meter.
    pub fn set_usage_meter(&mut self, meter: Arc<UsageMeter>) {
        self.usage = Some(meter);
    }

    /// Transcribe channels with `safety_voice: true` too (the transcript callback receives
    /// segments with `safety = true`).
    pub fn set_safety_enabled(&mut self, enabled: bool) {
        self.safety_enabled = enabled;
    }

    pub fn set_spoken_language(&self, user_id: UserId, language: Option<String>) {
        let mut langs = self.spoken_languages.lock();
        match language {
            Some(lang) => {
                langs.insert(user_id, lang);
            }
            None => {
                langs.remove(&user_id);
            }
        }
    }

    fn transcribes(&self, channel: &MediaChannel) -> bool {
        let cfg = channel.config();
        self.stt_provider.is_some()
            && (cfg.transcription || (self.safety_enabled && cfg.safety_voice))
    }

    pub fn set_stt_callback<F>(&mut self, f: F)
    where
        F: Fn(TranscriptSegment) + Send + Sync + 'static,
    {
        self.stt_callback = Some(Arc::new(f));
    }

    pub fn set_violation_callback<F>(&mut self, f: F)
    where
        F: Fn(UserId, ChannelId, Vec<ContentViolation>) + Send + Sync + 'static,
    {
        self.violation_callback = Some(Arc::new(f));
    }

    /// True when packets in `channel` are worth decoding at all.
    pub fn wants_channel(&self, channel: &MediaChannel) -> bool {
        !self.content_analyzers.is_empty() || self.transcribes(channel)
    }

    /// Feed a raw Opus packet from the router. Decodes to PCM with a stateful per-speaker
    /// decoder, accumulates, and dispatches when the segment is full.
    pub fn process_opus_packet(&self, channel: &MediaChannel, user_id: UserId, opus_data: &[u8]) {
        if !self.wants_channel(channel) {
            return;
        }
        let channel_id = channel.channel_id;
        let key = (user_id, channel_id);
        let pcm_samples = match self.decode_opus_frame(key, opus_data) {
            Ok(samples) => samples,
            Err(e) => {
                warn!("Opus decode failed (analysis skipped): {}", e);
                return;
            }
        };
        if pcm_samples.is_empty() {
            return;
        }

        let full = {
            let mut buffers = self.buffers.lock();
            let buf = buffers.entry(key).or_insert_with(|| UserAudioBuffer {
                pcm: Vec::with_capacity(self.segment_samples),
                started_at: Utc::now(),
                last_packet: Instant::now(),
            });
            if buf.pcm.is_empty() {
                buf.started_at = Utc::now();
            }
            buf.pcm.extend_from_slice(&pcm_samples);
            buf.last_packet = Instant::now();
            if buf.pcm.len() >= self.segment_samples {
                Some((std::mem::take(&mut buf.pcm), buf.started_at))
            } else {
                None
            }
        };
        if let Some((pcm, started_at)) = full {
            self.dispatch_analysis(channel, user_id, started_at, pcm);
        }
    }

    /// Dispatch buffers whose speaker fell silent (`options.silence_flush`). Call periodically
    /// with the channels currently hosted on the node.
    pub fn flush_idle(&self, channels: &dashmap::DashMap<ChannelId, Arc<MediaChannel>>) {
        let Some(silence) = self.options.silence_flush else {
            return;
        };
        let now = Instant::now();
        let mut ready = Vec::new();
        {
            let mut buffers = self.buffers.lock();
            for ((user_id, channel_id), buf) in buffers.iter_mut() {
                if buf.pcm.is_empty() || now.duration_since(buf.last_packet) < silence {
                    continue;
                }
                let pcm = std::mem::take(&mut buf.pcm);
                if pcm.len() >= self.min_samples {
                    ready.push((*user_id, *channel_id, buf.started_at, pcm));
                }
            }
        }
        for (user_id, channel_id, started_at, pcm) in ready {
            if let Some(channel) = channels.get(&channel_id) {
                self.dispatch_analysis(channel.value(), user_id, started_at, pcm);
            }
        }
    }

    /// Flush what a participant said so far and forget their decoder state for the channel.
    pub fn participant_left(&self, channel: &MediaChannel, user_id: UserId) {
        let key = (user_id, channel.channel_id);
        let tail = self.buffers.lock().remove(&key);
        self.decoders.lock().remove(&key);
        if let Some(buf) = tail {
            if buf.pcm.len() >= self.min_samples {
                self.dispatch_analysis(channel, user_id, buf.started_at, buf.pcm);
            }
        }
    }

    /// Spawn the periodic silence flusher; stops when the returned handle is aborted.
    pub fn spawn_idle_flusher(
        self: &Arc<Self>,
        channels: Arc<dashmap::DashMap<ChannelId, Arc<MediaChannel>>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let silence = self.options.silence_flush?;
        let pipeline = Arc::clone(self);
        let tick = (silence / 4).clamp(Duration::from_millis(50), Duration::from_millis(500));
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                pipeline.flush_idle(&channels);
            }
        }))
    }

    fn dispatch_analysis(
        &self,
        channel: &MediaChannel,
        user_id: UserId,
        started_at: DateTime<Utc>,
        pcm: Vec<i16>,
    ) {
        let channel_id = channel.channel_id;
        let sr = self.options.sample_rate;
        let pcm: Arc<Vec<i16>> = Arc::new(pcm);
        if let Some(stt) = self
            .stt_provider
            .as_ref()
            .filter(|_| self.transcribes(channel))
        {
            let deliver = channel.config().transcription;
            let safety = self.safety_enabled && channel.config().safety_voice;
            let spoken = self.spoken_languages.lock().get(&user_id).cloned();
            match Arc::clone(&self.stt_permits).try_acquire_owned() {
                Ok(permit) => {
                    let stt = stt.clone();
                    let cb = self.stt_callback.clone();
                    let app_id = channel.app_id;
                    let pcm = Arc::clone(&pcm);
                    let usage = self.usage.clone();
                    tokio::spawn(async move {
                        let audio_ms = pcm.len() as u64 * 1000 / u64::from(sr.max(1));
                        if let Some(meter) = usage {
                            meter.record(
                                app_id,
                                Some(channel_id),
                                UsageMetric::SttAudioMs,
                                audio_ms,
                            );
                        }
                        let outcome = stt.transcribe(&pcm, sr).await;
                        drop(permit);
                        match outcome {
                            Ok(mut result) => {
                                if result.language.is_empty() {
                                    if let Some(lang) = spoken {
                                        result.language = lang;
                                    }
                                }
                                if !result.text.trim().is_empty() {
                                    if let Some(ref cb) = cb {
                                        cb(TranscriptSegment {
                                            app_id,
                                            channel_id,
                                            user_id,
                                            started_at,
                                            audio_ms,
                                            result,
                                            pcm,
                                            sample_rate: sr,
                                            deliver,
                                            safety,
                                        });
                                    }
                                }
                            }
                            Err(e) => warn!("STT failed for {}: {}", user_id, e),
                        }
                    });
                }
                Err(_) => {
                    debug!(
                        "STT saturated ({} in flight), dropping {}ms segment of {}",
                        self.options.max_concurrent_stt,
                        pcm.len() as u64 * 1000 / u64::from(sr.max(1)),
                        user_id
                    );
                }
            }
        }
        self.dispatch_content_analysis(user_id, channel_id, pcm);
    }

    fn dispatch_content_analysis(
        &self,
        user_id: UserId,
        channel_id: ChannelId,
        pcm: Arc<Vec<i16>>,
    ) {
        if self.content_analyzers.is_empty() {
            return;
        }
        let sr = self.options.sample_rate;
        for analyzer in &self.content_analyzers {
            let analyzer = analyzer.clone();
            let cb = self.violation_callback.clone();
            let pcm = Arc::clone(&pcm);
            let uid_str = user_id.to_string();
            let cid_str = channel_id.to_string();
            tokio::spawn(async move {
                match analyzer.analyze(&pcm, sr, &uid_str, &cid_str).await {
                    Ok(violations) if !violations.is_empty() => {
                        warn!(
                            "Content violations detected for user {} in {}: {} issues",
                            uid_str,
                            cid_str,
                            violations.len()
                        );
                        if let Some(ref cb) = cb {
                            cb(user_id, channel_id, violations);
                        }
                    }
                    Err(e) => warn!("Content analysis failed: {}", e),
                    _ => {}
                }
            });
        }
    }

    /// Decode one Opus packet with the per-(user, channel) stateful decoder.
    fn decode_opus_frame(&self, key: (UserId, ChannelId), opus_data: &[u8]) -> Result<Vec<i16>> {
        if opus_data.is_empty() {
            return Ok(Vec::new());
        }
        let mut decoders = self.decoders.lock();
        let decoder = match decoders.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let d = opus::Decoder::new(self.options.sample_rate, opus::Channels::Mono)
                    .map_err(|e| AurixError::Codec(format!("opus decoder: {e}")))?;
                v.insert(d)
            }
        };
        let mut out = vec![0i16; MAX_DECODE_SAMPLES];
        let n = decoder
            .decode(opus_data, &mut out, false)
            .map_err(|e| AurixError::Codec(format!("opus decode: {e}")))?;
        out.truncate(n);
        Ok(out)
    }

    /// Remove all buffers for a user (on disconnect).
    pub fn remove_user(&self, user_id: UserId) {
        let mut buffers = self.buffers.lock();
        buffers.retain(|(uid, _), _| *uid != user_id);
        drop(buffers);
        self.decoders.lock().retain(|(uid, _), _| *uid != user_id);
        self.spoken_languages.lock().remove(&user_id);
    }

    pub fn is_enabled(&self) -> bool {
        self.stt_provider.is_some() || !self.content_analyzers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use aurix_common::types::ChannelConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct FakeStt {
        calls: AtomicUsize,
        gate: Option<Arc<Notify>>,
    }

    #[async_trait]
    impl SttProvider for FakeStt {
        async fn transcribe(&self, audio: &[i16], sample_rate: u32) -> Result<TranscriptResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            Ok(TranscriptResult {
                text: format!("{} samples", audio.len()),
                language: "en".into(),
                words: vec![],
                confidence: 1.0,
                duration_ms: audio.len() as u64 * 1000 / u64::from(sample_rate),
            })
        }
        fn provider_name(&self) -> &str {
            "fake"
        }
    }

    fn opus_frame() -> Vec<u8> {
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let mut out = vec![0u8; 1275];
        let n = enc.encode(&pcm, &mut out).unwrap();
        out.truncate(n);
        out
    }

    fn channel(transcription: bool) -> MediaChannel {
        MediaChannel::new(
            ChannelId::new(),
            AppId::new(),
            ChannelConfig {
                transcription,
                ..ChannelConfig::default()
            },
        )
    }

    fn pipeline(
        stt: Arc<FakeStt>,
        options: PipelineOptions,
    ) -> (
        Arc<AudioAnalysisPipeline>,
        Arc<Mutex<Vec<TranscriptSegment>>>,
    ) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut p = AudioAnalysisPipeline::new(options, Some(stt), vec![]);
        let sink = out.clone();
        p.set_stt_callback(move |seg| sink.lock().push(seg));
        (Arc::new(p), out)
    }

    fn options(segment_ms: u64) -> PipelineOptions {
        PipelineOptions {
            sample_rate: 48_000,
            segment: Duration::from_millis(segment_ms),
            silence_flush: Some(Duration::from_millis(50)),
            min_segment: Duration::from_millis(40),
            max_concurrent_stt: 8,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_transcription_channels_are_analysed() {
        let stt = Arc::new(FakeStt {
            calls: AtomicUsize::new(0),
            gate: None,
        });
        let (p, out) = pipeline(stt.clone(), options(100));
        let off = channel(false);
        let on = channel(true);
        let user = UserId::new();
        let frame = opus_frame();
        assert!(!p.wants_channel(&off));
        assert!(p.wants_channel(&on));
        for _ in 0..10 {
            p.process_opus_packet(&off, user, &frame);
        }
        assert_eq!(stt.calls.load(Ordering::SeqCst), 0);
        for _ in 0..5 {
            p.process_opus_packet(&on, user, &frame);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(stt.calls.load(Ordering::SeqCst), 1);
        let segs = out.lock();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].app_id, on.app_id);
        assert_eq!(segs[0].channel_id, on.channel_id);
        assert_eq!(segs[0].user_id, user);
        assert_eq!(segs[0].audio_ms, 100);
        assert_eq!(segs[0].result.text, "4800 samples");
        assert!(segs[0].deliver && !segs[0].safety);
        assert_eq!(segs[0].pcm.len(), 4800);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn safety_voice_channels_transcribe_only_with_a_safety_consumer() {
        let stt = Arc::new(FakeStt {
            calls: AtomicUsize::new(0),
            gate: None,
        });
        let safety_only = MediaChannel::new(
            ChannelId::new(),
            AppId::new(),
            ChannelConfig {
                safety_voice: true,
                ..ChannelConfig::default()
            },
        );
        let user = UserId::new();
        let frame = opus_frame();

        let (p, _) = pipeline(stt.clone(), options(100));
        assert!(!p.wants_channel(&safety_only), "no safety consumer wired");

        let out = Arc::new(Mutex::new(Vec::new()));
        let mut p = AudioAnalysisPipeline::new(options(100), Some(stt.clone()), vec![]);
        let sink = out.clone();
        p.set_stt_callback(move |seg| sink.lock().push(seg));
        p.set_safety_enabled(true);
        assert!(p.wants_channel(&safety_only));
        for _ in 0..5 {
            p.process_opus_packet(&safety_only, user, &frame);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(stt.calls.load(Ordering::SeqCst), 1);
        let segs = out.lock();
        assert_eq!(segs.len(), 1);
        assert!(segs[0].safety && !segs[0].deliver);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn silence_flushes_short_segments_and_drops_tiny_ones() {
        let stt = Arc::new(FakeStt {
            calls: AtomicUsize::new(0),
            gate: None,
        });
        let (p, out) = pipeline(stt.clone(), options(1000));
        let ch = Arc::new(channel(true));
        let channels = dashmap::DashMap::new();
        channels.insert(ch.channel_id, ch.clone());
        let talker = UserId::new();
        let blip = UserId::new();
        let frame = opus_frame();
        for _ in 0..3 {
            p.process_opus_packet(&ch, talker, &frame); // 60 ms ≥ min 40 ms
        }
        p.process_opus_packet(&ch, blip, &frame); // 20 ms < min
        p.flush_idle(&channels);
        assert_eq!(stt.calls.load(Ordering::SeqCst), 0, "not idle yet");
        tokio::time::sleep(Duration::from_millis(80)).await;
        p.flush_idle(&channels);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(stt.calls.load(Ordering::SeqCst), 1);
        let segs = out.lock();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].user_id, talker);
        assert_eq!(segs[0].audio_ms, 60);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn participant_leave_flushes_tail_and_forgets_state() {
        let stt = Arc::new(FakeStt {
            calls: AtomicUsize::new(0),
            gate: None,
        });
        let (p, out) = pipeline(stt.clone(), options(1000));
        let ch = channel(true);
        let user = UserId::new();
        let frame = opus_frame();
        for _ in 0..4 {
            p.process_opus_packet(&ch, user, &frame);
        }
        p.participant_left(&ch, user);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(out.lock().len(), 1);
        assert!(p.buffers.lock().is_empty());
        assert!(p.decoders.lock().is_empty());
        p.process_opus_packet(&ch, user, &frame);
        p.participant_left(&ch, user);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(out.lock().len(), 1, "20 ms tail is below min_segment");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saturated_stt_drops_segments_instead_of_queueing() {
        let gate = Arc::new(Notify::new());
        let stt = Arc::new(FakeStt {
            calls: AtomicUsize::new(0),
            gate: Some(gate.clone()),
        });
        let mut opts = options(40);
        opts.max_concurrent_stt = 1;
        let (p, out) = pipeline(stt.clone(), opts);
        let ch = channel(true);
        let frame = opus_frame();
        for _ in 0..3 {
            let user = UserId::new();
            p.process_opus_packet(&ch, user, &frame);
            p.process_opus_packet(&ch, user, &frame);
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(stt.calls.load(Ordering::SeqCst), 1);
        gate.notify_waiters();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(out.lock().len(), 1);
        let user = UserId::new();
        p.process_opus_packet(&ch, user, &frame);
        p.process_opus_packet(&ch, user, &frame);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(stt.calls.load(Ordering::SeqCst), 2, "permit released");
    }
}
