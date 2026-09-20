//! Post-hoc processing of stored recordings: channel mixdowns rendered from per-participant
//! tracks (Ogg/Opus or WAV) and speech-to-text transcripts with speaker attribution.
//!
//! Jobs are node-local: the node that accepted the request decodes on the blocking thread
//! pool, bounded by `recording.processing.max_concurrent`, and must be able to read every
//! source track (its own file or object storage). State lives in the database so any node can
//! answer status queries; a node restart fails the jobs it was running (mixdowns) or re-runs
//! them (transcripts), and clients may simply request again.

use crate::mixdown::{
    mix_tracks, segment_for_stt, MixTrack, OpusSink, TrackDecoder, WavSink, MIX_SAMPLE_RATE,
};
use crate::{
    decrypt_blob, discard_file, encrypt_blob, object_key, RecordingService, RECORDING_KIND_MIXDOWN,
    RECORDING_KIND_RECORDING,
};
use aurix_common::error::{AurixError, Result};
use aurix_common::tts_stt::{SttProvider, TranscriptWord};
use aurix_common::types::{AppId, ChannelId};
use aurix_db::models::{RecordingRow, RecordingTranscriptRow};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{info, warn};
use uuid::Uuid;

pub const FORMAT_OGG_OPUS: &str = "ogg_opus";
pub const FORMAT_WAV: &str = "wav";

/// Container of a rendered mixdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MixdownFormat {
    #[default]
    OggOpus,
    Wav,
}

impl MixdownFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OggOpus => FORMAT_OGG_OPUS,
            Self::Wav => FORMAT_WAV,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::OggOpus => "ogg",
            Self::Wav => "wav",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::OggOpus => "audio/ogg",
            Self::Wav => "audio/wav",
        }
    }
}

/// Content type for a stored recording's `format` column.
pub fn content_type_for(format: &str) -> &'static str {
    if format == FORMAT_WAV {
        MixdownFormat::Wav.content_type()
    } else {
        MixdownFormat::OggOpus.content_type()
    }
}

#[derive(Debug, Clone)]
pub struct MixdownRequest {
    pub channel_id: ChannelId,
    /// Tracks to combine; empty means every finished track of the channel.
    pub sources: Vec<Uuid>,
    pub format: MixdownFormat,
    pub stereo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingKind {
    Mixdown,
    Transcript,
}

impl ProcessingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mixdown => "mixdown",
            Self::Transcript => "transcript",
        }
    }
}

/// Emitted when a job finishes, for the server to turn into tenant events.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessingNotice {
    pub app_id: AppId,
    pub channel_id: ChannelId,
    pub recording_id: Uuid,
    pub kind: ProcessingKind,
    /// `ready` or `failed`.
    pub status: &'static str,
    pub error: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// One speaker turn of a stored transcript (`recording_transcripts.segments[]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    /// Recorded user; absent only for legacy rows.
    pub speaker: Option<Uuid>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub language: String,
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<TranscriptWord>,
}

pub(crate) struct Jobs {
    permits: Arc<Semaphore>,
    /// Accepted jobs not yet finished on this node.
    pending: AtomicU32,
    max_pending: u32,
    notices: mpsc::UnboundedSender<ProcessingNotice>,
    notice_rx: Mutex<Option<mpsc::UnboundedReceiver<ProcessingNotice>>>,
}

impl Jobs {
    pub(crate) fn new(max_concurrent: u32, max_queued: u32) -> Self {
        let (notices, notice_rx) = mpsc::unbounded_channel();
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent.max(1) as usize)),
            pending: AtomicU32::new(0),
            max_pending: max_concurrent.max(1).saturating_add(max_queued),
            notices,
            notice_rx: Mutex::new(Some(notice_rx)),
        }
    }

    fn admit(&self) -> Result<()> {
        let prev = self.pending.fetch_add(1, Ordering::AcqRel);
        if prev >= self.max_pending {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(AurixError::RateLimitExceeded(
                "Recording processing queue is full on this node; retry later".into(),
            ));
        }
        Ok(())
    }

    fn release(&self) {
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A decrypted source track with its place on the channel timeline.
struct SourceTrack {
    bytes: Vec<u8>,
    /// Wall-clock start of the first audio written (falls back to the row's start).
    audio_started_at: DateTime<Utc>,
}

fn audio_start(row: &RecordingRow) -> DateTime<Utc> {
    row.audio_started_at.unwrap_or(row.started_at)
}

fn offset_ms(base: DateTime<Utc>, start: DateTime<Utc>) -> u64 {
    (start - base).num_milliseconds().max(0) as u64
}

impl RecordingService {
    /// Completion notices; may be taken once (by the server binary).
    pub fn take_processing_notices(&self) -> Option<mpsc::UnboundedReceiver<ProcessingNotice>> {
        self.jobs.notice_rx.lock().take()
    }

    /// Post-hoc processing accepted on this node.
    pub fn processing_enabled(&self) -> bool {
        self.config.enabled && self.config.processing.enabled
    }

    /// A speech-to-text provider is wired in, so transcripts can be requested.
    pub fn transcripts_available(&self) -> bool {
        self.processing_enabled() && self.stt.is_some()
    }

    fn require_processing(&self) -> Result<()> {
        if !self.processing_enabled() {
            return Err(AurixError::InvalidConfiguration(
                "Recording processing is disabled (recording.enabled / recording.processing.enabled)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Render a mixdown of finished tracks of one channel. Returns the new `recordings` row in
    /// state `processing`; poll it (or listen for `recording.processed`) until `ready`.
    pub async fn request_mixdown(
        self: &Arc<Self>,
        app_id: AppId,
        req: MixdownRequest,
    ) -> Result<RecordingRow> {
        self.require_processing()?;
        let max_sources = self.config.processing.max_sources as usize;
        let sources = if req.sources.is_empty() {
            let rows = aurix_db::queries::list_channel_tracks(
                &self.pool,
                app_id.0,
                req.channel_id.0,
                max_sources as i64 + 1,
            )
            .await
            .map_err(|e| AurixError::Database(format!("Track lookup failed: {e}")))?;
            if rows.is_empty() {
                return Err(AurixError::NotFound(
                    "No finished tracks recorded in this channel".into(),
                ));
            }
            rows
        } else {
            let mut ids = req.sources.clone();
            ids.sort_unstable();
            ids.dedup();
            let rows = aurix_db::queries::get_recordings(&self.pool, app_id.0, &ids)
                .await
                .map_err(|e| AurixError::Database(format!("Track lookup failed: {e}")))?;
            if rows.len() != ids.len() {
                return Err(AurixError::NotFound("Source recording not found".into()));
            }
            rows
        };
        if sources.len() > max_sources {
            return Err(AurixError::Validation(format!(
                "A mixdown may combine at most {max_sources} tracks"
            )));
        }
        for row in &sources {
            if row.channel_id != req.channel_id.0 {
                return Err(AurixError::Validation(format!(
                    "Recording {} was captured in another channel",
                    row.id
                )));
            }
            self.check_track_usable(row)?;
        }
        self.check_tracks_reachable(&sources)?;

        let base = sources
            .iter()
            .map(audio_start)
            .min()
            .unwrap_or_else(Utc::now);
        let recording_id = Uuid::now_v7();
        let dir = PathBuf::from(&self.config.storage_path)
            .join(app_id.0.to_string())
            .join(req.channel_id.0.to_string());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| AurixError::Recording(format!("Failed to create directory: {e}")))?;
        let file_path = dir
            .join(format!("{recording_id}_mixdown.{}", req.format.extension()))
            .to_string_lossy()
            .to_string();
        let now = Utc::now();
        let row = RecordingRow {
            id: recording_id,
            app_id: app_id.0,
            channel_id: req.channel_id.0,
            session_id: Uuid::nil(),
            user_id: Uuid::nil(),
            file_path: file_path.clone(),
            file_size_bytes: 0,
            duration_secs: 0.0,
            format: req.format.as_str().to_string(),
            encrypted: self.encryption_key.is_some(),
            encryption_key_id: self.encryption_key_id.clone(),
            started_at: base,
            ended_at: None,
            expires_at: now + Duration::days(self.config.retention_days as i64),
            created_at: now,
            kind: RECORDING_KIND_MIXDOWN.to_string(),
            status: "processing".to_string(),
            audio_started_at: Some(base),
            sources: Some(sources.iter().map(|r| r.id).collect()),
            node_id: Some(self.node_id),
            error: None,
        };
        self.jobs.admit()?;
        let created = match aurix_db::queries::create_recording(&self.pool, &row).await {
            Ok(r) => r,
            Err(e) => {
                self.jobs.release();
                return Err(AurixError::Database(format!(
                    "Mixdown row creation failed: {e}"
                )));
            }
        };
        let this = Arc::clone(self);
        let channels: u8 = if req.stereo { 2 } else { 1 };
        let format = req.format;
        let channel_id = req.channel_id;
        tokio::spawn(async move {
            let permit = this.jobs.permits.clone().acquire_owned().await;
            let outcome = match permit {
                Ok(_permit) => {
                    this.run_mixdown(
                        app_id,
                        recording_id,
                        &file_path,
                        &sources,
                        base,
                        channels,
                        format,
                    )
                    .await
                }
                Err(_) => Err(AurixError::Internal("job pool closed".into())),
            };
            this.jobs.release();
            let error = match &outcome {
                Ok(()) => None,
                Err(e) => {
                    warn!("Mixdown {recording_id} failed: {e}");
                    discard_file(&file_path).await;
                    let _ =
                        aurix_db::queries::fail_recording(&this.pool, recording_id, &e.to_string())
                            .await;
                    Some(e.to_string())
                }
            };
            let _ = this.jobs.notices.send(ProcessingNotice {
                app_id,
                channel_id,
                recording_id,
                kind: ProcessingKind::Mixdown,
                status: if error.is_none() { "ready" } else { "failed" },
                error,
                timestamp: Utc::now(),
            });
        });
        Ok(created)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_mixdown(
        &self,
        app_id: AppId,
        recording_id: Uuid,
        file_path: &str,
        sources: &[RecordingRow],
        base: DateTime<Utc>,
        channels: u8,
        format: MixdownFormat,
    ) -> Result<()> {
        let mut tracks = Vec::with_capacity(sources.len());
        for row in sources {
            tracks.push(self.load_source(row).await?);
        }
        let bitrate = self.config.processing.mixdown_bitrate;
        let path = file_path.to_string();
        let duration_samples = tokio::task::spawn_blocking(move || -> Result<u64> {
            let mut mix = Vec::with_capacity(tracks.len());
            for t in &tracks {
                mix.push(MixTrack {
                    decoder: TrackDecoder::new(&t.bytes, channels)?,
                    offset_samples: offset_ms(base, t.audio_started_at)
                        * u64::from(MIX_SAMPLE_RATE)
                        / 1000,
                    gain: 1.0,
                });
            }
            let file = std::fs::File::create(&path)
                .map_err(|e| AurixError::Recording(format!("Failed to create file: {e}")))?;
            match format {
                MixdownFormat::OggOpus => {
                    let mut sink = OpusSink::new(BufWriter::new(file), channels, bitrate, false)?;
                    mix_tracks(mix, channels, &mut sink)
                }
                MixdownFormat::Wav => {
                    let mut sink = WavSink::new(BufWriter::new(file), channels)?;
                    mix_tracks(mix, channels, &mut sink)
                }
            }
        })
        .await
        .map_err(|e| AurixError::Recording(format!("Mixdown task failed: {e}")))??;

        if let Some(ref key) = self.encryption_key {
            let plain = tokio::fs::read(file_path)
                .await
                .map_err(|e| AurixError::Recording(format!("Read failed: {e}")))?;
            let encrypted = encrypt_blob(key, &plain)?;
            tokio::fs::write(file_path, &encrypted)
                .await
                .map_err(|e| AurixError::Recording(format!("Encrypted write failed: {e}")))?;
        }
        let size = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| AurixError::Recording(format!("Metadata read failed: {e}")))?
            .len();
        let secs = duration_samples as f64 / f64::from(MIX_SAMPLE_RATE);
        let present = aurix_db::queries::finish_recording(
            &self.pool,
            recording_id,
            size as i64,
            secs,
            Some(base),
        )
        .await
        .map_err(|e| AurixError::Database(format!("Mixdown finish failed: {e}")))?;
        if !present {
            discard_file(file_path).await;
            return Err(AurixError::Recording(
                "Mixdown was deleted while rendering".into(),
            ));
        }
        if let Some(ref s3) = self.s3 {
            let key = object_key(app_id, recording_id, format.as_str());
            match tokio::fs::read(file_path).await {
                Ok(body) => match s3.put_object(&key, body, format.content_type()).await {
                    Ok(()) => info!("Mixdown {recording_id} uploaded to object storage as {key}"),
                    Err(e) => warn!(
                        "Object storage upload failed for mixdown {recording_id}: {e} (file kept locally)"
                    ),
                },
                Err(e) => warn!("Could not read {file_path} for upload: {e}"),
            }
        }
        info!(
            "Mixdown {recording_id} ready: {} tracks, {secs:.1}s, {size} bytes ({})",
            sources.len(),
            format.as_str()
        );
        Ok(())
    }

    /// Transcribe a finished recording with the configured speech-to-text provider. For a
    /// mixdown the original tracks are transcribed one by one, so every segment carries its
    /// speaker. Returns the transcript row in state `queued`.
    pub async fn request_transcript(
        self: &Arc<Self>,
        app_id: AppId,
        recording_id: Uuid,
    ) -> Result<RecordingTranscriptRow> {
        self.require_processing()?;
        let Some(stt) = self.stt.clone() else {
            return Err(AurixError::InvalidConfiguration(
                "No speech-to-text provider configured ([stt])".into(),
            ));
        };
        let row = self
            .get_recording(app_id, recording_id)
            .await?
            .ok_or_else(|| AurixError::NotFound("Recording not found".into()))?;
        if row.status != "ready" {
            return Err(AurixError::Conflict(format!(
                "Recording is not ready (status: {})",
                row.status
            )));
        }
        let tracks = self.transcript_sources(&row).await?;
        self.check_tracks_reachable(&tracks)?;

        self.jobs.admit()?;
        let queued = aurix_db::queries::queue_recording_transcript(
            &self.pool,
            app_id.0,
            recording_id,
            self.node_id,
        )
        .await;
        match queued {
            Ok(true) => {}
            Ok(false) => {
                self.jobs.release();
                return Err(AurixError::Conflict(
                    "A transcript of this recording is already being produced".into(),
                ));
            }
            Err(e) => {
                self.jobs.release();
                return Err(AurixError::Database(format!(
                    "Transcript request failed: {e}"
                )));
            }
        }
        self.spawn_transcript_job(app_id, row, tracks, stt);
        aurix_db::queries::get_recording_transcript(&self.pool, app_id.0, recording_id)
            .await
            .map_err(|e| AurixError::Database(format!("Transcript lookup failed: {e}")))?
            .ok_or_else(|| AurixError::NotFound("Transcript not found".into()))
    }

    pub async fn get_transcript(
        &self,
        app_id: AppId,
        recording_id: Uuid,
    ) -> Result<Option<RecordingTranscriptRow>> {
        aurix_db::queries::get_recording_transcript(&self.pool, app_id.0, recording_id)
            .await
            .map_err(|e| AurixError::Database(format!("Transcript lookup failed: {e}")))
    }

    fn spawn_transcript_job(
        self: &Arc<Self>,
        app_id: AppId,
        row: RecordingRow,
        tracks: Vec<RecordingRow>,
        stt: Arc<dyn SttProvider>,
    ) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let recording_id = row.id;
            let channel_id = ChannelId(row.channel_id);
            let permit = this.jobs.permits.clone().acquire_owned().await;
            let outcome = match permit {
                Ok(_permit) => {
                    match aurix_db::queries::start_recording_transcript(
                        &this.pool,
                        recording_id,
                        this.node_id,
                    )
                    .await
                    {
                        Ok(true) => Some(this.run_transcript(&row, &tracks, stt).await),
                        Ok(false) => None,
                        Err(e) => Some(Err(AurixError::Database(format!(
                            "Transcript start failed: {e}"
                        )))),
                    }
                }
                Err(_) => Some(Err(AurixError::Internal("job pool closed".into()))),
            };
            this.jobs.release();
            let Some(outcome) = outcome else {
                return;
            };
            let error = match outcome {
                Ok(()) => None,
                Err(e) => {
                    warn!("Transcript of {recording_id} failed: {e}");
                    let _ = aurix_db::queries::fail_recording_transcript(
                        &this.pool,
                        recording_id,
                        this.node_id,
                        &e.to_string(),
                    )
                    .await;
                    Some(e.to_string())
                }
            };
            let _ = this.jobs.notices.send(ProcessingNotice {
                app_id,
                channel_id,
                recording_id,
                kind: ProcessingKind::Transcript,
                status: if error.is_none() { "ready" } else { "failed" },
                error,
                timestamp: Utc::now(),
            });
        });
    }

    async fn run_transcript(
        &self,
        row: &RecordingRow,
        tracks: &[RecordingRow],
        stt: Arc<dyn SttProvider>,
    ) -> Result<()> {
        let base = if row.kind == RECORDING_KIND_MIXDOWN {
            audio_start(row)
        } else {
            tracks
                .iter()
                .map(audio_start)
                .min()
                .unwrap_or(row.started_at)
        };
        let rate = self.config.processing.stt_sample_rate;
        let max_samples = rate as usize * self.config.processing.stt_chunk_secs as usize;
        let min_samples = rate as usize / 4;
        let mut segments: Vec<TranscriptSegment> = Vec::new();
        let mut duration_ms: u64 = 0;
        for track in tracks {
            let source = self.load_source(track).await?;
            let track_offset = offset_ms(base, source.audio_started_at);
            let (tx, mut rx) = mpsc::channel::<(Vec<i16>, u64)>(2);
            let bytes = source.bytes;
            let decode = tokio::task::spawn_blocking(move || {
                segment_for_stt(&bytes, rate, max_samples, min_samples, |pcm, start_ms| {
                    tx.blocking_send((pcm, start_ms))
                        .map_err(|_| AurixError::Recording("transcript consumer went away".into()))
                })
            });
            while let Some((pcm, start_ms)) = rx.recv().await {
                let chunk_ms = pcm.len() as u64 * 1000 / u64::from(rate);
                let result = stt.transcribe(&pcm, rate).await?;
                let start = track_offset + start_ms;
                if result.text.trim().is_empty() {
                    continue;
                }
                let words = result
                    .words
                    .into_iter()
                    .map(|w| TranscriptWord {
                        word: w.word,
                        start_ms: w.start_ms + start,
                        end_ms: w.end_ms + start,
                        confidence: w.confidence,
                    })
                    .collect();
                segments.push(TranscriptSegment {
                    speaker: Some(track.user_id),
                    start_ms: start,
                    end_ms: start + chunk_ms,
                    text: result.text.trim().to_string(),
                    language: result.language,
                    confidence: result.confidence,
                    words,
                });
            }
            let track_ms = decode.await.map_err(|e| {
                AurixError::Recording(format!("Transcript decode task failed: {e}"))
            })??;
            duration_ms = duration_ms.max(track_offset + track_ms);
        }
        segments.sort_by_key(|s| (s.start_ms, s.speaker));
        let text = segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let language = dominant_language(&segments);
        let json = serde_json::to_value(&segments)
            .map_err(|e| AurixError::Internal(format!("transcript encode: {e}")))?;
        let stored = aurix_db::queries::complete_recording_transcript(
            &self.pool,
            row.id,
            self.node_id,
            stt.provider_name(),
            language.as_deref(),
            &text,
            &json,
            duration_ms as i64,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Transcript store failed: {e}")))?;
        if !stored {
            return Err(AurixError::Recording(
                "Recording was deleted while transcribing".into(),
            ));
        }
        info!(
            "Transcript of {} ready: {} segments, {} tracks, {:.1}s",
            row.id,
            segments.len(),
            tracks.len(),
            duration_ms as f64 / 1000.0
        );
        Ok(())
    }

    /// Tracks whose audio a transcript of `row` is built from.
    async fn transcript_sources(&self, row: &RecordingRow) -> Result<Vec<RecordingRow>> {
        if row.kind == RECORDING_KIND_MIXDOWN {
            let ids = row.sources.clone().unwrap_or_default();
            let tracks = aurix_db::queries::get_recordings(&self.pool, row.app_id, &ids)
                .await
                .map_err(|e| AurixError::Database(format!("Track lookup failed: {e}")))?;
            if tracks.is_empty() {
                return Err(AurixError::Conflict(
                    "The tracks this mixdown was rendered from no longer exist".into(),
                ));
            }
            for t in &tracks {
                self.check_track_usable(t)?;
            }
            return Ok(tracks);
        }
        if row.format != FORMAT_OGG_OPUS {
            return Err(AurixError::Validation(
                "Only Ogg/Opus recordings can be transcribed".into(),
            ));
        }
        Ok(vec![row.clone()])
    }

    fn check_track_usable(&self, row: &RecordingRow) -> Result<()> {
        if row.kind != RECORDING_KIND_RECORDING {
            return Err(AurixError::Validation(format!(
                "Recording {} is a {} and cannot be used as a source track",
                row.id, row.kind
            )));
        }
        if row.status != "ready" {
            return Err(AurixError::Conflict(format!(
                "Recording {} is not finished (status: {})",
                row.id, row.status
            )));
        }
        if row.format != FORMAT_OGG_OPUS {
            return Err(AurixError::Validation(format!(
                "Recording {} is not Ogg/Opus",
                row.id
            )));
        }
        if row.encrypted && row.encryption_key_id != self.encryption_key_id {
            return Err(AurixError::Encryption(format!(
                "Recording {} was encrypted with a key that is not configured",
                row.id
            )));
        }
        Ok(())
    }

    /// Every source must be readable here: its file on this node, or object storage.
    fn check_tracks_reachable(&self, rows: &[RecordingRow]) -> Result<()> {
        if self.s3.is_some() {
            return Ok(());
        }
        for row in rows {
            if !std::path::Path::new(&row.file_path).is_file() {
                let where_ = row
                    .node_id
                    .map(|n| format!("node {n}"))
                    .unwrap_or_else(|| "another node".to_string());
                return Err(AurixError::Conflict(format!(
                    "Recording {} is stored on {where_}; send this request to that node or configure object storage",
                    row.id
                )));
            }
        }
        Ok(())
    }

    async fn load_source(&self, row: &RecordingRow) -> Result<SourceTrack> {
        let bytes = match tokio::fs::read(&row.file_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(ref s3) = self.s3 else {
                    return Err(AurixError::Recording(format!(
                        "Recording {} is not stored on this node",
                        row.id
                    )));
                };
                s3.get_object(&object_key(AppId(row.app_id), row.id, &row.format))
                    .await?
                    .ok_or_else(|| {
                        AurixError::Recording(format!(
                            "Recording {} is neither on this node nor in object storage",
                            row.id
                        ))
                    })?
            }
            Err(e) => {
                return Err(AurixError::Recording(format!(
                    "Read of {} failed: {e}",
                    row.id
                )))
            }
        };
        let bytes = if row.encrypted {
            if row.encryption_key_id != self.encryption_key_id {
                return Err(AurixError::Encryption(format!(
                    "Recording {} was encrypted with a key that is not configured",
                    row.id
                )));
            }
            let key = self
                .encryption_key
                .as_ref()
                .ok_or_else(|| AurixError::Encryption("No encryption key configured".into()))?;
            decrypt_blob(key, &bytes)?
        } else {
            bytes
        };
        Ok(SourceTrack {
            audio_started_at: audio_start(row),
            bytes,
        })
    }

    /// Reconciles jobs this node owned before it (re)started: mixdowns cannot resume and are
    /// failed (the client requests again), transcripts are re-run from scratch.
    pub async fn recover_processing(self: &Arc<Self>) -> Result<()> {
        let stale = aurix_db::queries::list_processing_recordings_on_node(&self.pool, self.node_id)
            .await
            .map_err(|e| AurixError::Database(format!("Stale mixdown lookup failed: {e}")))?;
        for row in &stale {
            discard_file(&row.file_path).await;
            let _ = aurix_db::queries::fail_recording(
                &self.pool,
                row.id,
                "interrupted by node restart",
            )
            .await;
            let _ = self.jobs.notices.send(ProcessingNotice {
                app_id: AppId(row.app_id),
                channel_id: ChannelId(row.channel_id),
                recording_id: row.id,
                kind: ProcessingKind::Mixdown,
                status: "failed",
                error: Some("interrupted by node restart".into()),
                timestamp: Utc::now(),
            });
        }
        if !stale.is_empty() {
            warn!(
                "{} mixdowns interrupted by the restart marked failed",
                stale.len()
            );
        }

        let unfinished =
            aurix_db::queries::list_unfinished_transcripts_on_node(&self.pool, self.node_id)
                .await
                .map_err(|e| {
                    AurixError::Database(format!("Stale transcript lookup failed: {e}"))
                })?;
        let mut requeued = 0usize;
        for t in unfinished {
            let app_id = AppId(t.app_id);
            let restart = async {
                let stt = self.stt.clone().ok_or_else(|| {
                    AurixError::InvalidConfiguration("No speech-to-text provider configured".into())
                })?;
                let row = self
                    .get_recording(app_id, t.recording_id)
                    .await?
                    .ok_or_else(|| AurixError::NotFound("Recording not found".into()))?;
                let tracks = self.transcript_sources(&row).await?;
                self.check_tracks_reachable(&tracks)?;
                self.jobs.admit()?;
                if !aurix_db::queries::queue_recording_transcript(
                    &self.pool,
                    app_id.0,
                    t.recording_id,
                    self.node_id,
                )
                .await
                .map_err(|e| AurixError::Database(e.to_string()))?
                {
                    // Still `running`/`queued` from before the restart: reset it ourselves.
                    let _ = aurix_db::queries::fail_recording_transcript(
                        &self.pool,
                        t.recording_id,
                        self.node_id,
                        "interrupted by node restart",
                    )
                    .await;
                    aurix_db::queries::queue_recording_transcript(
                        &self.pool,
                        app_id.0,
                        t.recording_id,
                        self.node_id,
                    )
                    .await
                    .map_err(|e| AurixError::Database(e.to_string()))?;
                }
                self.spawn_transcript_job(app_id, row, tracks, stt);
                Ok::<(), AurixError>(())
            };
            match restart.await {
                Ok(()) => requeued += 1,
                Err(e) => {
                    let _ = aurix_db::queries::fail_recording_transcript(
                        &self.pool,
                        t.recording_id,
                        self.node_id,
                        &format!("interrupted by node restart: {e}"),
                    )
                    .await;
                }
            }
        }
        if requeued > 0 {
            info!("{requeued} transcript jobs re-queued after restart");
        }
        Ok(())
    }
}

fn dominant_language(segments: &[TranscriptSegment]) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in segments {
        if !s.language.is_empty() {
            *counts.entry(s.language.as_str()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(lang, n)| (*n, std::cmp::Reverse(lang.to_string())))
        .map(|(lang, _)| lang.to_string())
}

/// Renders stored segments as SubRip subtitles.
pub fn render_srt(segments: &[TranscriptSegment]) -> String {
    let mut out = String::new();
    for (i, s) in segments.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            srt_time(s.start_ms),
            srt_time(s.end_ms.max(s.start_ms + 1)),
            cue_text(s)
        ));
    }
    out
}

/// Renders stored segments as WebVTT.
pub fn render_vtt(segments: &[TranscriptSegment]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for s in segments {
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            vtt_time(s.start_ms),
            vtt_time(s.end_ms.max(s.start_ms + 1)),
            cue_text(s)
        ));
    }
    out
}

fn cue_text(s: &TranscriptSegment) -> String {
    match s.speaker {
        Some(u) => format!("<v {u}>{}", s.text),
        None => s.text.clone(),
    }
}

fn srt_time(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn vtt_time(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(
        speaker: Option<Uuid>,
        start: u64,
        end: u64,
        text: &str,
        lang: &str,
    ) -> TranscriptSegment {
        TranscriptSegment {
            speaker,
            start_ms: start,
            end_ms: end,
            text: text.into(),
            language: lang.into(),
            confidence: 0.9,
            words: Vec::new(),
        }
    }

    #[test]
    fn renders_srt_and_vtt_with_speakers() {
        let u = Uuid::nil();
        let segs = vec![
            seg(Some(u), 0, 1500, "hello", "en"),
            seg(None, 3_661_001, 3_662_500, "bye", "en"),
        ];
        let srt = render_srt(&segs);
        assert!(srt.starts_with(
            "1\n00:00:00,000 --> 00:00:01,500\n<v 00000000-0000-0000-0000-000000000000>hello\n\n"
        ));
        assert!(srt.contains("2\n01:01:01,001 --> 01:01:02,500\nbye\n"));
        let vtt = render_vtt(&segs);
        assert!(vtt.starts_with("WEBVTT\n\n00:00:00.000 --> 00:00:01.500\n"));
        assert!(vtt.contains("01:01:01.001 --> 01:01:02.500\nbye\n"));
    }

    #[test]
    fn dominant_language_is_the_most_frequent() {
        let segs = vec![
            seg(None, 0, 1, "a", "ru"),
            seg(None, 1, 2, "b", "en"),
            seg(None, 2, 3, "c", "ru"),
        ];
        assert_eq!(dominant_language(&segs).as_deref(), Some("ru"));
        assert_eq!(dominant_language(&[]), None);
    }

    #[test]
    fn queue_admission_is_bounded() {
        let jobs = Jobs::new(1, 1);
        jobs.admit().unwrap();
        jobs.admit().unwrap();
        assert!(matches!(
            jobs.admit(),
            Err(AurixError::RateLimitExceeded(_))
        ));
        jobs.release();
        jobs.admit().unwrap();
    }
}
