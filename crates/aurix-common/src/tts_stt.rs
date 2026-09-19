use crate::error::{AurixError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A word-level timestamp from STT transcription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptWord {
    pub word: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub confidence: f32,
}

/// Full STT transcript result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptResult {
    pub text: String,
    pub language: String,
    pub words: Vec<TranscriptWord>,
    pub confidence: f32,
    pub duration_ms: u64,
}

/// Trait for pluggable Speech-to-Text engines.
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// Transcribe raw PCM audio (mono, 16-bit, at the given sample rate).
    async fn transcribe(&self, audio_pcm: &[i16], sample_rate: u32) -> Result<TranscriptResult>;

    /// Returns the name of this STT provider for logging.
    fn provider_name(&self) -> &str;
}

/// Interleaved 16-bit PCM as returned by a TTS engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmAudio {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

impl PcmAudio {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            self.frames() as u64 * 1000 / self.sample_rate as u64
        }
    }

    /// Downmix to mono by averaging the channels.
    pub fn to_mono(&self) -> Vec<i16> {
        match self.channels {
            0 => Vec::new(),
            1 => self.samples.clone(),
            n => {
                let n = n as usize;
                self.samples
                    .chunks_exact(n)
                    .map(|frame| {
                        let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
                        (sum / n as i32) as i16
                    })
                    .collect()
            }
        }
    }
}

/// Trait for pluggable Text-to-Speech engines. Engines return PCM; the media layer
/// resamples and Opus-encodes it for the channel.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    async fn synthesize(&self, text: &str, voice: &str) -> Result<PcmAudio>;

    fn provider_name(&self) -> &str;
}

/// Connection options shared by the HTTP reference providers.
#[derive(Debug, Clone, Default)]
pub struct HttpProviderOptions {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub timeout: Option<Duration>,
}

pub(crate) fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client creation should not fail")
}

pub(crate) fn trim_endpoint(endpoint: &str) -> String {
    endpoint.trim().trim_end_matches('/').to_string()
}

/// Reference STT implementation using a self-hosted Whisper inference server.
pub struct WhisperSttProvider {
    endpoint: String,
    api_key: Option<String>,
    model: Option<String>,
    language: Option<String>,
    client: reqwest::Client,
}

impl WhisperSttProvider {
    pub fn new(endpoint: &str) -> Self {
        Self::with_options(endpoint, HttpProviderOptions::default(), None)
    }

    pub fn with_options(
        endpoint: &str,
        options: HttpProviderOptions,
        language: Option<String>,
    ) -> Self {
        Self {
            endpoint: trim_endpoint(endpoint),
            api_key: options.api_key,
            model: options.model,
            language,
            client: http_client(options.timeout.unwrap_or(Duration::from_secs(30))),
        }
    }
}

#[async_trait]
impl SttProvider for WhisperSttProvider {
    async fn transcribe(&self, audio_pcm: &[i16], sample_rate: u32) -> Result<TranscriptResult> {
        // Convert i16 PCM to WAV bytes for the Whisper API
        let wav_data = pcm_to_wav(audio_pcm, sample_rate, 1);

        let part = reqwest::multipart::Part::bytes(wav_data)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| AurixError::Internal(format!("MIME error: {e}")))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "word");
        if let Some(model) = &self.model {
            form = form.text("model", model.clone());
        }
        if let Some(language) = &self.language {
            form = form.text("language", language.clone());
        }

        let mut request = self
            .client
            .post(format!("{}/v1/audio/transcriptions", self.endpoint))
            .multipart(form);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AurixError::Stt(format!("request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = read_body_limited(response, 4096).await.unwrap_or_default();
            return Err(AurixError::Stt(format!(
                "provider returned {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }

        let body = read_body_limited(response, MAX_STT_RESPONSE_BYTES).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| AurixError::Stt(format!("parse error: {e}")))?;

        let text = body["text"].as_str().unwrap_or("").to_string();
        let language = body["language"].as_str().unwrap_or("en").to_string();
        let duration_ms = (body["duration"].as_f64().unwrap_or(0.0) * 1000.0) as u64;
        let duration_ms = if duration_ms == 0 && sample_rate > 0 {
            audio_pcm.len() as u64 * 1000 / u64::from(sample_rate)
        } else {
            duration_ms
        };

        let mut words = Vec::new();
        if let Some(word_arr) = body["words"].as_array() {
            for w in word_arr {
                words.push(TranscriptWord {
                    word: w["word"].as_str().unwrap_or("").to_string(),
                    start_ms: (w["start"].as_f64().unwrap_or(0.0) * 1000.0) as u64,
                    end_ms: (w["end"].as_f64().unwrap_or(0.0) * 1000.0) as u64,
                    confidence: w["probability"].as_f64().unwrap_or(0.0) as f32,
                });
            }
        }

        Ok(TranscriptResult {
            text,
            language,
            words,
            confidence: 1.0,
            duration_ms,
        })
    }

    fn provider_name(&self) -> &str {
        "whisper"
    }
}

const MAX_STT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Hard cap on a TTS response body (~10 minutes of 48 kHz stereo 16-bit).
const MAX_TTS_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

pub(crate) async fn read_body_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>> {
    if let Some(len) = response.content_length() {
        if len > limit as u64 {
            return Err(AurixError::Internal(format!(
                "response body of {len} bytes exceeds the {limit} byte limit"
            )));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| AurixError::Internal(format!("response read failed: {e}")))?
    {
        if body.len() + chunk.len() > limit {
            return Err(AurixError::Internal(format!(
                "response body exceeds the {limit} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Reference TTS implementation for OpenAI-compatible `POST /v1/audio/speech` servers that
/// can return WAV (`response_format: "wav"`): Piper via OpenedAI-Speech, Kokoro-FastAPI,
/// Coqui/XTTS wrappers, or the hosted OpenAI API.
pub struct HttpTtsProvider {
    endpoint: String,
    api_key: Option<String>,
    model: Option<String>,
    max_response_bytes: usize,
    client: reqwest::Client,
}

impl HttpTtsProvider {
    pub fn new(endpoint: &str, options: HttpProviderOptions) -> Self {
        Self {
            endpoint: trim_endpoint(endpoint),
            api_key: options.api_key,
            model: options.model,
            max_response_bytes: MAX_TTS_RESPONSE_BYTES,
            client: http_client(options.timeout.unwrap_or(Duration::from_secs(30))),
        }
    }

    /// Cap the accepted response size (defaults to 128 MiB).
    pub fn with_max_response_bytes(mut self, bytes: usize) -> Self {
        self.max_response_bytes = bytes.max(64);
        self
    }
}

#[async_trait]
impl TtsProvider for HttpTtsProvider {
    async fn synthesize(&self, text: &str, voice: &str) -> Result<PcmAudio> {
        let mut body = serde_json::json!({
            "input": text,
            "voice": voice,
            "response_format": "wav",
        });
        if let Some(model) = &self.model {
            body["model"] = serde_json::Value::String(model.clone());
        }
        let mut request = self
            .client
            .post(format!("{}/v1/audio/speech", self.endpoint))
            .header(
                reqwest::header::ACCEPT,
                "audio/wav, audio/x-wav, audio/wave",
            )
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AurixError::Tts(format!("request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = read_body_limited(response, 4096).await.unwrap_or_default();
            return Err(AurixError::Tts(format!(
                "provider returned {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let bytes = read_body_limited(response, self.max_response_bytes)
            .await
            .map_err(|e| AurixError::Tts(e.to_string()))?;
        parse_wav(&bytes)
    }

    fn provider_name(&self) -> &str {
        "http-speech"
    }
}

/// Decode a RIFF/WAVE buffer (PCM 8/16/24/32-bit or IEEE float 32/64, any channel count)
/// into interleaved 16-bit samples.
pub fn parse_wav(bytes: &[u8]) -> Result<PcmAudio> {
    fn u16_at(b: &[u8], i: usize) -> Option<u16> {
        b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32_at(b: &[u8], i: usize) -> Option<u32> {
        b.get(i..i + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    let bad = |what: &str| AurixError::Tts(format!("invalid WAV: {what}"));

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("missing RIFF/WAVE header"));
    }
    let mut pos = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None; // (tag, channels, rate, bits)
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(bytes, pos + 4).ok_or_else(|| bad("truncated chunk"))? as usize;
        let body_start = pos + 8;
        // Streaming encoders write 0xFFFFFFFF / 0 for unknown data sizes: take the remainder.
        let body_end = if id == b"data" && (size == u32::MAX as usize || size == 0) {
            bytes.len()
        } else {
            body_start.saturating_add(size).min(bytes.len())
        };
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(bad("short fmt chunk"));
                }
                let mut tag = u16_at(body, 0).unwrap_or(0);
                let channels = u16_at(body, 2).unwrap_or(0);
                let rate = u32_at(body, 4).unwrap_or(0);
                let bits = u16_at(body, 14).unwrap_or(0);
                if tag == 0xFFFE {
                    // WAVE_FORMAT_EXTENSIBLE: the real tag is the first two bytes of the GUID.
                    tag = u16_at(body, 24).ok_or_else(|| bad("short extensible fmt"))?;
                }
                format = Some((tag, channels, rate, bits));
            }
            b"data" => {
                data = Some(body);
                if body_end == bytes.len() {
                    break;
                }
            }
            _ => {}
        }
        // Chunks are word-aligned.
        pos = body_end + (size & 1);
        if body_end == bytes.len() {
            break;
        }
    }
    let (tag, channels, sample_rate, bits) = format.ok_or_else(|| bad("no fmt chunk"))?;
    let data = data.ok_or_else(|| bad("no data chunk"))?;
    if channels == 0 || channels > 8 {
        return Err(bad("unsupported channel count"));
    }
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err(bad("unsupported sample rate"));
    }
    let samples: Vec<i16> = match (tag, bits) {
        (1, 8) => data.iter().map(|b| (i16::from(*b) - 128) << 8).collect(),
        (1, 16) => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect(),
        (1, 24) => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[1], c[2]]))
            .collect(),
        (1, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[2], c[3]]))
            .collect(),
        (3, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| float_to_i16(f32::from_le_bytes(*c)))
            .collect(),
        (3, 64) => data
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| float_to_i16(f64::from_le_bytes(*c) as f32))
            .collect(),
        _ => return Err(bad("unsupported sample format")),
    };
    let usable = samples.len() - samples.len() % channels as usize;
    let mut samples = samples;
    samples.truncate(usable);
    Ok(PcmAudio {
        sample_rate,
        channels,
        samples,
    })
}

fn float_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Trait for audio content analysis plugins (hate speech detection, etc.)
#[async_trait]
pub trait ContentAnalyzer: Send + Sync {
    /// Analyze a chunk of audio. Returns a list of policy violations detected.
    async fn analyze(
        &self,
        audio_pcm: &[i16],
        sample_rate: u32,
        user_id: &str,
        channel_id: &str,
    ) -> Result<Vec<ContentViolation>>;

    fn analyzer_name(&self) -> &str;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentViolation {
    pub category: String,
    pub severity: f32,
    pub description: String,
    pub timestamp_ms: u64,
}

/// Convert raw PCM i16 mono samples to a WAV byte buffer.
pub fn pcm_to_wav(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let file_len = 36 + data_len;
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;

    let mut buf = Vec::with_capacity(44 + samples.len() * 2);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_len.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_roundtrip_mono_16bit() {
        let samples: Vec<i16> = (0..480).map(|i| (i * 37 % 2000 - 1000) as i16).collect();
        let wav = pcm_to_wav(&samples, 16_000, 1);
        let parsed = parse_wav(&wav).unwrap();
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples, samples);
        assert_eq!(parsed.duration_ms(), 30);
    }

    #[test]
    fn wav_stereo_downmix_and_odd_chunks() {
        // fmt, then an odd-sized LIST chunk (padded), then data.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&3u32.to_le_bytes());
        wav.extend_from_slice(&[1, 2, 3, 0]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&8u32.to_le_bytes());
        for s in [1000i16, 3000, -2000, -4000] {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let parsed = parse_wav(&wav).unwrap();
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.frames(), 2);
        assert_eq!(parsed.to_mono(), vec![2000, -3000]);
    }

    #[test]
    fn wav_float32_and_streaming_size() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&u32::MAX.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&22_050u32.to_le_bytes());
        wav.extend_from_slice(&(22_050u32 * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&32u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&u32::MAX.to_le_bytes());
        for v in [0.5f32, -1.0, 2.0] {
            wav.extend_from_slice(&v.to_le_bytes());
        }
        let parsed = parse_wav(&wav).unwrap();
        assert_eq!(parsed.sample_rate, 22_050);
        assert_eq!(parsed.samples, vec![16_384, -32_767, 32_767]);
    }

    #[test]
    fn wav_rejects_garbage() {
        assert!(parse_wav(b"not a wav").is_err());
        assert!(parse_wav(&pcm_to_wav(&[], 16_000, 1)[..20]).is_err());
    }
}
