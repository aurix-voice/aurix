//! Text-to-speech playout: turns provider PCM into paced Opus frames and injects them into a
//! channel through the [`PacketRouter`], so synthesized speech follows the same routing rules
//! (channel type, receiver preferences, cascade, per-receiver encryption) as a microphone.
//!
//! Synthesized streams carry their own SSRC with the top bit set ([`SYNTH_SSRC_FLAG`]): a
//! participant's voice is `session_ssrc | FLAG`, a server announcement is derived from the
//! channel id. Receivers therefore never mix TTS frames into a microphone jitter buffer, and
//! SDKs can attribute the stream (`ssrc & !FLAG` is the participant's SSRC or nobody's).
//!
//! Synthesis runs concurrently (bounded); playout is serialised per synthesized stream so two
//! requests for the same voice never interleave.

use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{channel_id_hash, TtsState};
use aurix_common::tts_stt::{PcmAudio, TtsProvider};
use aurix_common::types::{AppId, ChannelId, SessionId, UserId};
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::mixer::{FRAME_SAMPLES, SAMPLE_RATE};
use crate::router::{InjectedFrame, PacketRouter};
use crate::session::MediaSession;

/// Top bit of a synthesized stream's SSRC (session SSRCs are always below `0x8000_0000`).
pub const SYNTH_SSRC_FLAG: u32 = 0x8000_0000;
const FRAME_DURATION: Duration = Duration::from_millis(20);

/// SSRC of TTS spoken on behalf of the session with `session_ssrc`.
pub fn participant_voice_ssrc(session_ssrc: u32) -> u32 {
    session_ssrc | SYNTH_SSRC_FLAG
}

/// SSRC of server announcements in `channel_id` (stable per channel, distinct per channel so a
/// listener in several channels keeps the streams apart).
pub fn system_voice_ssrc(channel_id: &ChannelId) -> u32 {
    (channel_id_hash(channel_id) & !SYNTH_SSRC_FLAG) | SYNTH_SSRC_FLAG
}

#[derive(Debug, Clone)]
pub struct TtsEngineOptions {
    /// Longest playout per request; longer synthesis output is truncated.
    pub max_audio: Duration,
    pub bitrate_bps: i32,
    /// Provider requests in flight per node.
    pub max_concurrent_synth: usize,
    /// Requests (queued or playing) per requesting session.
    pub max_queued_per_session: u32,
    /// Requests (queued or playing) per channel.
    pub max_queued_per_channel: u32,
}

impl Default for TtsEngineOptions {
    fn default() -> Self {
        Self {
            max_audio: Duration::from_secs(30),
            bitrate_bps: 32_000,
            max_concurrent_synth: 4,
            max_queued_per_session: 3,
            max_queued_per_channel: 8,
        }
    }
}

/// Who the synthesized speech belongs to and where it goes.
pub enum TtsSource {
    /// Spoken as `session`'s participant: into the channel (`to_channel`) and/or back to the
    /// requester only (`to_self`).
    Participant {
        session: Arc<MediaSession>,
        to_channel: bool,
        to_self: bool,
    },
    /// Server announcement to every local participant of the channel.
    System,
}

pub struct TtsRequest {
    pub app_id: AppId,
    pub channel_id: ChannelId,
    pub text: String,
    pub voice: String,
    pub source: TtsSource,
    pub client_ref: Option<String>,
    /// Reuse an id chosen upstream (e.g. an announcement replicated to every node).
    pub request_id: Option<Uuid>,
}

/// Lifecycle notification for one request.
#[derive(Debug, Clone)]
pub struct TtsStatusEvent {
    pub request_id: Uuid,
    pub app_id: AppId,
    pub channel_id: ChannelId,
    /// Requesting session (`None` for announcements).
    pub session_id: Option<SessionId>,
    pub user_id: Option<UserId>,
    pub client_ref: Option<String>,
    pub state: TtsState,
    pub duration_ms: Option<u64>,
    pub message: Option<String>,
}

struct Job {
    cancel: CancellationToken,
    session_id: Option<SessionId>,
}

/// Sequence/timestamp clock of one synthesized stream; holding its lock is the playout slot.
#[derive(Default)]
struct StreamClock {
    sequence: u32,
    timestamp: u32,
}

pub struct TtsEngine {
    provider: Arc<dyn TtsProvider>,
    router: OnceLock<Arc<PacketRouter>>,
    options: TtsEngineOptions,
    synth_permits: Arc<Semaphore>,
    jobs: DashMap<Uuid, Job>,
    queued_per_session: DashMap<SessionId, u32>,
    queued_per_channel: DashMap<ChannelId, u32>,
    streams: DashMap<u32, Arc<Mutex<StreamClock>>>,
    events: broadcast::Sender<TtsStatusEvent>,
}

impl TtsEngine {
    pub fn new(provider: Arc<dyn TtsProvider>, options: TtsEngineOptions) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            provider,
            router: OnceLock::new(),
            synth_permits: Arc::new(Semaphore::new(options.max_concurrent_synth.max(1))),
            options,
            jobs: DashMap::new(),
            queued_per_session: DashMap::new(),
            queued_per_channel: DashMap::new(),
            streams: DashMap::new(),
            events,
        }
    }

    /// Bind the router once the SFU has started; requests fail until then.
    pub fn attach_router(&self, router: Arc<PacketRouter>) {
        let _ = self.router.set(router);
    }

    pub fn provider_name(&self) -> &str {
        self.provider.provider_name()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TtsStatusEvent> {
        self.events.subscribe()
    }

    /// Queue a request. Returns its id; progress arrives on [`Self::subscribe`].
    pub fn submit(self: &Arc<Self>, request: TtsRequest) -> Result<Uuid> {
        let router = self
            .router
            .get()
            .cloned()
            .ok_or_else(|| AurixError::Internal("media plane not started".into()))?;
        let (session_id, user_id) = match &request.source {
            TtsSource::Participant { session, .. } => {
                (Some(session.session_id), Some(session.user_id))
            }
            TtsSource::System => (None, None),
        };
        if let Some(session_id) = session_id {
            let mut count = self.queued_per_session.entry(session_id).or_insert(0);
            if *count >= self.options.max_queued_per_session {
                return Err(AurixError::RateLimitExceeded(
                    "too many pending text-to-speech requests for this session".into(),
                ));
            }
            *count += 1;
        }
        {
            let mut count = self
                .queued_per_channel
                .entry(request.channel_id)
                .or_insert(0);
            if *count >= self.options.max_queued_per_channel {
                if let Some(session_id) = session_id {
                    self.release_session_slot(session_id);
                }
                return Err(AurixError::RateLimitExceeded(
                    "too many pending text-to-speech requests in this channel".into(),
                ));
            }
            *count += 1;
        }

        let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
        let cancel = CancellationToken::new();
        self.jobs.insert(
            request_id,
            Job {
                cancel: cancel.clone(),
                session_id,
            },
        );
        let base = TtsStatusEvent {
            request_id,
            app_id: request.app_id,
            channel_id: request.channel_id,
            session_id,
            user_id,
            client_ref: request.client_ref.clone(),
            state: TtsState::Queued,
            duration_ms: None,
            message: None,
        };
        let _ = self.events.send(base.clone());

        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = engine.run(&router, request, &base, cancel).await;
            let mut status = base;
            match outcome {
                Ok(Some(duration_ms)) => {
                    status.state = TtsState::Finished;
                    status.duration_ms = Some(duration_ms);
                }
                Ok(None) => status.state = TtsState::Cancelled,
                Err(e) => {
                    warn!("TTS request {} failed: {}", request_id, e);
                    status.state = TtsState::Failed;
                    status.message = Some(e.public_message());
                }
            }
            engine.jobs.remove(&request_id);
            if let Some(session_id) = session_id {
                engine.release_session_slot(session_id);
            }
            engine.release_channel_slot(status.channel_id);
            let _ = engine.events.send(status);
        });
        Ok(request_id)
    }

    /// Cancel one request. `requester` must own it (or be `None` for a server-side cancel).
    pub fn cancel(&self, request_id: &Uuid, requester: Option<SessionId>) -> bool {
        match self.jobs.get(request_id) {
            Some(job) if requester.is_none() || job.session_id == requester => {
                job.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Cancel everything a session requested (on disconnect or an explicit `TtsCancel`).
    /// Returns how many requests were pending.
    pub fn cancel_session(&self, session_id: &SessionId) -> usize {
        let mut n = 0;
        for job in self.jobs.iter() {
            if job.session_id == Some(*session_id) {
                job.cancel.cancel();
                n += 1;
            }
        }
        n
    }

    pub fn pending(&self) -> usize {
        self.jobs.len()
    }

    fn release_session_slot(&self, session_id: SessionId) {
        if let Some(mut count) = self.queued_per_session.get_mut(&session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                drop(count);
                self.queued_per_session
                    .remove_if(&session_id, |_, c| *c == 0);
            }
        }
    }

    fn release_channel_slot(&self, channel_id: ChannelId) {
        if let Some(mut count) = self.queued_per_channel.get_mut(&channel_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                drop(count);
                self.queued_per_channel
                    .remove_if(&channel_id, |_, c| *c == 0);
            }
        }
    }

    /// `Ok(Some(ms))` played to the end, `Ok(None)` cancelled, `Err` failed.
    async fn run(
        &self,
        router: &PacketRouter,
        request: TtsRequest,
        base: &TtsStatusEvent,
        cancel: CancellationToken,
    ) -> Result<Option<u64>> {
        let permit = tokio::select! {
            _ = cancel.cancelled() => return Ok(None),
            p = self.synth_permits.acquire() => p.map_err(|_| AurixError::Internal("tts engine closed".into()))?,
        };
        let pcm = tokio::select! {
            _ = cancel.cancelled() => return Ok(None),
            r = self.provider.synthesize(&request.text, &request.voice) => r?,
        };
        drop(permit);
        let (frames, truncated) = self.encode(&pcm)?;
        if frames.is_empty() {
            return Err(AurixError::Tts("provider returned no audio".into()));
        }
        let duration_ms = frames.len() as u64 * FRAME_DURATION.as_millis() as u64;

        let ssrc = match &request.source {
            TtsSource::Participant { session, .. } => participant_voice_ssrc(session.ssrc),
            TtsSource::System => system_voice_ssrc(&request.channel_id),
        };
        let clock = self
            .streams
            .entry(ssrc)
            .or_insert_with(|| Arc::new(Mutex::new(StreamClock::default())))
            .value()
            .clone();
        let mut clock = tokio::select! {
            _ = cancel.cancelled() => return Ok(None),
            guard = clock.lock() => guard,
        };

        let _ = self.events.send(TtsStatusEvent {
            state: TtsState::Playing,
            duration_ms: Some(duration_ms),
            message: truncated.then(|| "audio truncated to the configured maximum".to_string()),
            ..base.clone()
        });
        debug!(
            "TTS {} playing {} frames ({} ms) on ssrc {:#x} in {}",
            base.request_id,
            frames.len(),
            duration_ms,
            ssrc,
            request.channel_id
        );

        let mut ticker = tokio::time::interval(FRAME_DURATION);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        for frame in frames {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(None),
                _ = ticker.tick() => {}
            }
            let frame = InjectedFrame {
                ssrc,
                sequence: clock.sequence,
                timestamp: clock.timestamp,
                payload: frame,
            };
            clock.sequence = clock.sequence.wrapping_add(1);
            clock.timestamp = clock.timestamp.wrapping_add(FRAME_SAMPLES as u32);
            match &request.source {
                TtsSource::Participant {
                    session,
                    to_channel,
                    to_self,
                } => {
                    router
                        .inject_participant_audio(
                            session,
                            &request.channel_id,
                            frame,
                            *to_channel,
                            *to_self,
                        )
                        .await?
                }
                TtsSource::System => {
                    router
                        .inject_system_audio(&request.app_id, &request.channel_id, frame)
                        .await?
                }
            }
        }
        Ok(Some(duration_ms))
    }

    /// Mono 48 kHz Opus frames of `pcm`, capped at `max_audio`; the flag says whether it was cut.
    fn encode(&self, pcm: &PcmAudio) -> Result<(Vec<Bytes>, bool)> {
        if pcm.sample_rate == 0 || pcm.channels == 0 {
            return Err(AurixError::Tts(
                "provider returned an invalid audio format".into(),
            ));
        }
        let mono = pcm.to_mono();
        let mut samples = resample_linear(&mono, pcm.sample_rate, SAMPLE_RATE);
        let max_samples = (self.options.max_audio.as_secs_f64() * f64::from(SAMPLE_RATE)) as usize;
        let truncated = samples.len() > max_samples;
        if truncated {
            samples.truncate(max_samples);
        }
        trim_silence(&mut samples);
        let frames = encode_opus_frames(&samples, self.options.bitrate_bps)?;
        Ok((frames, truncated))
    }
}

/// Linear-interpolation resampler; adequate for speech from typical 16–24 kHz TTS output.
pub fn resample_linear(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    let out_len = ((samples.len() as f64) / ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    let last = samples.len() - 1;
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = (pos as usize).min(last);
        let frac = pos - idx as f64;
        let a = f64::from(samples[idx]);
        let b = f64::from(samples[(idx + 1).min(last)]);
        out.push((a + (b - a) * frac).round().clamp(-32768.0, 32767.0) as i16);
    }
    out
}

/// Drop leading/trailing digital silence (provider padding) but keep 20 ms of tail.
fn trim_silence(samples: &mut Vec<i16>) {
    const THRESHOLD: i16 = 64;
    let start = samples
        .iter()
        .position(|s| s.unsigned_abs() > THRESHOLD as u16)
        .unwrap_or(samples.len());
    let end = samples
        .iter()
        .rposition(|s| s.unsigned_abs() > THRESHOLD as u16)
        .map(|i| (i + 1 + FRAME_SAMPLES).min(samples.len()))
        .unwrap_or(0);
    if start >= end {
        samples.clear();
        return;
    }
    samples.truncate(end);
    samples.drain(..start);
}

/// Encode mono 48 kHz PCM into 20 ms Opus frames (the last frame is zero-padded).
pub fn encode_opus_frames(samples: &[i16], bitrate_bps: i32) -> Result<Vec<Bytes>> {
    if samples.is_empty() {
        return Ok(Vec::new());
    }
    let mut encoder =
        opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|e| AurixError::Codec(format!("opus encoder: {e}")))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(bitrate_bps.clamp(6_000, 128_000)))
        .map_err(|e| AurixError::Codec(format!("opus bitrate: {e}")))?;
    let _ = encoder.set_inband_fec(true);
    let mut out = Vec::with_capacity(samples.len() / FRAME_SAMPLES + 1);
    let mut buf = vec![0u8; 1275];
    let mut frame = vec![0i16; FRAME_SAMPLES];
    for chunk in samples.chunks(FRAME_SAMPLES) {
        frame[..chunk.len()].copy_from_slice(chunk);
        frame[chunk.len()..].fill(0);
        let n = encoder
            .encode(&frame, &mut buf)
            .map_err(|e| AurixError::Codec(format!("opus encode: {e}")))?;
        out.push(Bytes::copy_from_slice(&buf[..n]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_ssrcs_have_the_flag_and_keep_the_session_ssrc() {
        let ssrc = 0x1234_5678;
        assert_eq!(participant_voice_ssrc(ssrc), 0x9234_5678);
        assert_eq!(participant_voice_ssrc(ssrc) & !SYNTH_SSRC_FLAG, ssrc);
        let ch = ChannelId::new();
        assert_ne!(system_voice_ssrc(&ch) & SYNTH_SSRC_FLAG, 0);
        assert_eq!(system_voice_ssrc(&ch), system_voice_ssrc(&ch));
    }

    #[test]
    fn resample_changes_length_and_keeps_level() {
        let tone: Vec<i16> = (0..24_000)
            .map(|i| {
                ((i as f32 / 24_000.0 * 440.0 * std::f32::consts::TAU).sin() * 10_000.0) as i16
            })
            .collect();
        let up = resample_linear(&tone, 24_000, 48_000);
        assert_eq!(up.len(), 48_000);
        let rms = |s: &[i16]| {
            (s.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
        };
        assert!((rms(&up) - rms(&tone)).abs() < 200.0);
        assert_eq!(resample_linear(&tone, 48_000, 48_000).len(), tone.len());
        let down = resample_linear(&tone, 24_000, 16_000);
        assert_eq!(down.len(), 16_000);
    }

    #[test]
    fn trims_padding_but_keeps_a_tail() {
        let mut s = vec![0i16; 4800];
        s.extend(std::iter::repeat_n(8000, 960));
        s.extend(std::iter::repeat_n(0, 9600));
        trim_silence(&mut s);
        assert_eq!(s.len(), 960 + FRAME_SAMPLES);
        assert_eq!(s[0], 8000);
        let mut quiet = vec![0i16; 1000];
        trim_silence(&mut quiet);
        assert!(quiet.is_empty());
    }

    #[test]
    fn encodes_whole_frames_and_pads_the_last() {
        let samples = vec![1000i16; FRAME_SAMPLES * 2 + 10];
        let frames = encode_opus_frames(&samples, 32_000).unwrap();
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|f| !f.is_empty()));
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
        let mut out = vec![0i16; 5760];
        let n = dec.decode(&frames[0], &mut out, false).unwrap();
        assert_eq!(n, FRAME_SAMPLES);
        assert!(encode_opus_frames(&[], 32_000).unwrap().is_empty());
    }
}
