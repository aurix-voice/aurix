use aurix_common::crypto::MediaKeys;
use aurix_common::error::AurixError;
use aurix_common::protocol::{
    decode_audio_level, ReplayWindow, TransmissionMode, AUDIO_LEVEL_SILENCE,
};
use aurix_common::types::*;

use crate::quality::{UplinkEstimator, UplinkSample};
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Upper bound for a per-participant gain (1.0 = as sent, 2.0 = +6 dB).
pub const MAX_PARTICIPANT_GAIN: f32 = 2.0;

/// Gain for channels other than the focused one when the node does not configure
/// `media.unfocused_channel_gain`.
pub const DEFAULT_UNFOCUSED_GAIN: f32 = 0.5;

/// What this participant wants to hear: local ("for me") mutes, per-sender gain and the
/// persistent cross-mute list, plus the focused channel (every other channel is attenuated by
/// `unfocused_gain`). Evaluated per packet on the receiver side of the fan-out, so a muted or
/// blocked sender's audio never leaves the server towards this session.
#[derive(Debug)]
pub struct ReceiverPrefs {
    muted_everywhere: HashSet<UserId>,
    muted_in: HashSet<(ChannelId, UserId)>,
    gain: HashMap<UserId, f32>,
    /// Users this participant blocked (persisted in `user_blocks`).
    blocked: HashSet<UserId>,
    /// Users who blocked this participant; cross-mute is mutual so they are silenced too.
    blocked_by: HashSet<UserId>,
    focus: Option<ChannelId>,
    unfocused_gain: f32,
}

impl Default for ReceiverPrefs {
    fn default() -> Self {
        Self {
            muted_everywhere: HashSet::new(),
            muted_in: HashSet::new(),
            gain: HashMap::new(),
            blocked: HashSet::new(),
            blocked_by: HashSet::new(),
            focus: None,
            unfocused_gain: DEFAULT_UNFOCUSED_GAIN,
        }
    }
}

impl ReceiverPrefs {
    pub fn with_unfocused_gain(unfocused_gain: f32) -> Self {
        Self {
            unfocused_gain: clamp_unit_gain(unfocused_gain, DEFAULT_UNFOCUSED_GAIN),
            ..Self::default()
        }
    }

    /// Gain multiplier applied to audio from `sender` in `channel`, or `None` when it must be
    /// dropped for this receiver.
    pub fn gain_for(&self, sender: &UserId, channel: &ChannelId) -> Option<f32> {
        if self.blocked.contains(sender)
            || self.blocked_by.contains(sender)
            || self.muted_everywhere.contains(sender)
            || self.muted_in.contains(&(*channel, *sender))
        {
            return None;
        }
        let sender_gain = self.gain.get(sender).copied().unwrap_or(1.0);
        Some(sender_gain * self.focus_gain(channel))
    }

    /// `1.0` for the focused channel (or when nothing is focused), `unfocused_gain` otherwise.
    pub fn focus_gain(&self, channel: &ChannelId) -> f32 {
        match self.focus {
            Some(focused) if focused != *channel => self.unfocused_gain,
            _ => 1.0,
        }
    }

    pub fn set_focus(&mut self, channel: Option<ChannelId>) {
        self.focus = channel;
    }

    pub fn focus(&self) -> Option<ChannelId> {
        self.focus
    }

    pub fn set_muted(&mut self, sender: UserId, channel: Option<ChannelId>, muted: bool) {
        match (channel, muted) {
            (None, true) => {
                self.muted_everywhere.insert(sender);
            }
            (None, false) => {
                self.muted_everywhere.remove(&sender);
                self.muted_in.retain(|(_, u)| *u != sender);
            }
            (Some(ch), true) => {
                self.muted_in.insert((ch, sender));
            }
            (Some(ch), false) => {
                self.muted_in.remove(&(ch, sender));
            }
        }
    }

    pub fn set_gain(&mut self, sender: UserId, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, MAX_PARTICIPANT_GAIN)
        } else {
            1.0
        };
        if (gain - 1.0).abs() < f32::EPSILON {
            self.gain.remove(&sender);
        } else {
            self.gain.insert(sender, gain);
        }
    }

    pub fn set_blocked(&mut self, user: UserId, blocked: bool) {
        if blocked {
            self.blocked.insert(user);
        } else {
            self.blocked.remove(&user);
        }
    }

    pub fn set_blocked_by(&mut self, user: UserId, blocked: bool) {
        if blocked {
            self.blocked_by.insert(user);
        } else {
            self.blocked_by.remove(&user);
        }
    }

    pub fn load_blocks(
        &mut self,
        blocked: impl IntoIterator<Item = UserId>,
        blocked_by: impl IntoIterator<Item = UserId>,
    ) {
        self.blocked = blocked.into_iter().collect();
        self.blocked_by = blocked_by.into_iter().collect();
    }

    pub fn blocked_users(&self) -> Vec<UserId> {
        let mut v: Vec<UserId> = self.blocked.iter().copied().collect();
        v.sort_unstable_by_key(|u| u.0);
        v
    }

    pub fn gains(&self) -> Vec<(UserId, f32)> {
        self.gain.iter().map(|(u, g)| (*u, *g)).collect()
    }

    pub fn is_blocked(&self, user: &UserId) -> bool {
        self.blocked.contains(user)
    }

    /// True when a persistent block exists in either direction; such pairs exchange neither
    /// audio nor text.
    pub fn is_blocked_either_way(&self, user: &UserId) -> bool {
        self.blocked.contains(user) || self.blocked_by.contains(user)
    }

    /// Muted-for-me senders, `(user, channel)` with `None` meaning every channel.
    pub fn local_mutes(&self) -> Vec<(UserId, Option<ChannelId>)> {
        self.muted_everywhere
            .iter()
            .map(|u| (*u, None))
            .chain(self.muted_in.iter().map(|(c, u)| (*u, Some(*c))))
            .collect()
    }
}

fn clamp_unit_gain(gain: f32, fallback: f32) -> f32 {
    if gain.is_finite() {
        gain.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

/// How a participant's media reaches the SFU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Native AURX/UDP client (authenticated with the per-session media key).
    Aurx,
    /// Browser/WebRTC client (media arrives via str0m, downlink is a mixed track).
    WebRtc,
}

#[derive(Debug)]
pub struct MediaSession {
    pub session_id: SessionId,
    pub user_id: UserId,
    pub app_id: AppId,
    pub display_name: String,
    pub ssrc: u32,
    /// Per-session master media key handed to the client over the authenticated control channel.
    pub media_key: [u8; 32],
    /// Authentication/encryption keys derived from `media_key` (see `MediaKeys`).
    pub keys: MediaKeys,
    pub transport: RwLock<Transport>,
    pub remote_addr: RwLock<Option<SocketAddr>>,
    pub channels: RwLock<Vec<ChannelId>>,
    pub is_muted: AtomicBool,
    pub is_server_muted: AtomicBool,
    pub is_speaking: AtomicBool,
    pub prefs: RwLock<ReceiverPrefs>,
    /// Which joined channels receive this session's uplink (`All` by default).
    pub transmission: RwLock<TransmissionMode>,
    pub sequence: AtomicU32,
    /// Sequence counter for server-originated packets addressed to this session
    /// (acks, commands); keeps their encryption IVs unique under the session key.
    pub downlink_sequence: AtomicU32,
    pub last_audio_timestamp: AtomicU64,
    /// Wall-clock ms of the last audio packet, used for the speaking timeout.
    pub last_audio_at_ms: AtomicI64,
    /// Latest sender-reported audio level (`-dBov`, `AUDIO_LEVEL_SILENCE` when unknown/quiet)
    /// and when it was measured; `energy_reported` is the last level sent in `ChannelEnergy`.
    pub audio_level: AtomicU8,
    pub audio_level_at_ms: AtomicI64,
    pub energy_reported: AtomicU8,
    pub last_heartbeat: RwLock<DateTime<Utc>>,
    pub quality: RwLock<QualityMetrics>,
    /// Server-measured uplink (fed by the router) and the latest merged report sent to the
    /// client / shown to operators.
    pub uplink: Mutex<UplinkEstimator>,
    pub network_quality: RwLock<Option<NetworkQuality>>,
    pub created_at: DateTime<Utc>,
    pub replay: Mutex<ReplayWindow>,
    /// Highest `SessionBind` timestamp accepted so far (rejects replayed binds).
    pub last_bind_ms: AtomicI64,
    /// Browser RTP SSRC (WebRTC transport only), learned from the first RTP packet.
    pub webrtc_ssrc: AtomicU32,
    active: AtomicBool,
    packets_sent: AtomicU64,
    packets_received: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
}

impl MediaSession {
    pub fn new(
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
        display_name: String,
        ssrc: u32,
        media_key: [u8; 32],
    ) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            user_id,
            app_id,
            display_name,
            ssrc,
            media_key,
            keys: MediaKeys::derive(&media_key),
            transport: RwLock::new(Transport::Aurx),
            remote_addr: RwLock::new(None),
            channels: RwLock::new(Vec::new()),
            is_muted: AtomicBool::new(false),
            is_server_muted: AtomicBool::new(false),
            is_speaking: AtomicBool::new(false),
            prefs: RwLock::new(ReceiverPrefs::default()),
            transmission: RwLock::new(TransmissionMode::All),
            sequence: AtomicU32::new(0),
            downlink_sequence: AtomicU32::new(0),
            last_audio_timestamp: AtomicU64::new(0),
            last_audio_at_ms: AtomicI64::new(0),
            audio_level: AtomicU8::new(AUDIO_LEVEL_SILENCE),
            audio_level_at_ms: AtomicI64::new(0),
            energy_reported: AtomicU8::new(AUDIO_LEVEL_SILENCE),
            last_heartbeat: RwLock::new(Utc::now()),
            quality: RwLock::new(QualityMetrics {
                rtt_ms: 0.0,
                jitter_ms: 0.0,
                packet_loss_percent: 0.0,
                bitrate_kbps: 0,
                mos_score: 4.5,
            }),
            uplink: Mutex::new(UplinkEstimator::default()),
            network_quality: RwLock::new(None),
            created_at: Utc::now(),
            replay: Mutex::new(ReplayWindow::default()),
            last_bind_ms: AtomicI64::new(i64::MIN),
            webrtc_ssrc: AtomicU32::new(0),
            active: AtomicBool::new(true),
            packets_sent: AtomicU64::new(0),
            packets_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
        })
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn next_downlink_sequence(&self) -> u32 {
        self.downlink_sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
    }

    pub fn transport(&self) -> Transport {
        *self.transport.read()
    }

    pub fn set_transport(&self, t: Transport) {
        *self.transport.write() = t;
    }

    /// True once the UDP source address has been authenticated via `SessionBind`
    /// (or the WebRTC ICE session connected).
    pub fn is_bound(&self) -> bool {
        self.remote_addr.read().is_some()
    }

    pub fn set_remote_addr(&self, addr: SocketAddr) {
        *self.remote_addr.write() = Some(addr);
    }

    pub fn clear_remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr.write().take()
    }

    pub fn get_remote_addr(&self) -> Option<SocketAddr> {
        *self.remote_addr.read()
    }

    pub fn join_channel(&self, channel_id: ChannelId) {
        let mut channels = self.channels.write();
        if !channels.contains(&channel_id) {
            channels.push(channel_id);
        }
    }

    pub fn leave_channel(&self, channel_id: &ChannelId) {
        let mut channels = self.channels.write();
        channels.retain(|c| c != channel_id);
    }

    /// Drops routing state that referenced `channel_id`: a `Single` transmission targeting it
    /// falls back to `None` (nothing is sent to a channel the client did not choose) and focus
    /// on it is cleared. Returns `(transmission_changed, focus_changed)`.
    pub fn forget_channel(&self, channel_id: &ChannelId) -> (bool, bool) {
        let mut transmission = self.transmission.write();
        let transmission_changed = matches!(
            *transmission,
            TransmissionMode::Single { channel_id: target } if target == *channel_id
        );
        if transmission_changed {
            *transmission = TransmissionMode::None;
        }
        drop(transmission);
        let mut prefs = self.prefs.write();
        let focus_changed = prefs.focus() == Some(*channel_id);
        if focus_changed {
            prefs.set_focus(None);
        }
        (transmission_changed, focus_changed)
    }

    pub fn transmission(&self) -> TransmissionMode {
        *self.transmission.read()
    }

    /// Sets the transmission mode; `Single` must name a channel this session is joined to.
    pub fn set_transmission(&self, mode: TransmissionMode) -> Result<(), AurixError> {
        if let TransmissionMode::Single { channel_id } = &mode {
            if !self.is_in_channel(channel_id) {
                return Err(AurixError::ChannelNotFound(
                    "transmission target is not a joined channel".into(),
                ));
            }
        }
        *self.transmission.write() = mode;
        Ok(())
    }

    /// True when this session's uplink may be forwarded into `channel_id`.
    pub fn transmits_to(&self, channel_id: &ChannelId) -> bool {
        self.transmission.read().allows(channel_id)
    }

    /// Focuses a joined channel (others are attenuated) or clears focus with `None`.
    pub fn set_focus(&self, channel_id: Option<ChannelId>) -> Result<(), AurixError> {
        if let Some(channel_id) = &channel_id {
            if !self.is_in_channel(channel_id) {
                return Err(AurixError::ChannelNotFound(
                    "focus target is not a joined channel".into(),
                ));
            }
        }
        self.prefs.write().set_focus(channel_id);
        Ok(())
    }

    pub fn focus(&self) -> Option<ChannelId> {
        self.prefs.read().focus()
    }

    /// Gain this session applies to `channel` from its focus alone (no sender involved).
    pub fn focus_gain(&self, channel: &ChannelId) -> f32 {
        self.prefs.read().focus_gain(channel)
    }

    pub fn is_in_channel(&self, channel_id: &ChannelId) -> bool {
        self.channels.read().contains(channel_id)
    }

    pub fn get_channels(&self) -> Vec<ChannelId> {
        self.channels.read().clone()
    }

    pub fn update_heartbeat(&self) {
        *self.last_heartbeat.write() = Utc::now();
    }

    pub fn heartbeat_age_secs(&self) -> i64 {
        Utc::now()
            .signed_duration_since(*self.last_heartbeat.read())
            .num_seconds()
    }

    pub fn record_packet_sent(&self, bytes: u64) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_packet_received(&self, bytes: u64) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Anti-replay check for an authenticated packet's sequence number.
    pub fn accept_sequence(&self, seq: u32) -> bool {
        self.replay.lock().check_and_update(seq)
    }

    /// Record an audio frame that counts as voice. Returns `true` if the speaking state
    /// flipped to true.
    pub fn mark_audio_activity(&self) -> bool {
        self.last_audio_at_ms
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
        !self.is_speaking.swap(true, Ordering::Relaxed)
    }

    /// Record the sender-measured level of an incoming frame (`None` = unlabeled frame). Returns
    /// `true` if the speaking state flipped to true. Labeled frames below `threshold` (linear
    /// energy) are silence: they refresh the level but let the speaking state expire.
    pub fn record_audio_level(&self, level: Option<u8>, threshold: f32) -> bool {
        let now = Utc::now().timestamp_millis();
        match level {
            None => self.mark_audio_activity(),
            Some(level) => {
                let level = level.min(AUDIO_LEVEL_SILENCE);
                self.audio_level.store(level, Ordering::Relaxed);
                self.audio_level_at_ms.store(now, Ordering::Relaxed);
                if decode_audio_level(level) >= threshold && level < AUDIO_LEVEL_SILENCE {
                    self.mark_audio_activity()
                } else {
                    false
                }
            }
        }
    }

    /// Current level for reporting: the last measured one, or silence once no labeled frame
    /// arrived within `stale_ms`.
    pub fn current_audio_level(&self, stale_ms: i64) -> u8 {
        let at = self.audio_level_at_ms.load(Ordering::Relaxed);
        if Utc::now().timestamp_millis() - at > stale_ms {
            AUDIO_LEVEL_SILENCE
        } else {
            self.audio_level.load(Ordering::Relaxed)
        }
    }

    /// Level to include in the next `ChannelEnergy` report, if it moved enough since the last
    /// one (>= `min_step` dB, or any transition to/from silence).
    pub fn take_energy_report(&self, stale_ms: i64, min_step: u8) -> Option<u8> {
        let current = self.current_audio_level(stale_ms);
        let reported = self.energy_reported.load(Ordering::Relaxed);
        if current == reported {
            return None;
        }
        let silence_edge = (current == AUDIO_LEVEL_SILENCE) != (reported == AUDIO_LEVEL_SILENCE);
        if !silence_edge && current.abs_diff(reported) < min_step {
            return None;
        }
        self.energy_reported.store(current, Ordering::Relaxed);
        Some(current)
    }

    /// Forget the last reported level so the next report carries the current one even if it
    /// did not change (used when a new member joins and has no baseline yet).
    pub fn reset_energy_report(&self) {
        self.energy_reported
            .store(AUDIO_LEVEL_SILENCE, Ordering::Relaxed);
    }

    /// Clear speaking if no audio arrived within `timeout_ms`. Returns `true` if it flipped to false.
    pub fn expire_speaking(&self, timeout_ms: i64) -> bool {
        if !self.is_speaking.load(Ordering::Relaxed) {
            return false;
        }
        let last = self.last_audio_at_ms.load(Ordering::Relaxed);
        if Utc::now().timestamp_millis() - last > timeout_ms {
            self.is_speaking.swap(false, Ordering::Relaxed)
        } else {
            false
        }
    }

    pub fn is_transmitting_allowed(&self) -> bool {
        self.is_active()
            && !self.is_muted.load(Ordering::Relaxed)
            && !self.is_server_muted.load(Ordering::Relaxed)
    }

    /// Gain this session applies to audio from `sender` in `channel`, `None` if it is silenced.
    pub fn gain_for(&self, sender: &UserId, channel: &ChannelId) -> Option<f32> {
        self.prefs.read().gain_for(sender, channel)
    }

    pub fn update_quality(&self, metrics: QualityMetrics) {
        *self.quality.write() = metrics;
    }

    pub fn get_quality(&self) -> QualityMetrics {
        self.quality.read().clone()
    }

    /// Account for an authenticated uplink packet (see `UplinkEstimator::record`).
    pub fn record_uplink(&self, seq: u64, rtp_ts: Option<u32>, bytes: usize) {
        self.uplink
            .lock()
            .record(seq, rtp_ts, bytes, std::time::Instant::now());
    }

    /// Close the current uplink interval and merge it with the client's last `QualityReport`.
    /// Returns the new report and whether its bar count differs from the previous one.
    pub fn refresh_network_quality(&self) -> (NetworkQuality, bool) {
        let sample: UplinkSample = self.uplink.lock().sample(std::time::Instant::now());
        let client = self.get_quality();
        let quality = NetworkQuality::compose(
            &client,
            sample.jitter_ms,
            sample.loss_percent,
            sample.bitrate_kbps,
            sample.packets_received,
            sample.packets_lost,
        );
        let mut slot = self.network_quality.write();
        let changed = slot.map(|q| q.bars) != Some(quality.bars);
        *slot = Some(quality);
        (quality, changed)
    }

    pub fn get_network_quality(&self) -> Option<NetworkQuality> {
        *self.network_quality.read()
    }

    pub fn next_sequence(&self) -> u32 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub fn stats(&self) -> SessionStats {
        SessionStats {
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            packets_received: self.packets_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            quality: self.get_quality(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub quality: QualityMetrics,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Arc<MediaSession> {
        MediaSession::new(
            SessionId::new(),
            UserId::new(),
            AppId::new(),
            "t".into(),
            1,
            [7u8; 32],
        )
    }

    #[test]
    fn labeled_frames_below_threshold_do_not_start_speaking() {
        let s = session();
        // 0.01 threshold = 40 dBov: 41 is quieter, 39 louder.
        assert!(!s.record_audio_level(Some(41), 0.01));
        assert!(!s.is_speaking.load(Ordering::Relaxed));
        assert_eq!(s.current_audio_level(1000), 41);
        assert!(!s.record_audio_level(Some(AUDIO_LEVEL_SILENCE), 0.0));
        assert!(s.record_audio_level(Some(39), 0.01));
        assert!(!s.record_audio_level(Some(39), 0.01));
        assert!(s.is_speaking.load(Ordering::Relaxed));
        // Unlabeled frames keep the legacy behaviour: any packet is voice.
        let u = session();
        assert!(u.record_audio_level(None, 0.01));
        assert_eq!(u.current_audio_level(1000), AUDIO_LEVEL_SILENCE);
    }

    #[test]
    fn energy_reports_only_on_meaningful_change() {
        let s = session();
        assert_eq!(s.take_energy_report(1000, 3), None);
        s.record_audio_level(Some(30), 0.0);
        assert_eq!(s.take_energy_report(1000, 3), Some(30));
        s.record_audio_level(Some(32), 0.0);
        assert_eq!(s.take_energy_report(1000, 3), None);
        s.record_audio_level(Some(33), 0.0);
        assert_eq!(s.take_energy_report(1000, 3), Some(33));
        s.record_audio_level(Some(AUDIO_LEVEL_SILENCE), 0.0);
        assert_eq!(s.take_energy_report(1000, 3), Some(AUDIO_LEVEL_SILENCE));
        assert_eq!(s.take_energy_report(1000, 3), None);
        // A stale level decays to silence and is reported once.
        s.record_audio_level(Some(20), 0.0);
        assert_eq!(s.take_energy_report(1000, 3), Some(20));
        s.audio_level_at_ms.fetch_sub(5000, Ordering::Relaxed);
        assert_eq!(s.take_energy_report(1000, 3), Some(AUDIO_LEVEL_SILENCE));
        // A reset re-reports an unchanged, still-fresh level exactly once.
        s.record_audio_level(Some(20), 0.0);
        assert_eq!(s.take_energy_report(1000, 3), Some(20));
        s.reset_energy_report();
        assert_eq!(s.take_energy_report(1000, 3), Some(20));
        assert_eq!(s.take_energy_report(1000, 3), None);
        s.record_audio_level(Some(AUDIO_LEVEL_SILENCE), 0.0);
        assert_eq!(s.take_energy_report(1000, 3), Some(AUDIO_LEVEL_SILENCE));
        s.reset_energy_report();
        assert_eq!(s.take_energy_report(1000, 3), None);
    }

    #[test]
    fn prefs_scopes_and_precedence() {
        let mut p = ReceiverPrefs::default();
        let (alice, bob) = (UserId::new(), UserId::new());
        let (team, party) = (ChannelId::new(), ChannelId::new());
        assert_eq!(p.gain_for(&alice, &team), Some(1.0));

        p.set_muted(alice, Some(team), true);
        assert_eq!(p.gain_for(&alice, &team), None);
        assert_eq!(p.gain_for(&alice, &party), Some(1.0));
        assert_eq!(p.gain_for(&bob, &team), Some(1.0));

        p.set_muted(alice, None, true);
        assert_eq!(p.gain_for(&alice, &party), None);
        assert_eq!(p.local_mutes().len(), 2);
        // Unmuting everywhere also clears channel-scoped mutes.
        p.set_muted(alice, None, false);
        assert_eq!(p.gain_for(&alice, &team), Some(1.0));
        assert!(p.local_mutes().is_empty());

        // Channel unmute does not touch an all-channel mute.
        p.set_muted(alice, None, true);
        p.set_muted(alice, Some(team), false);
        assert_eq!(p.gain_for(&alice, &team), None);
        p.set_muted(alice, None, false);

        p.set_gain(alice, 0.25);
        assert_eq!(p.gain_for(&alice, &team), Some(0.25));
        p.set_gain(alice, 5.0);
        assert_eq!(p.gain_for(&alice, &team), Some(MAX_PARTICIPANT_GAIN));
        p.set_gain(alice, -1.0);
        assert_eq!(p.gain_for(&alice, &team), Some(0.0));
        p.set_gain(alice, f32::NAN);
        assert_eq!(p.gain_for(&alice, &team), Some(1.0));
        assert!(p.gains().is_empty(), "unity gain is not stored");

        // Blocks win over gain, in both directions, and survive load_blocks replacement.
        p.set_gain(alice, 0.5);
        p.set_blocked(alice, true);
        assert_eq!(p.gain_for(&alice, &team), None);
        assert!(p.is_blocked(&alice));
        p.set_blocked(alice, false);
        assert_eq!(p.gain_for(&alice, &team), Some(0.5));
        p.set_blocked_by(alice, true);
        assert_eq!(p.gain_for(&alice, &team), None);
        assert!(
            !p.is_blocked(&alice),
            "being blocked is not the same as blocking"
        );
        p.load_blocks([bob], []);
        assert_eq!(p.gain_for(&alice, &team), Some(0.5));
        assert_eq!(p.gain_for(&bob, &team), None);
        assert_eq!(p.blocked_users(), vec![bob]);
    }

    #[test]
    fn focus_attenuates_other_channels_and_stacks_with_sender_gain() {
        let mut p = ReceiverPrefs::with_unfocused_gain(0.25);
        let alice = UserId::new();
        let (team, party) = (ChannelId::new(), ChannelId::new());
        assert_eq!(p.gain_for(&alice, &team), Some(1.0));

        p.set_focus(Some(team));
        assert_eq!(p.gain_for(&alice, &team), Some(1.0));
        assert_eq!(p.gain_for(&alice, &party), Some(0.25));
        p.set_gain(alice, 2.0);
        assert_eq!(p.gain_for(&alice, &party), Some(0.5));
        // Mutes and blocks still win over focus.
        p.set_muted(alice, Some(team), true);
        assert_eq!(p.gain_for(&alice, &team), None);

        p.set_focus(None);
        assert_eq!(p.gain_for(&alice, &party), Some(2.0));

        // Out-of-range or NaN gains fall back to safe values.
        assert_eq!(
            ReceiverPrefs::with_unfocused_gain(f32::NAN).focus_gain(&team),
            1.0
        );
        let mut clamped = ReceiverPrefs::with_unfocused_gain(7.0);
        clamped.set_focus(Some(team));
        assert_eq!(clamped.focus_gain(&party), 1.0);
        let mut zero = ReceiverPrefs::with_unfocused_gain(-3.0);
        zero.set_focus(Some(team));
        assert_eq!(zero.focus_gain(&party), 0.0);
    }

    #[test]
    fn transmission_and_focus_require_membership_and_reset_on_leave() {
        let s = session();
        let (team, party) = (ChannelId::new(), ChannelId::new());
        assert_eq!(s.transmission(), TransmissionMode::All);
        assert!(s.transmits_to(&team));

        assert!(s
            .set_transmission(TransmissionMode::Single { channel_id: team })
            .is_err());
        assert!(s.set_focus(Some(team)).is_err());

        s.join_channel(team);
        s.join_channel(party);
        s.set_transmission(TransmissionMode::Single { channel_id: team })
            .unwrap();
        assert!(s.transmits_to(&team) && !s.transmits_to(&party));
        s.set_focus(Some(team)).unwrap();
        assert_eq!(s.focus(), Some(team));

        // Leaving an unrelated channel changes nothing.
        s.leave_channel(&party);
        assert_eq!(s.forget_channel(&party), (false, false));
        assert_eq!(
            s.transmission(),
            TransmissionMode::Single { channel_id: team }
        );

        s.leave_channel(&team);
        assert_eq!(s.forget_channel(&team), (true, true));
        assert_eq!(s.transmission(), TransmissionMode::None);
        assert!(s.focus().is_none());
        assert!(!s.transmits_to(&party));

        s.set_transmission(TransmissionMode::All).unwrap();
        assert!(s.transmits_to(&party));
    }
}
