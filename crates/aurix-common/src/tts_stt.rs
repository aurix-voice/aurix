use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

/// Trait for pluggable Text-to-Speech engines.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    /// Synthesize text into Opus-encoded audio frames.
    /// Returns a Vec of Opus packets ready for injection into a channel.
    async fn synthesize(&self, text: &str, voice: &str, sample_rate: u32) -> Result<Vec<Vec<u8>>>;

    /// List available voice names.
    fn available_voices(&self) -> Vec<String>;

    fn provider_name(&self) -> &str;
}

/// Reference STT implementation using a self-hosted Whisper inference server.
pub struct WhisperSttProvider {
    endpoint: String,
    client: reqwest::Client,
}

impl WhisperSttProvider {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client creation should not fail"),
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
            .map_err(|e| crate::error::AurixError::Internal(format!("MIME error: {e}")))?;

        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "word");

        let response = self
            .client
            .post(format!("{}/v1/audio/transcriptions", self.endpoint))
            .multipart(form)
            .send()
            .await
            .map_err(|e| {
                crate::error::AurixError::Internal(format!("Whisper request failed: {e}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::error::AurixError::Internal(format!(
                "Whisper returned {status}: {body}"
            )));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| crate::error::AurixError::Internal(format!("Whisper parse error: {e}")))?;

        let text = body["text"].as_str().unwrap_or("").to_string();
        let language = body["language"].as_str().unwrap_or("en").to_string();
        let duration_ms = (body["duration"].as_f64().unwrap_or(0.0) * 1000.0) as u64;

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
fn pcm_to_wav(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
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
