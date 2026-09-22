pub mod live;
pub mod live_directory;
pub mod mixdown;
pub mod ogg;
pub mod processing;
pub mod s3;

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use aurix_common::config::RecordingConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::sink::{AudioEvidence, AudioSink, EvidenceStore, StoredEvidence};
use aurix_common::tts_stt::SttProvider;
use aurix_common::types::*;
use aurix_db::models::RecordingRow;
use aurix_db::DbPool;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use live::LiveStreams;
use ogg::OggOpusWriter;
use parking_lot::Mutex;
use processing::Jobs;
use s3::{S3Client, S3Config};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tracing::{info, warn};
use uuid::Uuid;

/// Packets buffered before writing, to absorb reordering (≈100 ms at 20 ms frames).
const REORDER_DEPTH: usize = 5;
/// Largest RTP gap preserved as silence in the Ogg granule stream (5 s at 48 kHz).
const MAX_GAP_SAMPLES: u64 = 48_000 * 5;
const DEFAULT_FRAME_SAMPLES: u64 = 960;
const OPUS_CLOCK_RATE: u64 = 48_000;

/// Reorders Opus packets by RTP timestamp and derives per-packet durations from timestamp
/// deltas, so the Ogg granule positions reflect real time including gaps.
struct PacketOrderer {
    pending: BTreeMap<u64, Vec<u8>>,
    last_ts: Option<u64>,
    /// Extended (wrap-free) timestamp base.
    highest_ext: Option<u64>,
}

impl PacketOrderer {
    fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            last_ts: None,
            highest_ext: None,
        }
    }

    fn extend(&mut self, ts: u32) -> u64 {
        let Some(highest) = self.highest_ext else {
            self.highest_ext = Some(ts as u64);
            return ts as u64;
        };
        let cycle = highest & !0xFFFF_FFFF;
        let candidates = [
            cycle.wrapping_sub(1 << 32) | ts as u64,
            cycle | ts as u64,
            (cycle + (1u64 << 32)) | ts as u64,
        ];
        let ext = candidates
            .into_iter()
            .min_by_key(|c| c.abs_diff(highest))
            .unwrap_or(ts as u64);
        if ext > highest {
            self.highest_ext = Some(ext);
        }
        ext
    }

    /// Returns packets ready to be written as `(samples, data)` pairs.
    fn push(&mut self, ts: u32, data: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let ext = self.extend(ts);
        if let Some(last) = self.last_ts {
            if ext <= last {
                return Vec::new();
            }
        }
        self.pending.entry(ext).or_insert_with(|| data.to_vec());
        let mut out = Vec::new();
        while self.pending.len() > REORDER_DEPTH {
            if let Some((ts, pkt)) = self.pending.pop_first() {
                out.push((self.duration_for(ts), pkt));
            }
        }
        out
    }

    fn flush(&mut self) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some((ts, pkt)) = self.pending.pop_first() {
            out.push((self.duration_for(ts), pkt));
        }
        out
    }

    fn duration_for(&mut self, ts: u64) -> u64 {
        let dur = match self.last_ts {
            Some(prev) if ts > prev => (ts - prev).min(MAX_GAP_SAMPLES),
            _ => DEFAULT_FRAME_SAMPLES,
        };
        self.last_ts = Some(ts);
        dur
    }
}

struct ActiveRecording {
    app_id: Uuid,
    channel_id: ChannelId,
    user_id: UserId,
    writer: OggOpusWriter<BufWriter<std::fs::File>>,
    file_path: String,
    orderer: PacketOrderer,
    consent: RecordingConsent,
    started_at: DateTime<Utc>,
    /// When the first packet was written (consent may delay it past `started_at`).
    audio_started_at: Option<DateTime<Utc>>,
    packets_written: u64,
}

#[derive(Default)]
struct ActiveState {
    by_id: HashMap<Uuid, ActiveRecording>,
    by_channel_user: HashMap<(ChannelId, UserId), Uuid>,
    channels: HashSet<ChannelId>,
}

impl ActiveState {
    fn insert(&mut self, id: Uuid, rec: ActiveRecording) {
        self.by_channel_user
            .insert((rec.channel_id, rec.user_id), id);
        self.channels.insert(rec.channel_id);
        self.by_id.insert(id, rec);
    }

    fn remove(&mut self, id: &Uuid) -> Option<ActiveRecording> {
        let rec = self.by_id.remove(id)?;
        self.by_channel_user.remove(&(rec.channel_id, rec.user_id));
        if !self.by_id.values().any(|r| r.channel_id == rec.channel_id) {
            self.channels.remove(&rec.channel_id);
        }
        Some(rec)
    }
}

pub struct RecordingService {
    pool: DbPool,
    config: RecordingConfig,
    encryption_key: Option<[u8; 32]>,
    encryption_key_id: Option<String>,
    s3: Option<S3Client>,
    active: Arc<Mutex<ActiveState>>,
    live: Arc<LiveStreams>,
    node_id: Uuid,
    stt: Option<Arc<dyn SttProvider>>,
    jobs: Jobs,
}

/// A capture (stored recording or live stream) active in a channel, for client disclosure.
#[derive(Debug, Clone, Copy)]
pub struct ActiveCapture {
    pub id: Uuid,
    /// Nil for operator-started live streams.
    pub initiated_by: UserId,
    pub live: bool,
}

impl RecordingService {
    /// `stt` enables post-hoc transcripts of stored recordings (`recording.processing`).
    pub fn new(
        pool: DbPool,
        config: RecordingConfig,
        production: bool,
        node_id: Uuid,
        stt: Option<Arc<dyn SttProvider>>,
    ) -> Result<Self> {
        let encryption_key = if config.encryption_enabled {
            let key_str = config.encryption_key.as_ref().ok_or_else(|| {
                AurixError::InvalidConfiguration(
                    "Recording encryption enabled but no key provided".into(),
                )
            })?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(key_str)
                .map_err(|e| {
                    AurixError::InvalidConfiguration(format!("Invalid encryption key: {e}"))
                })?;
            if decoded.len() != 32 {
                return Err(AurixError::InvalidConfiguration(
                    "Encryption key must be 32 bytes (base64 of 32 random bytes)".into(),
                ));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&decoded);
            Some(key)
        } else {
            None
        };
        // Key id lets operators rotate keys while still identifying which key a file was written with.
        let encryption_key_id = encryption_key.map(|k| hex::encode(&Sha256::digest(k)[..8]));

        let s3 = match (&config.s3_bucket, &config.s3_region) {
            (Some(bucket), Some(region)) => {
                let endpoint = config
                    .s3_endpoint
                    .clone()
                    .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
                Some(S3Client::new(S3Config {
                    endpoint,
                    region: region.clone(),
                    bucket: bucket.clone(),
                    access_key: config.s3_access_key.clone().unwrap_or_default(),
                    secret_key: config.s3_secret_key.clone().unwrap_or_default(),
                    path_style: config.s3_endpoint.is_some(),
                })?)
            }
            (None, None) => None,
            _ => {
                return Err(AurixError::InvalidConfiguration(
                    "recording.s3_bucket and recording.s3_region must be set together".into(),
                ))
            }
        };

        let live = Arc::new(LiveStreams::new(
            config.live.clone(),
            config.require_consent,
            production,
            config.max_recording_duration_secs,
            node_id,
        ));
        let jobs = Jobs::new(
            config.processing.max_concurrent,
            config.processing.max_queued,
        );

        Ok(Self {
            pool,
            config,
            encryption_key,
            encryption_key_id,
            s3,
            active: Arc::new(Mutex::new(ActiveState::default())),
            live,
            node_id,
            stt,
            jobs,
        })
    }

    pub fn require_consent(&self) -> bool {
        self.config.require_consent
    }

    /// Stored (file) recordings enabled on this node.
    pub fn storage_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Real-time streams of channel audio to operator services.
    pub fn live(&self) -> &Arc<LiveStreams> {
        &self.live
    }

    /// Start recording one participant. Each participant gets a separate Ogg/Opus track.
    /// With `require_consent`, no audio is written until `set_consent(.., Accepted)` is called.
    pub async fn start_recording(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        session_id: SessionId,
        user_id: UserId,
        sample_rate: u32,
        channels: u8,
    ) -> Result<RecordingRow> {
        if !self.config.enabled {
            return Err(AurixError::InvalidConfiguration(
                "Stored recordings are disabled (recording.enabled)".into(),
            ));
        }
        if self
            .active
            .lock()
            .by_channel_user
            .contains_key(&(channel_id, user_id))
        {
            return Err(AurixError::Conflict(
                "User is already being recorded in this channel".into(),
            ));
        }
        let recording_id = Uuid::now_v7();
        let dir = PathBuf::from(&self.config.storage_path)
            .join(app_id.0.to_string())
            .join(channel_id.0.to_string());
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| AurixError::Recording(format!("Failed to create directory: {e}")))?;

        let file_name = format!("{}_{}.ogg", recording_id, user_id.0);
        let file_path = dir.join(&file_name).to_string_lossy().to_string();
        let now = Utc::now();
        let expires_at = now + Duration::days(self.config.retention_days as i64);

        let row = RecordingRow {
            id: recording_id,
            app_id: app_id.0,
            channel_id: channel_id.0,
            session_id: session_id.0,
            user_id: user_id.0,
            file_path: file_path.clone(),
            file_size_bytes: 0,
            duration_secs: 0.0,
            format: "ogg_opus".to_string(),
            encrypted: self.encryption_key.is_some(),
            encryption_key_id: self.encryption_key_id.clone(),
            started_at: now,
            ended_at: None,
            expires_at,
            created_at: now,
            kind: RECORDING_KIND_RECORDING.to_string(),
            status: "recording".to_string(),
            audio_started_at: None,
            sources: None,
            node_id: Some(self.node_id),
            error: None,
        };
        let created = aurix_db::queries::create_recording(&self.pool, &row)
            .await
            .map_err(|e| AurixError::Database(format!("Recording creation failed: {e}")))?;

        let file = std::fs::File::create(&file_path)
            .map_err(|e| AurixError::Recording(format!("Failed to create file: {e}")))?;
        let serial = crc32fast::hash(recording_id.as_bytes());
        let writer = OggOpusWriter::new(BufWriter::new(file), serial, sample_rate, channels)
            .map_err(|e| AurixError::Recording(format!("Failed to init Ogg writer: {e}")))?;

        let consent = if self.config.require_consent {
            RecordingConsent::Pending
        } else {
            RecordingConsent::Accepted
        };
        self.active.lock().insert(
            recording_id,
            ActiveRecording {
                app_id: app_id.0,
                channel_id,
                user_id,
                writer,
                file_path,
                orderer: PacketOrderer::new(),
                consent,
                started_at: now,
                audio_started_at: None,
                packets_written: 0,
            },
        );
        info!(
            "Recording started: {} for user {} in {} (consent: {:?})",
            recording_id, user_id, channel_id, consent
        );
        Ok(created)
    }

    /// Active stored recordings and live streams in a channel.
    pub fn active_in_channel(&self, channel_id: &ChannelId) -> Vec<ActiveCapture> {
        let mut out: Vec<ActiveCapture> = {
            let st = self.active.lock();
            st.by_id
                .iter()
                .filter(|(_, r)| r.channel_id == *channel_id)
                .map(|(id, r)| ActiveCapture {
                    id: *id,
                    initiated_by: r.user_id,
                    live: false,
                })
                .collect()
        };
        out.extend(
            self.live
                .active_in_channel(channel_id)
                .into_iter()
                .map(|id| ActiveCapture {
                    id,
                    initiated_by: UserId(Uuid::nil()),
                    live: true,
                }),
        );
        out
    }

    pub fn consent_state(&self, recording_id: &Uuid) -> Option<RecordingConsent> {
        self.active
            .lock()
            .by_id
            .get(recording_id)
            .map(|r| r.consent)
    }

    /// Record a participant's consent decision for a stored recording or a live stream.
    /// Declining stops their recording / their frames immediately.
    pub async fn set_consent(
        &self,
        app_id: AppId,
        recording_id: Uuid,
        user_id: UserId,
        consent: RecordingConsent,
    ) -> Result<()> {
        if self
            .live
            .set_consent(app_id, recording_id, user_id, consent)?
        {
            return Ok(());
        }
        let stop = {
            let mut st = self.active.lock();
            let rec = st
                .by_id
                .get_mut(&recording_id)
                .filter(|r| r.app_id == app_id.0)
                .ok_or_else(|| {
                    AurixError::NotFound("Recording is not active on this node".into())
                })?;
            if rec.user_id != user_id {
                return Err(AurixError::AuthorizationDenied(
                    "Consent can only be given by the recorded user".into(),
                ));
            }
            rec.consent = consent;
            consent == RecordingConsent::Declined
        };
        info!(
            "Recording {} consent for user {}: {:?}",
            recording_id, user_id, consent
        );
        if stop {
            self.stop_recording(app_id, recording_id).await?;
        }
        Ok(())
    }

    /// Feed one Opus packet (48 kHz RTP clock) into the recording of `(channel, user)`.
    pub fn write_opus_packet(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        rtp_timestamp: u32,
        opus_data: &[u8],
    ) -> Result<bool> {
        if opus_data.is_empty() {
            return Ok(false);
        }
        let mut st = self.active.lock();
        let Some(id) = st.by_channel_user.get(&(channel_id, user_id)).copied() else {
            return Ok(false);
        };
        let rec = st
            .by_id
            .get_mut(&id)
            .ok_or_else(|| AurixError::Recording("Recording not active".into()))?;
        if rec.consent != RecordingConsent::Accepted {
            return Ok(false);
        }
        if rec.audio_started_at.is_none() {
            rec.audio_started_at = Some(Utc::now());
        }
        for (samples, pkt) in rec.orderer.push(rtp_timestamp, opus_data) {
            rec.writer
                .write_packet_with_duration(&pkt, scale_samples(samples, rec.writer.sample_rate()))
                .map_err(|e| AurixError::Recording(format!("Write failed: {e}")))?;
            rec.packets_written += 1;
        }
        Ok(true)
    }

    pub async fn stop_recording(&self, app_id: AppId, recording_id: Uuid) -> Result<RecordingRow> {
        let (file_path, packets, audio_secs, audio_started_at) = {
            let mut st = self.active.lock();
            let owned = st.by_id.get(&recording_id).map(|r| r.app_id == app_id.0);
            match owned {
                None => return Err(AurixError::Recording("Recording not active".into())),
                Some(false) => {
                    return Err(AurixError::AuthorizationDenied(
                        "Recording belongs to another application".into(),
                    ))
                }
                Some(true) => {}
            }
            let mut active = st
                .remove(&recording_id)
                .ok_or_else(|| AurixError::Recording("Recording not active".into()))?;
            for (samples, pkt) in active.orderer.flush() {
                active
                    .writer
                    .write_packet_with_duration(
                        &pkt,
                        scale_samples(samples, active.writer.sample_rate()),
                    )
                    .map_err(|e| AurixError::Recording(format!("Write failed: {e}")))?;
                active.packets_written += 1;
            }
            active
                .writer
                .finish()
                .map_err(|e| AurixError::Recording(format!("Ogg finalize failed: {e}")))?;
            let secs = active.writer.granule() as f64 / active.writer.sample_rate().max(1) as f64;
            (
                active.file_path.clone(),
                active.packets_written,
                secs,
                active.audio_started_at,
            )
        };

        if let Some(ref key) = self.encryption_key {
            let plain = tokio::fs::read(&file_path)
                .await
                .map_err(|e| AurixError::Recording(format!("Read failed: {e}")))?;
            let encrypted = encrypt_blob(key, &plain)?;
            tokio::fs::write(&file_path, &encrypted)
                .await
                .map_err(|e| AurixError::Recording(format!("Encrypted write failed: {e}")))?;
        }

        let metadata = tokio::fs::metadata(&file_path)
            .await
            .map_err(|e| AurixError::Recording(format!("Metadata read failed: {e}")))?;
        let row_present = aurix_db::queries::finish_recording(
            &self.pool,
            recording_id,
            metadata.len() as i64,
            audio_secs,
            audio_started_at,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Recording finish failed: {e}")))?;
        if !row_present {
            discard_file(&file_path).await;
            return Err(AurixError::Recording(
                "Recording was erased while live; audio discarded".into(),
            ));
        }

        if let Some(ref s3) = self.s3 {
            let key = object_key(app_id, recording_id, processing::FORMAT_OGG_OPUS);
            match tokio::fs::read(&file_path).await {
                Ok(body) => match s3.put_object(&key, body, "audio/ogg").await {
                    Ok(()) => info!(
                        "Recording {} uploaded to object storage as {}",
                        recording_id, key
                    ),
                    Err(e) => warn!(
                        "Object storage upload failed for {}: {e} (file kept locally)",
                        recording_id
                    ),
                },
                Err(e) => warn!("Could not read {} for upload: {e}", file_path),
            }
        }

        info!(
            "Recording stopped: {} ({} packets, {:.1}s audio, {} bytes)",
            recording_id,
            packets,
            audio_secs,
            metadata.len()
        );
        aurix_db::queries::get_recording(&self.pool, app_id.0, recording_id)
            .await
            .map_err(|e| AurixError::Database(format!("Recording lookup failed: {e}")))?
            .ok_or_else(|| AurixError::Recording("Recording not found".into()))
    }

    /// Stop every active recording and live stream in a channel (e.g. when the channel is
    /// destroyed).
    pub async fn stop_channel(&self, app_id: AppId, channel_id: &ChannelId) -> usize {
        let ids: Vec<Uuid> = {
            let st = self.active.lock();
            st.by_id
                .iter()
                .filter(|(_, r)| r.channel_id == *channel_id)
                .map(|(id, _)| *id)
                .collect()
        };
        let mut n = self
            .live
            .stop_channel(app_id, channel_id, "channel_stopped");
        for id in ids {
            if self.stop_recording(app_id, id).await.is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Stop the recording of a user who left a channel.
    pub async fn on_user_left(&self, channel_id: ChannelId, user_id: UserId) {
        let target = {
            let st = self.active.lock();
            st.by_channel_user
                .get(&(channel_id, user_id))
                .and_then(|id| st.by_id.get(id).map(|r| (*id, AppId(r.app_id))))
        };
        if let Some((id, app_id)) = target {
            if let Err(e) = self.stop_recording(app_id, id).await {
                warn!("Failed to stop recording {} after user left: {e}", id);
            }
        }
    }

    /// Stop recordings that exceeded `max_recording_duration_secs`. Intended to run periodically.
    pub async fn enforce_duration_limit(&self) -> usize {
        let limit = Duration::seconds(self.config.max_recording_duration_secs as i64);
        let now = Utc::now();
        let expired: Vec<(Uuid, AppId)> = {
            let st = self.active.lock();
            st.by_id
                .iter()
                .filter(|(_, r)| now - r.started_at > limit)
                .map(|(id, r)| (*id, AppId(r.app_id)))
                .collect()
        };
        let mut n = 0;
        for (id, app_id) in expired {
            match self.stop_recording(app_id, id).await {
                Ok(_) => {
                    n += 1;
                    info!("Recording {} stopped: duration limit reached", id);
                }
                Err(e) => warn!("Failed to stop over-long recording {}: {e}", id),
            }
        }
        n
    }

    pub async fn get_recording(
        &self,
        app_id: AppId,
        recording_id: Uuid,
    ) -> Result<Option<RecordingRow>> {
        aurix_db::queries::get_recording(&self.pool, app_id.0, recording_id)
            .await
            .map_err(|e| AurixError::Database(format!("Recording lookup failed: {e}")))
    }

    pub async fn list_recordings(
        &self,
        app_id: AppId,
        channel_id: Option<ChannelId>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RecordingRow>> {
        aurix_db::queries::list_recordings(
            &self.pool,
            app_id.0,
            channel_id.map(|c| c.0),
            limit,
            offset,
        )
        .await
        .map_err(|e| AurixError::Database(format!("Recording list failed: {e}")))
    }

    /// Read a finished recording, decrypting it if it was encrypted at rest.
    pub async fn read_recording(
        &self,
        app_id: AppId,
        recording_id: Uuid,
    ) -> Result<(RecordingRow, Vec<u8>)> {
        let row = self
            .get_recording(app_id, recording_id)
            .await?
            .ok_or_else(|| AurixError::NotFound("Recording not found".into()))?;
        if row.status == "recording" || row.status == "processing" {
            return Err(AurixError::Conflict(
                "Recording is still in progress".into(),
            ));
        }
        if row.status == "failed" {
            return Err(AurixError::Conflict(format!(
                "Recording failed: {}",
                row.error.as_deref().unwrap_or("unknown error")
            )));
        }
        let bytes = tokio::fs::read(&row.file_path)
            .await
            .map_err(|e| AurixError::Recording(format!("Read failed: {e}")))?;
        if row.encrypted {
            if row.encryption_key_id != self.encryption_key_id {
                return Err(AurixError::Encryption(
                    "Recording was encrypted with a key that is not configured".into(),
                ));
            }
            let key = self
                .encryption_key
                .as_ref()
                .ok_or_else(|| AurixError::Encryption("No encryption key configured".into()))?;
            return Ok((row, decrypt_blob(key, &bytes)?));
        }
        Ok((row, bytes))
    }

    /// Presigned download URL when object storage is configured.
    pub fn presigned_url(
        &self,
        app_id: AppId,
        recording_id: Uuid,
        format: &str,
        expires_secs: u64,
    ) -> Result<Option<String>> {
        match self.s3 {
            Some(ref s3) => Ok(Some(s3.presigned_get_url(
                &object_key(app_id, recording_id, format),
                expires_secs,
                Utc::now(),
            )?)),
            None => Ok(None),
        }
    }

    pub async fn delete_recording(&self, app_id: AppId, recording_id: Uuid) -> Result<()> {
        let row = self
            .get_recording(app_id, recording_id)
            .await?
            .ok_or_else(|| AurixError::NotFound("Recording not found".into()))?;
        if self.active.lock().by_id.contains_key(&recording_id) {
            return Err(AurixError::Conflict(
                "Stop the recording before deleting it".into(),
            ));
        }
        self.remove_artifacts(&row).await;
        aurix_db::queries::delete_recording(&self.pool, recording_id)
            .await
            .map_err(|e| AurixError::Database(format!("Recording delete failed: {e}")))
    }

    async fn remove_artifacts(&self, row: &RecordingRow) {
        match tokio::fs::remove_file(&row.file_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to delete recording file {}: {e}", row.file_path),
        }
        if let Some(ref s3) = self.s3 {
            if let Err(e) = s3
                .delete_object(&object_key(AppId(row.app_id), row.id, &row.format))
                .await
            {
                warn!(
                    "Failed to delete recording {} from object storage: {e}",
                    row.id
                );
            }
        }
    }

    /// Retention: delete expired recordings in batches.
    pub async fn cleanup_expired(&self) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let batch = aurix_db::queries::list_expired_recordings(&self.pool, 100)
                .await
                .map_err(|e| AurixError::Database(format!("List expired failed: {e}")))?;
            if batch.is_empty() {
                break;
            }
            for rec in &batch {
                self.remove_artifacts(rec).await;
                aurix_db::queries::delete_recording(&self.pool, rec.id)
                    .await
                    .map_err(|e| AurixError::Database(format!("Recording delete failed: {e}")))?;
                total += 1;
            }
        }
        if total > 0 {
            info!("Cleaned up {} expired recordings", total);
        }
        Ok(total)
    }
}

impl AudioSink for RecordingService {
    fn wants_channel(&self, channel_id: &ChannelId) -> bool {
        self.active.lock().channels.contains(channel_id) || self.live.wants_channel(channel_id)
    }

    fn on_audio(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        ssrc: u32,
        rtp_timestamp: u32,
        payload: &[u8],
    ) {
        self.live
            .on_audio(channel_id, user_id, ssrc, rtp_timestamp, payload);
        if let Err(e) = self.write_opus_packet(channel_id, user_id, rtp_timestamp, payload) {
            warn!(
                "Recording write failed for user {} in {}: {e}",
                user_id, channel_id
            );
        }
    }

    fn on_participant_left(&self, channel_id: ChannelId, user_id: UserId) {
        self.live.on_participant_left(channel_id, user_id);
        let has = self
            .active
            .lock()
            .by_channel_user
            .contains_key(&(channel_id, user_id));
        if !has {
            return;
        }
        let active = self.active.clone();
        let pool = self.pool.clone();
        let key = self.encryption_key;
        tokio::spawn(async move {
            // Finalize inline without S3 upload: the sink runs on the media hot path and must not
            // block; the durable upload is retried by the periodic maintenance job if configured.
            let finished = {
                let mut st = active.lock();
                let Some(id) = st.by_channel_user.get(&(channel_id, user_id)).copied() else {
                    return;
                };
                let Some(mut rec) = st.remove(&id) else {
                    return;
                };
                for (samples, pkt) in rec.orderer.flush() {
                    let _ = rec.writer.write_packet_with_duration(
                        &pkt,
                        scale_samples(samples, rec.writer.sample_rate()),
                    );
                }
                let _ = rec.writer.finish();
                let secs = rec.writer.granule() as f64 / rec.writer.sample_rate().max(1) as f64;
                (id, rec.file_path, secs, rec.audio_started_at)
            };
            let (id, path, secs, audio_started_at) = finished;
            if let Some(key) = key {
                if let Ok(plain) = tokio::fs::read(&path).await {
                    if let Ok(enc) = encrypt_blob(&key, &plain) {
                        let _ = tokio::fs::write(&path, enc).await;
                    }
                }
            }
            let size = tokio::fs::metadata(&path)
                .await
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            match aurix_db::queries::finish_recording(&pool, id, size, secs, audio_started_at).await
            {
                Ok(true) => info!("Recording {} finished: participant left", id),
                Ok(false) => {
                    discard_file(&path).await;
                    info!("Recording {} erased while live; audio discarded", id);
                }
                Err(e) => warn!(
                    "Failed to finish recording {} after participant left: {e}",
                    id
                ),
            }
        });
    }
}

#[async_trait::async_trait]
impl aurix_common::sink::UserMediaPurger for RecordingService {
    async fn purge_user_media(&self, app_id: AppId, user_id: UserId) -> Result<u64> {
        self.live.purge_user(user_id);
        let live: Vec<Uuid> = {
            let st = self.active.lock();
            st.by_id
                .iter()
                .filter(|(_, r)| r.app_id == app_id.0 && r.user_id == user_id)
                .map(|(id, _)| *id)
                .collect()
        };
        for id in live {
            if let Err(e) = self.stop_recording(app_id, id).await {
                warn!("Failed to stop recording {id} before erasing its owner: {e}");
            }
        }
        let mut total = 0u64;
        loop {
            let rows =
                aurix_db::queries::list_recordings_for_user(&self.pool, app_id.0, user_id.0, 100)
                    .await
                    .map_err(|e| AurixError::Database(format!("Recording list failed: {e}")))?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                self.remove_derived(AppId(row.app_id), &[row.id]).await?;
                self.remove_artifacts(row).await;
                aurix_db::queries::delete_recording(&self.pool, row.id)
                    .await
                    .map_err(|e| AurixError::Database(format!("Recording delete failed: {e}")))?;
                total += 1;
            }
        }
        Ok(total)
    }
}

impl RecordingService {
    /// Erases mixdowns rendered from any of `source_ids` (their transcripts go with them).
    async fn remove_derived(&self, app_id: AppId, source_ids: &[Uuid]) -> Result<()> {
        let derived =
            aurix_db::queries::list_recordings_derived_from(&self.pool, app_id.0, source_ids)
                .await
                .map_err(|e| AurixError::Database(format!("Derived lookup failed: {e}")))?;
        for row in &derived {
            self.remove_artifacts(row).await;
            aurix_db::queries::delete_recording(&self.pool, row.id)
                .await
                .map_err(|e| AurixError::Database(format!("Recording delete failed: {e}")))?;
        }
        Ok(())
    }
}

async fn discard_file(path: &str) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("Failed to discard recording file {path}: {e}"),
    }
}

pub const RECORDING_KIND_RECORDING: &str = "recording";
pub const RECORDING_KIND_EVIDENCE: &str = "evidence";
pub const RECORDING_KIND_MIXDOWN: &str = "mixdown";

/// Longest evidence clip accepted from the safety pipeline (pre-roll + flagged segment).
const MAX_EVIDENCE_SECS: usize = 120;
const EVIDENCE_FRAME_MS: usize = 20;

#[async_trait::async_trait]
impl EvidenceStore for RecordingService {
    /// Encodes the clip to Ogg/Opus and stores it exactly like a stopped recording (encrypted
    /// at rest when a key is configured, mirrored to object storage, expiring after the
    /// safety retention), as a `recordings` row of kind `evidence`. Requires
    /// `recording.enabled`; the safety config validator enforces that pairing.
    async fn store_audio_evidence(&self, evidence: AudioEvidence) -> Result<StoredEvidence> {
        if !self.config.enabled {
            return Err(AurixError::InvalidConfiguration(
                "Stored recordings are disabled (recording.enabled)".into(),
            ));
        }
        let AudioEvidence {
            app_id,
            channel_id,
            session_id,
            user_id,
            pcm,
            sample_rate,
            started_at,
            retention_days,
        } = evidence;
        if pcm.is_empty() {
            return Err(AurixError::Recording("Evidence clip is empty".into()));
        }
        let sample_rate = match sample_rate {
            8_000 | 12_000 | 16_000 | 24_000 | 48_000 => sample_rate,
            other => {
                return Err(AurixError::Recording(format!(
                    "Unsupported evidence sample rate {other}"
                )))
            }
        };
        let frame = sample_rate as usize * EVIDENCE_FRAME_MS / 1000;
        let max_samples = sample_rate as usize * MAX_EVIDENCE_SECS;
        let pcm = if pcm.len() > max_samples {
            &pcm[pcm.len() - max_samples..]
        } else {
            &pcm[..]
        };

        let recording_id = Uuid::now_v7();
        let dir = PathBuf::from(&self.config.storage_path)
            .join(app_id.0.to_string())
            .join(channel_id.0.to_string());
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| AurixError::Recording(format!("Failed to create directory: {e}")))?;
        let file_path = dir
            .join(format!("{}_{}_evidence.ogg", recording_id, user_id.0))
            .to_string_lossy()
            .to_string();

        let pcm_owned = pcm.to_vec();
        let serial = crc32fast::hash(recording_id.as_bytes());
        let (mut ogg, duration_secs) = tokio::task::spawn_blocking(move || {
            encode_evidence_ogg(&pcm_owned, sample_rate, frame, serial)
        })
        .await
        .map_err(|e| AurixError::Recording(format!("Evidence encoder task failed: {e}")))??;
        if let Some(ref key) = self.encryption_key {
            ogg = encrypt_blob(key, &ogg)?;
        }
        tokio::fs::write(&file_path, &ogg)
            .await
            .map_err(|e| AurixError::Recording(format!("Evidence write failed: {e}")))?;

        let now = Utc::now();
        let row = RecordingRow {
            id: recording_id,
            app_id: app_id.0,
            channel_id: channel_id.0,
            session_id: session_id.0,
            user_id: user_id.0,
            file_path: file_path.clone(),
            file_size_bytes: ogg.len() as i64,
            duration_secs,
            format: "ogg_opus".to_string(),
            encrypted: self.encryption_key.is_some(),
            encryption_key_id: self.encryption_key_id.clone(),
            started_at,
            ended_at: Some(now),
            expires_at: now + Duration::days(retention_days.max(1) as i64),
            created_at: now,
            kind: RECORDING_KIND_EVIDENCE.to_string(),
            status: "ready".to_string(),
            audio_started_at: Some(started_at),
            sources: None,
            node_id: Some(self.node_id),
            error: None,
        };
        if let Err(e) = aurix_db::queries::create_recording(&self.pool, &row).await {
            discard_file(&file_path).await;
            return Err(AurixError::Database(format!(
                "Evidence row creation failed: {e}"
            )));
        }

        if let Some(ref s3) = self.s3 {
            let key = object_key(app_id, recording_id, processing::FORMAT_OGG_OPUS);
            match s3.put_object(&key, ogg.clone(), "audio/ogg").await {
                Ok(()) => info!(
                    "Evidence clip {} uploaded to object storage as {}",
                    recording_id, key
                ),
                Err(e) => warn!(
                    "Object storage upload failed for evidence {}: {e} (file kept locally)",
                    recording_id
                ),
            }
        }
        info!(
            "Evidence clip stored: {} ({:.1}s, {} bytes)",
            recording_id,
            duration_secs,
            ogg.len()
        );
        Ok(StoredEvidence {
            recording_id,
            duration_secs,
            size_bytes: ogg.len() as u64,
        })
    }
}

/// Encodes mono PCM into a complete Ogg/Opus stream; returns the bytes and the audio length.
fn encode_evidence_ogg(
    pcm: &[i16],
    sample_rate: u32,
    frame: usize,
    serial: u32,
) -> Result<(Vec<u8>, f64)> {
    let mut encoder =
        opus::Encoder::new(sample_rate, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|e| AurixError::Recording(format!("Opus encoder init failed: {e}")))?;
    let mut bytes = Vec::new();
    let mut writer = OggOpusWriter::new(&mut bytes, serial, sample_rate, 1)
        .map_err(|e| AurixError::Recording(format!("Failed to init Ogg writer: {e}")))?;
    let mut out = vec![0u8; 4000];
    let mut padded = vec![0i16; frame];
    for chunk in pcm.chunks(frame) {
        let input: &[i16] = if chunk.len() == frame {
            chunk
        } else {
            padded.fill(0);
            padded[..chunk.len()].copy_from_slice(chunk);
            &padded
        };
        let n = encoder
            .encode(input, &mut out)
            .map_err(|e| AurixError::Recording(format!("Opus encode failed: {e}")))?;
        writer
            .write_packet_with_duration(&out[..n], frame as u64)
            .map_err(|e| AurixError::Recording(format!("Write failed: {e}")))?;
    }
    writer
        .finish()
        .map_err(|e| AurixError::Recording(format!("Ogg finalize failed: {e}")))?;
    let secs = writer.granule() as f64 / sample_rate.max(1) as f64;
    drop(writer);
    Ok((bytes, secs))
}

fn object_key(app_id: AppId, recording_id: Uuid, format: &str) -> String {
    let ext = if format == processing::FORMAT_WAV {
        "wav"
    } else {
        "ogg"
    };
    format!("{}/{}.{ext}", app_id.0, recording_id)
}

/// RTP Opus always uses a 48 kHz clock; convert to the Ogg stream's sample rate.
fn scale_samples(samples_48k: u64, sample_rate: u32) -> u64 {
    if sample_rate as u64 == OPUS_CLOCK_RATE {
        samples_48k
    } else {
        samples_48k * sample_rate as u64 / OPUS_CLOCK_RATE
    }
}

fn encrypt_blob(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| AurixError::Encryption(format!("Cipher init failed: {e}")))?;
    let mut nonce_bytes = [0u8; 12];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, data)
        .map_err(|e| AurixError::Encryption(format!("Encryption failed: {e}")))?;
    let mut result = Vec::with_capacity(12 + encrypted.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&encrypted);
    Ok(result)
}

fn decrypt_blob(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 12 + 16 {
        return Err(AurixError::Encryption(
            "Encrypted recording too short".into(),
        ));
    }
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| AurixError::Encryption(format!("Cipher init failed: {e}")))?;
    let nonce = Nonce::from_slice(&data[..12]);
    cipher.decrypt(nonce, &data[12..]).map_err(|_| {
        AurixError::Encryption("Decryption failed (wrong key or corrupted file)".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orderer_reorders_and_preserves_gaps() {
        let mut o = PacketOrderer::new();
        // Out-of-order arrival: 960 arrives before 0.
        assert!(o.push(960, b"b").is_empty());
        assert!(o.push(0, b"a").is_empty());
        let mut all = Vec::new();
        for i in 2..7u32 {
            all.extend(o.push(i * 960, b"x"));
        }
        // 7 packets > depth 5 -> the two oldest were released, in order.
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].1, b"a".to_vec());
        assert_eq!(all[1].1, b"b".to_vec());
        assert_eq!(o.flush().len(), 5);
        // Gap: jump 1 second -> duration preserved.
        let mut o = PacketOrderer::new();
        o.push(0, b"a");
        o.push(48_000, b"b");
        let out = o.flush();
        assert_eq!(out[0].0, DEFAULT_FRAME_SAMPLES);
        assert_eq!(out[1].0, 48_000);
        // Late duplicate/older packet is dropped.
        assert!(o.push(0, b"late").is_empty());
        assert!(o.flush().is_empty());
    }

    #[test]
    fn orderer_handles_timestamp_wrap() {
        let mut o = PacketOrderer::new();
        o.push(u32::MAX - 959, b"a");
        o.push(0, b"b");
        let out = o.flush();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].1, b"b".to_vec());
        assert_eq!(out[1].0, 960);
    }

    #[test]
    fn encrypt_roundtrip_and_tamper_detection() {
        let key = [7u8; 32];
        let enc = encrypt_blob(&key, b"OggS...").unwrap();
        assert_eq!(decrypt_blob(&key, &enc).unwrap(), b"OggS...");
        let mut bad = enc.clone();
        bad[20] ^= 1;
        assert!(decrypt_blob(&key, &bad).is_err());
        assert!(decrypt_blob(&[8u8; 32], &enc).is_err());
    }
}
