//! Optional server-side storage of live transcripts and their translations (`stt.persist`).
//!
//! The speech pipeline stores every transcript it delivers (`Transcript` event) before the
//! event leaves the node; the WebSocket layer of whichever node translated the segment for its
//! listeners stores each translation under the same transcript id (first writer wins, one row
//! per target language). Storage never gates delivery: a failed insert is logged and the
//! transcript still reaches the channel. Rows are readable through the operator REST API
//! (keyset pages, newest first), included in a user's data export, removed with the user and
//! swept by `stt.retention_days`.

use aurix_common::config::SttConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{
    decode_chat_cursor, encode_chat_cursor, Transcript, TranscriptWordTiming,
};
use aurix_common::types::{AppId, ChannelId, MediaNodeId, UserId};
use aurix_db::models::{TranscriptCursor, TranscriptRow, TranscriptTranslationRow};
use aurix_db::DbPool;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Default and hard maximum page size of the transcript lists.
pub const TRANSCRIPT_PAGE_DEFAULT: u32 = 50;
pub const TRANSCRIPT_PAGE_MAX: u32 = 200;
/// Rows removed per retention-sweep iteration.
const RETENTION_BATCH: i64 = 5_000;
/// Longest a live transcript waits for its row before it is delivered unstored.
pub const STORE_TIMEOUT: Duration = Duration::from_secs(2);

/// A stored translation of a transcript.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredTranslation {
    pub language: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

/// One stored transcript with the translations made of it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredTranscript {
    pub id: uuid::Uuid,
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<TranscriptWordTiming>,
    #[serde(default)]
    pub translations: Vec<StoredTranslation>,
    /// Node that transcribed the segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<MediaNodeId>,
    pub created_at: DateTime<Utc>,
}

impl StoredTranscript {
    /// Opaque page cursor addressing this transcript (same 24-byte keyset form as chat).
    pub fn cursor(&self) -> String {
        encode_chat_cursor(self.started_at, self.id)
    }
}

/// One page of stored transcripts, newest first.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TranscriptPage {
    pub transcripts: Vec<StoredTranscript>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

pub struct TranscriptStore {
    cfg: SttConfig,
    pool: DbPool,
    node_id: MediaNodeId,
}

impl TranscriptStore {
    pub fn new(cfg: SttConfig, pool: DbPool, node_id: MediaNodeId) -> Self {
        Self { cfg, pool, node_id }
    }

    /// Transcripts are being stored on this deployment (`stt.enabled && stt.persist`).
    pub fn enabled(&self) -> bool {
        self.cfg.enabled && self.cfg.persist
    }

    fn ensure_enabled(&self) -> Result<()> {
        if self.enabled() {
            Ok(())
        } else {
            Err(AurixError::NotFound(
                "Transcript storage is not enabled (stt.persist = false)".into(),
            ))
        }
    }

    /// Stores a transcript this node is about to deliver. The caller publishes the live event
    /// once this returns, so translations made anywhere in the fleet find their source row;
    /// the wait is bounded by [`STORE_TIMEOUT`] and errors are logged, never returned —
    /// storage must never drop the live event.
    pub async fn store(&self, app_id: AppId, transcript: &Transcript) {
        if !self.enabled() {
            return;
        }
        let row = TranscriptRow {
            id: transcript.id,
            app_id: app_id.0,
            channel_id: transcript.channel_id.0,
            user_id: transcript.user_id.0,
            text: transcript.text.clone(),
            language: transcript.language.clone(),
            started_at: transcript.started_at,
            duration_ms: i32::try_from(transcript.duration_ms).unwrap_or(i32::MAX),
            words: if transcript.words.is_empty() {
                None
            } else {
                serde_json::to_value(&transcript.words).ok()
            },
            node_id: Some(self.node_id.0),
            created_at: Utc::now(),
        };
        match tokio::time::timeout(
            STORE_TIMEOUT,
            aurix_db::queries::insert_transcript(&self.pool, &row),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(transcript = %transcript.id, "Storing transcript failed: {e}"),
            Err(_) => warn!(
                transcript = %transcript.id,
                "Storing transcript timed out after {STORE_TIMEOUT:?}; delivering unstored"
            ),
        }
    }

    /// Stores a translation of `transcript_id` into `language` (first writer wins).
    pub async fn store_translation(
        &self,
        app_id: AppId,
        transcript_id: uuid::Uuid,
        language: &str,
        text: &str,
    ) {
        if !self.enabled() {
            return;
        }
        match aurix_db::queries::insert_transcript_translation(
            &self.pool,
            app_id.0,
            transcript_id,
            language,
            text,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => debug!(
                transcript = %transcript_id,
                language,
                "Translation not stored (transcript row missing or language already stored)"
            ),
            Err(e) => warn!(transcript = %transcript_id, "Storing translation failed: {e}"),
        }
    }

    fn page_limit(requested: Option<u32>) -> i64 {
        i64::from(
            requested
                .unwrap_or(TRANSCRIPT_PAGE_DEFAULT)
                .clamp(1, TRANSCRIPT_PAGE_MAX),
        )
    }

    fn cursor(raw: Option<&str>, what: &str) -> Result<Option<TranscriptCursor>> {
        match raw {
            None => Ok(None),
            Some(c) => decode_chat_cursor(c)
                .map(|(started_at, id)| Some(TranscriptCursor { started_at, id }))
                .ok_or_else(|| AurixError::Validation(format!("Invalid `{what}` cursor"))),
        }
    }

    /// One page of a channel's stored transcripts (optionally of one speaker), newest first.
    /// `before` / `after` are cursors from an earlier page; without `before` the page is
    /// anchored at the present (or right after `after` when paging forward).
    pub async fn list_channel(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        user_id: Option<UserId>,
        before: Option<&str>,
        after: Option<&str>,
        limit: Option<u32>,
    ) -> Result<TranscriptPage> {
        self.ensure_enabled()?;
        let before = Self::cursor(before, "before")?;
        let after = Self::cursor(after, "after")?;
        let limit = Self::page_limit(limit);
        let forward = before.is_none() && after.is_some();
        let rows = aurix_db::queries::list_channel_transcripts(
            &self.pool,
            app_id.0,
            channel_id.0,
            user_id.map(|u| u.0),
            before,
            after,
            forward,
            limit + 1,
        )
        .await
        .map_err(|e| AurixError::Database(format!("transcripts query: {e}")))?;
        self.page(rows, before, after, forward, limit as usize)
            .await
    }

    /// One page of everything one user said across the application's channels.
    pub async fn list_user(
        &self,
        app_id: AppId,
        user_id: UserId,
        before: Option<&str>,
        after: Option<&str>,
        limit: Option<u32>,
    ) -> Result<TranscriptPage> {
        self.ensure_enabled()?;
        let before = Self::cursor(before, "before")?;
        let after = Self::cursor(after, "after")?;
        let limit = Self::page_limit(limit);
        let forward = before.is_none() && after.is_some();
        let rows = aurix_db::queries::list_user_transcripts(
            &self.pool,
            app_id.0,
            user_id.0,
            before,
            after,
            forward,
            limit + 1,
        )
        .await
        .map_err(|e| AurixError::Database(format!("transcripts query: {e}")))?;
        self.page(rows, before, after, forward, limit as usize)
            .await
    }

    async fn page(
        &self,
        mut rows: Vec<TranscriptRow>,
        before: Option<TranscriptCursor>,
        after: Option<TranscriptCursor>,
        forward: bool,
        limit: usize,
    ) -> Result<TranscriptPage> {
        let more = rows.len() > limit;
        rows.truncate(limit);
        if forward {
            rows.reverse();
        }
        let transcripts = self.with_translations(rows).await?;
        let newest = transcripts.first().map(StoredTranscript::cursor);
        let oldest = transcripts.last().map(StoredTranscript::cursor);
        let (older_exist, newer_exist) = if forward {
            (after.is_some(), more)
        } else {
            (more, before.is_some())
        };
        Ok(TranscriptPage {
            next_before: oldest.filter(|_| older_exist),
            next_after: newest.filter(|_| newer_exist),
            transcripts,
        })
    }

    /// Joins the stored translations onto the rows (one query for the whole page).
    pub async fn with_translations(
        &self,
        rows: Vec<TranscriptRow>,
    ) -> Result<Vec<StoredTranscript>> {
        let ids: Vec<uuid::Uuid> = rows.iter().map(|r| r.id).collect();
        let translations = aurix_db::queries::list_transcript_translations(&self.pool, &ids)
            .await
            .map_err(|e| AurixError::Database(format!("transcript_translations query: {e}")))?;
        Ok(join_translations(rows, translations))
    }

    /// Deletes one stored transcript (`NOT_FOUND` when it is not in `app_id`).
    pub async fn delete(&self, app_id: AppId, transcript_id: uuid::Uuid) -> Result<()> {
        self.ensure_enabled()?;
        let removed = aurix_db::queries::delete_transcript(&self.pool, app_id.0, transcript_id)
            .await
            .map_err(|e| AurixError::Database(format!("transcripts delete: {e}")))?;
        if removed {
            Ok(())
        } else {
            Err(AurixError::NotFound("Transcript not found".into()))
        }
    }

    /// Deletes every stored transcript of a channel; returns how many.
    pub async fn delete_channel(&self, app_id: AppId, channel_id: ChannelId) -> Result<u64> {
        self.ensure_enabled()?;
        aurix_db::queries::delete_channel_transcripts(&self.pool, app_id.0, channel_id.0)
            .await
            .map_err(|e| AurixError::Database(format!("transcripts delete: {e}")))
    }

    /// Removes transcripts that started before `cutoff` (all applications), returns the count.
    pub async fn sweep_before(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let mut total = 0;
        loop {
            let n =
                aurix_db::queries::delete_transcripts_before(&self.pool, cutoff, RETENTION_BATCH)
                    .await
                    .map_err(|e| AurixError::Database(format!("transcripts sweep: {e}")))?;
            total += n;
            if n < RETENTION_BATCH as u64 {
                return Ok(total);
            }
        }
    }

    /// Hourly sweep of stored transcripts older than `stt.retention_days`.
    pub fn start_retention_sweep(self: &Arc<Self>) {
        if !self.enabled() || self.cfg.retention_days == 0 {
            return;
        }
        let store = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let cutoff = Utc::now() - chrono::Duration::days(store.cfg.retention_days as i64);
                match store.sweep_before(cutoff).await {
                    Ok(n) if n > 0 => info!("Deleted {n} expired transcripts"),
                    Ok(_) => {}
                    Err(e) => warn!("Transcript retention sweep failed: {e}"),
                }
            }
        });
    }
}

/// Attaches translations to their transcripts, keeping the row order.
pub fn join_translations(
    rows: Vec<TranscriptRow>,
    translations: Vec<TranscriptTranslationRow>,
) -> Vec<StoredTranscript> {
    let mut by_id: std::collections::HashMap<uuid::Uuid, Vec<StoredTranslation>> =
        std::collections::HashMap::new();
    for t in translations {
        by_id
            .entry(t.transcript_id)
            .or_default()
            .push(StoredTranslation {
                language: t.language,
                text: t.text,
                created_at: t.created_at,
            });
    }
    rows.into_iter()
        .map(|r| StoredTranscript {
            translations: by_id.remove(&r.id).unwrap_or_default(),
            id: r.id,
            channel_id: ChannelId::from_uuid(r.channel_id),
            user_id: UserId::from_uuid(r.user_id),
            text: r.text,
            language: r.language,
            started_at: r.started_at,
            duration_ms: u64::try_from(r.duration_ms).unwrap_or(0),
            words: r
                .words
                .and_then(|w| serde_json::from_value(w).ok())
                .unwrap_or_default(),
            node_id: r.node_id.map(MediaNodeId::from_uuid),
            created_at: r.created_at,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u128, started: i64) -> TranscriptRow {
        TranscriptRow {
            id: uuid::Uuid::from_u128(id),
            app_id: uuid::Uuid::from_u128(1),
            channel_id: uuid::Uuid::from_u128(2),
            user_id: uuid::Uuid::from_u128(3),
            text: format!("t{id}"),
            language: Some("en".into()),
            started_at: DateTime::from_timestamp(started, 0).unwrap(),
            duration_ms: 1200,
            words: Some(serde_json::json!([{"word": "hi", "start_ms": 0, "end_ms": 300}])),
            node_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn translations_join_onto_their_transcript_in_row_order() {
        let rows = vec![row(10, 200), row(11, 100)];
        let translations = vec![
            TranscriptTranslationRow {
                transcript_id: uuid::Uuid::from_u128(11),
                language: "de".into(),
                text: "hallo".into(),
                created_at: Utc::now(),
            },
            TranscriptTranslationRow {
                transcript_id: uuid::Uuid::from_u128(11),
                language: "ru".into(),
                text: "привет".into(),
                created_at: Utc::now(),
            },
            TranscriptTranslationRow {
                transcript_id: uuid::Uuid::from_u128(99),
                language: "fr".into(),
                text: "orphan".into(),
                created_at: Utc::now(),
            },
        ];
        let joined = join_translations(rows, translations);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0].id, uuid::Uuid::from_u128(10));
        assert!(joined[0].translations.is_empty());
        assert_eq!(joined[0].words.len(), 1);
        assert_eq!(joined[0].words[0].word, "hi");
        let langs: Vec<&str> = joined[1]
            .translations
            .iter()
            .map(|t| t.language.as_str())
            .collect();
        assert_eq!(langs, ["de", "ru"]);
        assert_eq!(joined[1].duration_ms, 1200);
    }

    #[test]
    fn cursor_round_trips_and_page_limit_is_clamped() {
        let t = join_translations(vec![row(7, 1_700_000_000)], Vec::new()).remove(0);
        let c = t.cursor();
        let decoded = TranscriptStore::cursor(Some(&c), "before")
            .unwrap()
            .unwrap();
        assert_eq!(decoded.id, t.id);
        assert_eq!(decoded.started_at, t.started_at);
        assert!(TranscriptStore::cursor(Some("not-a-cursor"), "after").is_err());
        assert_eq!(TranscriptStore::page_limit(None), 50);
        assert_eq!(TranscriptStore::page_limit(Some(0)), 1);
        assert_eq!(TranscriptStore::page_limit(Some(10_000)), 200);
    }
}
