use aurix_common::error::{AurixError, Result};
use aurix_common::tts_stt::{ContentAnalyzer, ContentViolation, SttProvider, TranscriptResult};
use aurix_common::types::{ChannelId, UserId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

/// Largest Opus frame (120 ms @ 48 kHz mono).
const MAX_DECODE_SAMPLES: usize = 5760;

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
    decoders: Mutex<HashMap<(UserId, ChannelId), opus::Decoder>>,
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
            decoders: Mutex::new(HashMap::new()),
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

    /// Feed a raw Opus packet from the router. Decodes to PCM with a stateful
    /// per-user decoder, accumulates, and dispatches when the buffer is full.
    pub fn process_opus_packet(
        &self,
        user_id: UserId,
        channel_id: ChannelId,
        opus_data: &[u8],
    ) {
        let key = (user_id, channel_id);
        let pcm_samples = match self.decode_opus_frame(key, opus_data) {
            Ok(samples) => samples,
            Err(e) => {
                warn!("Opus decode failed (analysis skipped): {}", e);
                return;
            }
        };

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

    /// Decode one Opus packet with the per-(user, channel) stateful decoder.
    fn decode_opus_frame(&self, key: (UserId, ChannelId), opus_data: &[u8]) -> Result<Vec<i16>> {
        if opus_data.is_empty() {
            return Ok(Vec::new());
        }
        let mut decoders = self.decoders.lock();
        let decoder = match decoders.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let d = opus::Decoder::new(self.sample_rate, opus::Channels::Mono)
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
    }

    pub fn is_enabled(&self) -> bool {
        self.stt_provider.is_some() || !self.content_analyzers.is_empty()
    }
}