//! Cross-node session mirror.
//!
//! Every node writes a compact copy of each live session into Redis (`session:{id}:mirror`)
//! and refreshes it while the session lives. When the node dies, the mirror outlives it for
//! `cluster.session_mirror_ttl_secs`; a client that reconnects to *another* node with the same
//! `<session_id>.<resume_token>` gets its session back there — same session id and SSRC, so
//! nobody in its channels sees a leave/join pair — with a fresh media key and endpoint.
//!
//! Ownership (`session:{id}:node`) is fenced: a node only writes a mirror it owns, and a
//! takeover is a compare-and-set from the previous owner to the new one, so two nodes cannot
//! both adopt the same session and a stale owner cannot overwrite a migrated session.

use aurix_common::protocol::TransmissionMode;
use aurix_common::types::*;
use serde::{Deserialize, Serialize};

/// Hex-encoded SHA-256 of the resume token (never the token itself).
pub type ResumeHashHex = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirroredChannel {
    pub channel_id: ChannelId,
    pub role: ChannelRole,
    #[serde(default)]
    pub priority: bool,
}

/// Receiver-side state that a takeover restores so the participant hears the same thing on
/// the new node. Persistent blocks are reloaded from the database instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirroredPrefs {
    #[serde(default)]
    pub local_mutes: Vec<(UserId, Option<ChannelId>)>,
    #[serde(default)]
    pub gains: Vec<(UserId, f32)>,
    #[serde(default)]
    pub transmission: TransmissionMode,
    #[serde(default)]
    pub focus: Option<ChannelId>,
    #[serde(default)]
    pub codec: AudioCodec,
    #[serde(default)]
    pub downlink: DownlinkMode,
    /// The client asked the node to denoise its uplink (`SetNoiseSuppression`).
    #[serde(default)]
    pub noise_suppression: bool,
    #[serde(default)]
    pub muted: bool,
    #[serde(default = "default_true")]
    pub transcripts: bool,
    /// Listener translation target (`SetTranslation.language`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoken_language: Option<String>,
    #[serde(default)]
    pub translation_speech: bool,
}

fn default_true() -> bool {
    true
}

impl Default for MirroredPrefs {
    fn default() -> Self {
        Self {
            local_mutes: Vec::new(),
            gains: Vec::new(),
            transmission: TransmissionMode::default(),
            focus: None,
            codec: AudioCodec::default(),
            downlink: DownlinkMode::default(),
            noise_suppression: false,
            muted: false,
            transcripts: true,
            translation: None,
            spoken_language: None,
            translation_speech: false,
        }
    }
}

/// How far ahead of the mirrored `audio_seq` the adopting node restarts the participant's
/// downlink audio sequence. Receivers keep a per-SSRC anti-replay window, so the stream must
/// continue *above* everything they have already accepted; the mirror is at most one refresh
/// period (≤ 60 s, ≤ 100 packets/s with FEC) old, and the jump itself is what tells their
/// jitter buffers to resynchronise.
pub const MIGRATION_SEQUENCE_GAP: u32 = 1 << 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMirror {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub ssrc: u32,
    /// Last per-sender downlink audio sequence the owner has handed out (`MediaSession::
    /// next_sequence`); the adopting node continues from `audio_seq + MIGRATION_SEQUENCE_GAP`.
    #[serde(default)]
    pub audio_seq: u32,
    pub resume_hash: ResumeHashHex,
    pub ip: String,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub channels: Vec<MirroredChannel>,
    #[serde(default)]
    pub prefs: MirroredPrefs,
    /// Unix milliseconds of the last write.
    pub updated_at: i64,
}

impl SessionMirror {
    pub fn resume_hash_hex(hash: &[u8; 32]) -> ResumeHashHex {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The presented token's hash matches the mirrored one (constant time).
    pub fn resume_hash_matches(&self, presented: &[u8; 32]) -> bool {
        let Ok(stored) = decode_hex(&self.resume_hash) else {
            return false;
        };
        aurix_common::crypto::constant_time_eq(&stored, presented)
    }
}

fn decode_hex(s: &str) -> Result<[u8; 32], ()> {
    if s.len() != 64 {
        return Err(());
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).ok_or(())?;
        let lo = (chunk[1] as char).to_digit(16).ok_or(())?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

/// Why a mirrored session could not be adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverRefused {
    /// No mirror (expired, never written, or the session was closed by its owner).
    NotMirrored,
    /// Mirror exists but the credential does not prove ownership (wrong user/app/token).
    Denied,
    /// Another node changed ownership between read and claim.
    Raced,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_hash_round_trips_and_compares() {
        let hash = [7u8; 32];
        let mirror = SessionMirror {
            session_id: SessionId::new(),
            user_id: UserId::new(),
            app_id: AppId::new(),
            display_name: "n".into(),
            ssrc: 1,
            audio_seq: 0,
            resume_hash: SessionMirror::resume_hash_hex(&hash),
            ip: "127.0.0.1".into(),
            user_agent: None,
            channels: vec![],
            prefs: MirroredPrefs::default(),
            updated_at: 0,
        };
        assert_eq!(mirror.resume_hash.len(), 64);
        assert!(mirror.resume_hash_matches(&hash));
        assert!(!mirror.resume_hash_matches(&[8u8; 32]));
        let json = serde_json::to_string(&mirror).unwrap();
        let back: SessionMirror = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mirror);
    }

    #[test]
    fn old_mirrors_without_prefs_deserialize_with_defaults() {
        let json = r#"{"session_id":"11111111-1111-4111-8111-111111111111","user_id":"22222222-2222-4222-8222-222222222222","app_id":"33333333-3333-4333-8333-333333333333","display_name":"x","ssrc":5,"resume_hash":"00","ip":"::1","updated_at":1}"#;
        let m: SessionMirror = serde_json::from_str(json).unwrap();
        assert!(m.channels.is_empty());
        assert!(m.prefs.transcripts);
        assert_eq!(m.prefs.codec, AudioCodec::Opus);
        assert!(!m.resume_hash_matches(&[0u8; 32]));
    }
}
