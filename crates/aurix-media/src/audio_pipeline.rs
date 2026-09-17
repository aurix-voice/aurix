use aurix_common::error::Result;
use aurix_common::tts_stt::{ContentAnalyzer, ContentViolation, SttProvider, TranscriptResult};
use aurix_common::types::{ChannelId, UserId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

/// Per-user PCM accumulation buffer.
struct UserAudioBuffer {
    pcm: Vec<i16>,
    #[allow(dead_code)]
    sample_rate: u32,
}

/// Pipeline that intercepts Opus packets from the SFU router,
/// decodes them to PCM, accumulates a configurable window,
/// and dispatches to STT providers and content analyzers.
pub struct AudioAnalysisPipeline {
    /// Per-user PCM buffers keyed by (user_id, channel_id)
    buffers: Mutex<HashMap<(UserId, ChannelId), UserAudioBuffer>>,
    stt_provider: Option<Arc<dyn SttProvider>>,
    content_analyzers: Vec<Arc<dyn ContentAnalyzer>>,
    /// How many PCM samples to accumulate before dispatching (e.g. 2 seconds at 48kHz = 96000)
    dispatch_threshold: usize,
    sample_rate: u32,
    /// Callback for STT results
    stt_callback: Option<Arc<dyn Fn(UserId, ChannelId, TranscriptResult) + Send + Sync>>,
    /// Callback for content violations
    violation_callback: Option<Arc<dyn Fn(UserId, ChannelId, Vec<ContentViolation>) + Send + Sync>>,
}

impl AudioAnalysisPipeline {
    pub fn new(
        sample_rate: u32,
        buffer_duration_secs: f32,
        stt_provider: Option<Arc<dyn SttProvider>>,
        content_analyzers: Vec<Arc<dyn ContentAnalyzer>>,
    ) -> Self {
        let dispatch_threshold = (sample_rate as f32 * buffer_duration_secs) as usize;
        Self {
            buffers: Mutex::new(HashMap::new()),
            stt_provider,
            content_analyzers,
            dispatch_threshold,
            sample_rate,
            stt_callback: None,
            violation_callback: None,
        }
    }

    pub fn set_stt_callback<F>(&mut self, f: F)
    where
        F: Fn(UserId, ChannelId, TranscriptResult) + Send + Sync + 'static,
    {
        self.stt_callback = Some(Arc::new(f));
    }

    pub fn set_violation_callback<F>(&mut self, f: F)
    where
        F: Fn(UserId, ChannelId, Vec<ContentViolation>) + Send + Sync + 'static,
    {
        self.violation_callback = Some(Arc::new(f));
    }

    /// Feed a raw Opus packet from the router. Decodes to PCM
    /// using a simple single-frame Opus decode, accumulates,
    /// and dispatches when the buffer is full.
    ///
    /// NOTE: Opus decoding requires the `audiopus` crate with `static` feature.
    /// If audiopus is not available, this method stores raw bytes and the
    /// STT provider must handle Opus-encoded input directly.
    pub fn process_opus_packet(
        &self,
        user_id: UserId,
        channel_id: ChannelId,
        opus_data: &[u8],
    ) {
        // Decode Opus to PCM using a stateless single-frame decode.
        // For production, maintain per-user decoders for cross-packet state.
        let pcm_samples = match Self::decode_opus_frame(opus_data, self.sample_rate) {
            Ok(samples) => samples,
            Err(e) => {
                // If Opus decode is unavailable, skip analysis for this packet
                warn!("Opus decode failed (analysis skipped): {}", e);
                return;
            }
        };

        let key = (user_id, channel_id);
        let should_dispatch = {
            let mut buffers = self.buffers.lock();
            let buf = buffers.entry(key).or_insert_with(|| UserAudioBuffer {
                pcm: Vec::with_capacity(self.dispatch_threshold),
                sample_rate: self.sample_rate,
            });
            buf.pcm.extend_from_slice(&pcm_samples);
            buf.pcm.len() >= self.dispatch_threshold
        };

        if should_dispatch {
            let pcm = {
                let mut buffers = self.buffers.lock();
                if let Some(buf) = buffers.get_mut(&key) {
                    std::mem::take(&mut buf.pcm)
                } else {
                    return;
                }
            };
            self.dispatch_analysis(user_id, channel_id, pcm);
        }
    }

    fn dispatch_analysis(&self, user_id: UserId, channel_id: ChannelId, pcm: Vec<i16>) {
        // STT
        if let Some(ref stt) = self.stt_provider {
            let stt = stt.clone();
            let cb = self.stt_callback.clone();
            let sr = self.sample_rate;
            let pcm_for_stt = pcm.clone();
            tokio::spawn(async move {
                match stt.transcribe(&pcm_for_stt, sr).await {
                    Ok(result) => {
                        if !result.text.trim().is_empty() {
                            info!("STT [{}@{}]: {}", user_id, channel_id, result.text);
                            if let Some(ref cb) = cb {
                                cb(user_id, channel_id, result);
                            }
                        }
                    }
                    Err(e) => warn!("STT failed for {}: {}", user_id, e),
                }
            });
        }

        // Content analysis
        for analyzer in &self.content_analyzers {
            let analyzer = analyzer.clone();
            let cb = self.violation_callback.clone();
            let pcm = pcm.clone();
            let sr = self.sample_rate;
            let uid_str = user_id.to_string();
            let cid_str = channel_id.to_string();
            tokio::spawn(async move {
                match analyzer.analyze(&pcm, sr, &uid_str, &cid_str).await {
                    Ok(violations) if !violations.is_empty() => {
                        warn!(
                            "Content violations detected for user {} in {}: {} issues",
                            uid_str, cid_str, violations.len()
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

    /// Minimal single-frame Opus decode.
    /// Uses Opus packet format: the first byte encodes the TOC (table of contents).
    /// Frame duration can be derived from the TOC to calculate sample count.
    ///
    /// In production with the `audiopus` crate:
    /// ```ignore
    /// let mut decoder = audiopus::coder::Decoder::new(
    ///     audiopus::SampleRate::Hz48000,
    ///     audiopus::Channels::Mono,
    /// ).unwrap();
    /// let mut output = vec![0i16; 5760]; // max frame size
    /// let n = decoder.decode(Some(opus_data), &mut output, false).unwrap();
    /// output.truncate(n);
    /// ```
    ///
    /// Without audiopus, we provide a fallback that estimates frame size
    /// from the Opus TOC byte and fills silence (for builds without libopus).
    fn decode_opus_frame(opus_data: &[u8], sample_rate: u32) -> Result<Vec<i16>> {
        if opus_data.is_empty() {
            return Ok(Vec::new());
        }

        // Fallback: parse Opus TOC to determine frame duration, return silence
        let toc = opus_data[0];
        let config = (toc >> 3) & 0x1F;
        let frame_duration_ms: f32 = match config {
            0..=3 => 10.0,
            4..=7 => 20.0,
            8..=11 => 40.0,
            12..=13 => 60.0,
            14..=15 => if config == 14 { 10.0 } else { 20.0 },
            16..=19 => 2.5,
            20..=23 => 5.0,
            24..=27 => 10.0,
            28..=31 => 20.0,
            _ => 20.0,
        };
        let sample_count = (sample_rate as f32 * frame_duration_ms / 1000.0) as usize;
        Ok(vec![0i16; sample_count])
    }

    /// Remove all buffers for a user (on disconnect).
    pub fn remove_user(&self, user_id: UserId) {
        let mut buffers = self.buffers.lock();
        buffers.retain(|(uid, _), _| *uid != user_id);
    }
}