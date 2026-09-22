//! Fleet directory of live streams (`live_streams` table).
//!
//! Every node publishes the streams it owns and refreshes their status; other nodes read the
//! rows to list/stop the fleet's streams through their own API and the cascade planner
//! forwards a channel's audio to every node that has a stream on it. Rows are advisory
//! (status is at most one refresh interval stale); the owning node stays authoritative.

use crate::live::{LiveStreamInfo, LiveStreams};
use aurix_db::models::LiveStreamRow;
use aurix_db::DbPool;
use tracing::warn;
use uuid::Uuid;

pub fn row_of(info: &LiveStreamInfo) -> LiveStreamRow {
    LiveStreamRow {
        id: info.id,
        node_id: info.node_id,
        app_id: info.app_id,
        channel_id: info.channel_id.0,
        mode: info.mode.as_str().to_string(),
        format: info.format.as_str().to_string(),
        mix: info.mix,
        state: serde_json::to_value(info.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "streaming".to_string()),
        users: info
            .users
            .as_ref()
            .and_then(|u| serde_json::to_value(u).ok()),
        label: info.label.clone(),
        push_url: info.push_url.clone(),
        started_at: info.started_at,
        updated_at: chrono::Utc::now(),
        frames_sent: info.frames_sent.min(i64::MAX as u64) as i64,
        frames_dropped: info.frames_dropped.min(i64::MAX as u64) as i64,
        reconnects: info.reconnects.min(i32::MAX as u32) as i32,
        consent: serde_json::to_value(&info.consent).unwrap_or(serde_json::Value::Null),
    }
}

/// The published status of a stream owned by another node. `None` for rows written by a
/// newer node whose enums this build does not know.
pub fn info_of(row: LiveStreamRow) -> Option<LiveStreamInfo> {
    serde_json::from_value(serde_json::json!({
        "id": row.id,
        "app_id": row.app_id,
        "channel_id": row.channel_id,
        "node_id": row.node_id,
        "mode": row.mode,
        "format": row.format,
        "mix": row.mix,
        "state": row.state,
        "users": row.users,
        "label": row.label,
        "push_url": row.push_url,
        "started_at": row.started_at,
        "frames_sent": row.frames_sent.max(0),
        "frames_dropped": row.frames_dropped.max(0),
        "reconnects": row.reconnects.max(0),
        "consent": if row.consent.is_object() { row.consent } else { serde_json::json!({}) },
    }))
    .ok()
}

pub async fn publish(pool: &DbPool, info: &LiveStreamInfo) {
    if let Err(e) = aurix_db::queries::upsert_live_stream(pool, &row_of(info)).await {
        warn!(
            "Live stream {} not published to the fleet directory: {e}",
            info.id
        );
    }
}

pub async fn remove(pool: &DbPool, id: Uuid) {
    if let Err(e) = aurix_db::queries::delete_live_stream(pool, id).await {
        warn!("Live stream {id} not removed from the fleet directory: {e}");
    }
}

/// Refreshes every stream this node owns and drops directory rows it no longer has (a
/// crash between `remove` and the row, or rows written before a restart with the same id).
pub async fn sync(pool: &DbPool, live: &LiveStreams) {
    let streams = live.list_all();
    for info in &streams {
        publish(pool, info).await;
    }
    let keep: Vec<Uuid> = streams.iter().map(|s| s.id).collect();
    if let Err(e) = aurix_db::queries::prune_live_streams(pool, live.node_id(), &keep).await {
        warn!("Live stream directory prune failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{StreamFormat, StreamMode, StreamState};
    use aurix_common::types::{ChannelId, RecordingConsent, UserId};
    use std::collections::HashMap;

    #[test]
    fn row_roundtrip_keeps_every_field() {
        let user = UserId(Uuid::new_v4());
        let mut consent = HashMap::new();
        consent.insert(user, RecordingConsent::Accepted);
        let info = LiveStreamInfo {
            id: Uuid::new_v4(),
            app_id: Uuid::new_v4(),
            channel_id: ChannelId(Uuid::new_v4()),
            node_id: Uuid::new_v4(),
            mode: StreamMode::Push,
            format: StreamFormat::PcmS16le,
            mix: true,
            state: StreamState::Reconnecting,
            users: Some(vec![user]),
            label: Some("ops".into()),
            push_url: Some("wss://sink.example/live".into()),
            started_at: chrono::Utc::now(),
            frames_sent: 12_345,
            frames_dropped: 7,
            reconnects: 2,
            consent,
        };
        let row = row_of(&info);
        assert_eq!(row.mode, "push");
        assert_eq!(row.format, "pcm_s16le");
        assert_eq!(row.state, "reconnecting");
        assert_eq!(row.frames_sent, 12_345);
        let back = info_of(row).expect("row converts back");
        assert_eq!(back.id, info.id);
        assert_eq!(back.node_id, info.node_id);
        assert_eq!(back.channel_id, info.channel_id);
        assert_eq!(back.mode, info.mode);
        assert_eq!(back.format, info.format);
        assert!(back.mix);
        assert_eq!(back.state, StreamState::Reconnecting);
        assert_eq!(back.users, info.users);
        assert_eq!(back.label, info.label);
        assert_eq!(back.push_url, info.push_url);
        assert_eq!(back.started_at, info.started_at);
        assert_eq!(back.frames_sent, 12_345);
        assert_eq!(back.frames_dropped, 7);
        assert_eq!(back.reconnects, 2);
        assert_eq!(back.consent.get(&user), Some(&RecordingConsent::Accepted));

        // Rows written by an unknown future state are skipped rather than mis-reported.
        let mut bad = row_of(&info);
        bad.state = "teleporting".into();
        assert!(info_of(bad).is_none());
    }
}
