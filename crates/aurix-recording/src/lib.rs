pub mod ogg;

use aurix_common::jitter_buffer::RecordingJitterBuffer;
use aes_gcm::{aead::{Aead, KeyInit}, Aes256Gcm, Nonce};
use aurix_common::config::RecordingConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use aurix_db::models::RecordingRow;
use aurix_db::DbPool;
use base64::Engine;
use chrono::{Duration, Utc};
use ogg::OggOpusWriter;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tracing::{info, warn};
use uuid::Uuid;

struct ActiveRecording {
    ogg_writer: OggOpusWriter<BufWriter<std::fs::File>>,
    file_path: String,
    user_id: Uuid,
    jitter_buffer: RecordingJitterBuffer,
}

pub struct RecordingService {
    pool: DbPool,
    config: RecordingConfig,
    encryption_key: Option<[u8; 32]>,
    active_writers: Arc<Mutex<HashMap<Uuid, ActiveRecording>>>,
}

impl RecordingService {
    pub fn new(pool: DbPool, config: RecordingConfig) -> Result<Self> {
        let encryption_key = if config.encryption_enabled {
            let key_str = config.encryption_key.as_ref().ok_or_else(|| {
                AurixError::InvalidConfiguration("Recording encryption enabled but no key provided".into())
            })?;
            let decoded = base64::engine::general_purpose::STANDARD.decode(key_str)
                .map_err(|e| AurixError::InvalidConfiguration(format!("Invalid encryption key: {e}")))?;
            if decoded.len() != 32 {
                return Err(AurixError::InvalidConfiguration("Encryption key must be 32 bytes".into()));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&decoded);
            Some(key)
        } else {
            None
        };
        Ok(Self { pool, config, encryption_key, active_writers: Arc::new(Mutex::new(HashMap::new())) })
    }

    /// Start a recording for a SINGLE user in a channel.
    /// Each user gets their own Ogg file (separate track per speaker).
    pub async fn start_recording(
        &self, app_id: AppId, channel_id: ChannelId, session_id: SessionId,
        user_id: UserId, sample_rate: u32, channels: u8,
    ) -> Result<RecordingRow> {
        let recording_id = Uuid::now_v7();
        let dir = PathBuf::from(&self.config.storage_path)
            .join(app_id.0.to_string())
            .join(channel_id.0.to_string());
        fs::create_dir_all(&dir).await
            .map_err(|e| AurixError::Recording(format!("Failed to create directory: {e}")))?;

        // Per-user file: {recording_id}_{user_id}.ogg
        let file_name = format!("{}_{}.ogg", recording_id, user_id.0);
        let file_path = dir.join(&file_name).to_string_lossy().to_string();
        let expires_at = Utc::now() + Duration::days(self.config.retention_days as i64);

        let row = RecordingRow {
            id: recording_id, app_id: app_id.0, channel_id: channel_id.0,
            session_id: session_id.0, user_id: user_id.0, file_path: file_path.clone(),
            file_size_bytes: 0, duration_secs: 0.0, format: "ogg_opus".to_string(),
            encrypted: self.config.encryption_enabled,
            encryption_key_id: self.encryption_key.as_ref().map(|_| "default".to_string()),
            started_at: Utc::now(), ended_at: None, expires_at, created_at: Utc::now(),
        };
        let created = aurix_db::queries::create_recording(&self.pool, &row).await
            .map_err(|e| AurixError::Database(format!("Recording creation failed: {e}")))?;

        let file = std::fs::File::create(&file_path)
            .map_err(|e| AurixError::Recording(format!("Failed to create file: {e}")))?;
        let serial = crc32fast::hash(recording_id.as_bytes());
        let ogg = OggOpusWriter::new(BufWriter::new(file), serial, sample_rate, channels)
            .map_err(|e| AurixError::Recording(format!("Failed to init Ogg writer: {e}")))?;

        self.active_writers.lock().insert(recording_id, ActiveRecording {
            ogg_writer: ogg, file_path, user_id: user_id.0,
            jitter_buffer: RecordingJitterBuffer::new(50), // 50 packet window ≈ 1 second at 20ms frames
        });
        info!("Recording started: {} for user {} -> {}", recording_id, user_id, file_name);
        Ok(created)
    }

    pub fn write_opus_packet(&self, recording_id: Uuid, user_id: Uuid, seq: u32, opus_data: &[u8]) -> Result<()> {
        let mut writers = self.active_writers.lock();
        let active = writers.get_mut(&recording_id)
            .ok_or_else(|| AurixError::Recording("Recording not active".into()))?;
        if active.user_id != user_id { return Ok(()); }

        // Insert into jitter buffer
        active.jitter_buffer.insert(seq, opus_data.to_vec());

        // Drain packets that are ready (in order)
        let ready = active.jitter_buffer.drain_ready();
        for (_seq, packet_data) in ready {
            active.ogg_writer.write_packet(&packet_data)
                .map_err(|e| AurixError::Recording(format!("Write failed: {e}")))?;
        }
        Ok(())
    }

    pub async fn stop_recording(&self, recording_id: Uuid) -> Result<()> {
        let file_path = {
            let mut writers = self.active_writers.lock();
            if let Some(mut active) = writers.remove(&recording_id) {
                active.ogg_writer.finish()
                    .map_err(|e| AurixError::Recording(format!("Ogg finalize failed: {e}")))?;
                active.file_path.clone()
            } else {
                return Err(AurixError::Recording("Recording not active".into()));
            }
        };

        // Encrypt at rest if enabled
        if let Some(ref key) = self.encryption_key {
            let plain: Vec<u8> = tokio::fs::read(&file_path).await
                .map_err(|e| AurixError::Recording(format!("Read failed: {e}")))?;
            let encrypted = self.encrypt_chunk(key, &plain)?;
            tokio::fs::write(&file_path, &encrypted).await
                .map_err(|e| AurixError::Recording(format!("Encrypted write failed: {e}")))?;
        }

        let metadata: std::fs::Metadata = tokio::fs::metadata(&file_path).await
            .map_err(|e| AurixError::Recording(format!("Metadata read failed: {e}")))?;

        let recording = aurix_db::queries::get_recording(&self.pool, recording_id).await
            .map_err(|e| AurixError::Database(format!("Recording lookup failed: {e}")))?
            .ok_or_else(|| AurixError::Recording("Recording not found in DB".into()))?;

        let duration = Utc::now().signed_duration_since(recording.started_at).num_seconds() as f64;
        aurix_db::queries::finish_recording(&self.pool, recording_id, metadata.len() as i64, duration).await
            .map_err(|e| AurixError::Database(format!("Recording finish failed: {e}")))?;

        // ── Upload to S3 if configured ──
        if let (Some(ref bucket), Some(ref region)) = (&self.config.s3_bucket, &self.config.s3_region) {
            if let Err(e) = self.upload_to_s3(&file_path, bucket, region, &recording.id.to_string()).await {
                warn!("S3 upload failed for recording {}: {e} (file kept on local disk)", recording_id);
            } else {
                info!("Recording {} uploaded to S3", recording_id);
            }
        }

        info!("Recording stopped: {} ({}s, {} bytes)", recording_id, duration, metadata.len());
        Ok(())
    }

    pub async fn get_recording(&self, recording_id: Uuid) -> Result<Option<RecordingRow>> {
        aurix_db::queries::get_recording(&self.pool, recording_id).await
            .map_err(|e| AurixError::Database(format!("Recording lookup failed: {e}")))
    }

    /// Batched cleanup that processes expired recordings in chunks to avoid OOM.
    pub async fn cleanup_expired(&self) -> Result<u64> {
        let mut total_deleted = 0u64;
        loop {
            let batch = aurix_db::queries::list_expired_recordings(&self.pool, 100).await
                .map_err(|e| AurixError::Database(format!("List expired failed: {e}")))?;

            if batch.is_empty() { break; }

            for rec in &batch {
                if let Err(e) = tokio::fs::remove_file(&rec.file_path).await {
                    warn!("Failed to delete recording file {}: {e}", rec.file_path);
                }
                let _ = aurix_db::queries::delete_recording(&self.pool, rec.id).await;
                total_deleted += 1;
            }
        }
        if total_deleted > 0 { info!("Cleaned up {} expired recordings", total_deleted); }
        Ok(total_deleted)
    }

    fn encrypt_chunk(&self, key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| AurixError::Encryption(format!("Cipher init failed: {e}")))?;
        let mut nonce_bytes = [0u8; 12];
        use rand::Rng;
        rand::thread_rng().fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher.encrypt(nonce, data)
            .map_err(|e| AurixError::Encryption(format!("Encryption failed: {e}")))?;
        let mut result = Vec::with_capacity(12 + encrypted.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&encrypted);
        Ok(result)
    }

    /// Simple S3 upload using pre-signed PUT (S3-compatible HTTP PUT).
    async fn upload_to_s3(&self, file_path: &str, bucket: &str, region: &str, key: &str) -> Result<()> {
        let default_endpoint = format!("https://s3.{}.amazonaws.com", region);
        let endpoint = self.config.s3_endpoint.as_deref()
            .unwrap_or(&default_endpoint);
        let url = format!("{}/{}/{}.ogg", endpoint, bucket, key);
        let body: Vec<u8> = tokio::fs::read(file_path).await
            .map_err(|e| AurixError::Recording(format!("S3 read failed: {e}")))?;
        let client = reqwest::Client::new();
        let resp = client.put(&url)
            .header("Content-Type", "audio/ogg")
            .body(body)
            .send().await
            .map_err(|e| AurixError::Recording(format!("S3 upload request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(AurixError::Recording(format!("S3 returned {}", resp.status())));
        }
        Ok(())
    }
}