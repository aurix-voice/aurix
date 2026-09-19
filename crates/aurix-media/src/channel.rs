use crate::session::MediaSession;
use aurix_common::error::AurixError;
use aurix_common::types::*;
use dashmap::DashMap;
use parking_lot::{RwLock, RwLockReadGuard};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// A sender-reported level older than this no longer describes the frame being routed.
const AMBIENT_LEVEL_STALE_MS: i64 = 500;

/// How one receiver should hear a sender's frame: gain after channel routing, positional
/// attenuation and the receiver's own preferences, plus where the sender is relative to the
/// receiver (positional channels with `directional` enabled only).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mix {
    pub volume: f32,
    pub direction: Option<Direction>,
}

impl Mix {
    pub const UNITY: Mix = Mix {
        volume: 1.0,
        direction: None,
    };

    pub fn volume(volume: f32) -> Mix {
        Mix {
            volume,
            direction: None,
        }
    }
}

#[derive(Clone)]
struct Pose {
    position: Position3D,
    orientation: Orientation3D,
}

/// A member of this channel hosted on another node (learned from the event bus / database),
/// so rosters, presence and relayed audio can treat them like a local participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteParticipant {
    pub session_id: SessionId,
    pub display_name: String,
    pub ssrc: u32,
    pub role: ChannelRole,
    pub is_muted: bool,
}

/// Presence of one channel member as seen by another (`ChannelJoinAck.participants` entry
/// or a `ParticipantJoined`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    pub user_id: UserId,
    pub display_name: String,
    pub ssrc: u32,
    pub role: ChannelRole,
    pub is_muted: bool,
    pub is_speaking: bool,
    pub local: bool,
}

/// A pair of members came into / went out of each other's roster radius. `observer` is a
/// participant hosted on this node (the one to notify); `subject` may be local or remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterChange {
    pub observer: UserId,
    pub subject: UserId,
    pub visible: bool,
}

fn pair(a: UserId, b: UserId) -> (UserId, UserId) {
    if a.0 <= b.0 {
        (a, b)
    } else {
        (b, a)
    }
}

pub struct MediaChannel {
    pub channel_id: ChannelId,
    pub app_id: AppId,
    pub channel_type: ChannelType,
    /// Current configuration; operator edits are applied live via [`MediaChannel::update_config`].
    config: RwLock<ChannelConfig>,
    participants: DashMap<UserId, Arc<MediaSession>>,
    participant_roles: DashMap<UserId, ChannelRole>,
    ssrc_map: DashMap<u32, UserId>,
    participant_count: AtomicU32,
    /// Members hosted on other nodes of the cascade.
    remote: DashMap<UserId, RemoteParticipant>,
    /// Poses of local and remote members (remote ones arrive over the event bus).
    positions: DashMap<UserId, Pose>,
    /// Pairs currently within each other's `roster_radius` (unordered, smaller id first).
    /// Only maintained when the channel has a roster radius.
    visible: RwLock<HashSet<(UserId, UserId)>>,
}

impl MediaChannel {
    pub fn new(channel_id: ChannelId, app_id: AppId, config: ChannelConfig) -> Self {
        Self {
            channel_id,
            app_id,
            channel_type: config.channel_type,
            config: RwLock::new(config),
            participants: DashMap::new(),
            participant_roles: DashMap::new(),
            ssrc_map: DashMap::new(),
            participant_count: AtomicU32::new(0),
            remote: DashMap::new(),
            positions: DashMap::new(),
            visible: RwLock::new(HashSet::new()),
        }
    }

    pub fn config(&self) -> RwLockReadGuard<'_, ChannelConfig> {
        self.config.read()
    }

    /// Replaces the configuration for participants already in the channel. The channel type
    /// is fixed for the channel's lifetime; a different type in `config` is ignored.
    pub fn update_config(&self, mut config: ChannelConfig) {
        config.channel_type = self.channel_type;
        let had_radius = self.roster_radius().is_some();
        *self.config.write() = config;
        if had_radius && self.roster_radius().is_none() {
            self.visible.write().clear();
        }
    }

    pub fn audio_policy(&self) -> AudioPolicy {
        self.config.read().audio_policy()
    }

    pub fn add_participant(
        &self,
        session: Arc<MediaSession>,
        role: ChannelRole,
    ) -> Result<(), AurixError> {
        if session.app_id != self.app_id {
            return Err(AurixError::AuthorizationDenied(
                "Session belongs to a different application".into(),
            ));
        }
        if self.participants.contains_key(&session.user_id) {
            // Re-join with the same user: refresh session/role without changing the count.
            self.remove_participant(&session.user_id);
        }
        // Reserve a slot atomically so concurrent joins cannot exceed max_participants.
        let max = self.config.read().max_participants;
        let reserved =
            self.participant_count
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                    if c >= max {
                        None
                    } else {
                        Some(c + 1)
                    }
                });
        if reserved.is_err() {
            return Err(AurixError::ChannelFull(format!(
                "Channel {} full ({}/{})",
                self.channel_id, max, max
            )));
        }
        self.ssrc_map.insert(session.ssrc, session.user_id);
        self.participant_roles.insert(session.user_id, role);
        self.remote.remove(&session.user_id);
        self.participants.insert(session.user_id, session);
        Ok(())
    }

    pub fn remove_participant(&self, user_id: &UserId) -> Option<Arc<MediaSession>> {
        if let Some((_, session)) = self.participants.remove(user_id) {
            self.ssrc_map.remove(&session.ssrc);
            self.participant_roles.remove(user_id);
            self.positions.remove(user_id);
            self.forget_visibility(user_id);
            for other in self.participants.iter() {
                other
                    .value()
                    .ambient
                    .lock()
                    .forget_sender(&self.channel_id, user_id);
            }
            self.participant_count.fetch_sub(1, Ordering::Relaxed);
            Some(session)
        } else {
            None
        }
    }

    /// Registers (or refreshes) a member hosted on another node. If their pose arrived
    /// first, roster visibility is evaluated now (transitions for local observers returned).
    pub fn add_remote(&self, user_id: UserId, participant: RemoteParticipant) -> Vec<RosterChange> {
        if self.participants.contains_key(&user_id) {
            return Vec::new();
        }
        self.remote.insert(user_id, participant);
        if self.positions.contains_key(&user_id) {
            self.refresh_visibility(&user_id)
        } else {
            Vec::new()
        }
    }

    /// Forgets a remote member; returns the local observers that had them in sight (they
    /// need a `ParticipantLeft`), or `None` when the channel has no roster radius (everyone
    /// saw them).
    pub fn remove_remote(&self, user_id: &UserId) -> Option<Vec<UserId>> {
        let observers = self.observers_of(user_id);
        self.remote.remove(user_id);
        self.positions.remove(user_id);
        self.forget_visibility(user_id);
        for other in self.participants.iter() {
            other
                .value()
                .ambient
                .lock()
                .forget_sender(&self.channel_id, user_id);
        }
        observers
    }

    pub fn remote_count(&self) -> usize {
        self.remote.len()
    }

    pub fn is_remote(&self, user_id: &UserId) -> bool {
        self.remote.contains_key(user_id)
    }

    /// Poses of the members hosted on this node (to seed a node that just learned about
    /// this channel).
    pub fn local_poses(&self) -> Vec<aurix_common::protocol::UserPosition> {
        self.participants
            .iter()
            .filter_map(|e| {
                let pose = self.positions.get(e.key())?;
                Some(aurix_common::protocol::UserPosition {
                    user_id: *e.key(),
                    position: pose.value().position.clone(),
                    orientation: pose.value().orientation.clone(),
                })
            })
            .collect()
    }

    /// Local participants that currently see `user_id` (`None`: no roster radius, all do).
    pub fn observers_of(&self, user_id: &UserId) -> Option<Vec<UserId>> {
        self.roster_radius()?;
        let visible = self.visible.read();
        Some(
            self.participants
                .iter()
                .filter(|e| e.key() != user_id && visible.contains(&pair(*e.key(), *user_id)))
                .map(|e| *e.key())
                .collect(),
        )
    }

    fn forget_visibility(&self, user_id: &UserId) {
        let mut visible = self.visible.write();
        if !visible.is_empty() {
            visible.retain(|(a, b)| a != user_id && b != user_id);
        }
    }

    pub fn roster_radius(&self) -> Option<f32> {
        self.config
            .read()
            .positional_config
            .as_ref()
            .and_then(|p| p.roster_radius)
    }

    pub fn text_radius(&self) -> Option<f32> {
        self.config
            .read()
            .positional_config
            .as_ref()
            .and_then(|p| p.text_radius)
    }

    /// Whether `observer` currently sees `subject` in this channel: always without a roster
    /// radius, otherwise only while the pair is within it (both poses known).
    pub fn sees(&self, observer: &UserId, subject: &UserId) -> bool {
        if observer == subject || self.roster_radius().is_none() {
            return true;
        }
        self.visible.read().contains(&pair(*observer, *subject))
    }

    /// Whether channel text from `sender` reaches `receiver` (`text_radius`; both poses must
    /// be known, as for audio). The sender always gets their own echo.
    pub fn text_reaches(&self, sender: &UserId, receiver: &UserId) -> bool {
        if sender == receiver {
            return true;
        }
        let Some(radius) = self.text_radius() else {
            return true;
        };
        let (Some(a), Some(b)) = (self.positions.get(sender), self.positions.get(receiver)) else {
            return false;
        };
        a.value().position.distance_to(&b.value().position) <= radius
    }

    /// Everything `observer` should currently see in the roster: local and remote members
    /// except themselves, filtered by the roster radius.
    pub fn roster_for(&self, observer: &UserId) -> Vec<RosterEntry> {
        let mut out = Vec::new();
        for e in self.participants.iter() {
            if e.key() == observer || !self.sees(observer, e.key()) {
                continue;
            }
            let s = e.value();
            out.push(RosterEntry {
                user_id: s.user_id,
                display_name: s.display_name.clone(),
                ssrc: s.ssrc,
                role: self.get_role(&s.user_id),
                is_muted: s.is_muted.load(Ordering::Relaxed)
                    || s.is_server_muted.load(Ordering::Relaxed),
                is_speaking: s.is_speaking.load(Ordering::Relaxed),
                local: true,
            });
        }
        for e in self.remote.iter() {
            if e.key() == observer || !self.sees(observer, e.key()) {
                continue;
            }
            let r = e.value();
            out.push(RosterEntry {
                user_id: *e.key(),
                display_name: r.display_name.clone(),
                ssrc: r.ssrc,
                role: r.role,
                is_muted: r.is_muted,
                is_speaking: false,
                local: false,
            });
        }
        out
    }

    /// Presence entry for `user_id` (local or remote), for a `ParticipantJoined` sent when
    /// they come into range.
    pub fn roster_entry(&self, user_id: &UserId) -> Option<RosterEntry> {
        if let Some(s) = self.participants.get(user_id) {
            let s = s.value();
            return Some(RosterEntry {
                user_id: s.user_id,
                display_name: s.display_name.clone(),
                ssrc: s.ssrc,
                role: self.get_role(&s.user_id),
                is_muted: s.is_muted.load(Ordering::Relaxed)
                    || s.is_server_muted.load(Ordering::Relaxed),
                is_speaking: s.is_speaking.load(Ordering::Relaxed),
                local: true,
            });
        }
        self.remote.get(user_id).map(|r| RosterEntry {
            user_id: *user_id,
            display_name: r.display_name.clone(),
            ssrc: r.ssrc,
            role: r.role,
            is_muted: r.is_muted,
            is_speaking: false,
            local: false,
        })
    }

    /// Marks a remote member's mute state (kept for rosters built later).
    pub fn set_remote_muted(&self, user_id: &UserId, muted: bool) {
        if let Some(mut r) = self.remote.get_mut(user_id) {
            r.is_muted = muted;
        }
    }

    pub fn get_participant(&self, user_id: &UserId) -> Option<Arc<MediaSession>> {
        self.participants.get(user_id).map(|s| s.value().clone())
    }

    pub fn get_participant_by_ssrc(&self, ssrc: u32) -> Option<Arc<MediaSession>> {
        self.ssrc_map.get(&ssrc).and_then(|uid| {
            self.participants
                .get(uid.value())
                .map(|s| s.value().clone())
        })
    }

    pub fn get_all_participants(&self) -> Vec<Arc<MediaSession>> {
        self.participants
            .iter()
            .map(|e| e.value().clone())
            .collect()
    }

    pub fn get_other_participants(&self, exclude: &UserId) -> Vec<Arc<MediaSession>> {
        self.participants
            .iter()
            .filter(|e| e.key() != exclude)
            .map(|e| e.value().clone())
            .collect()
    }

    pub fn participant_count(&self) -> u32 {
        self.participant_count.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.participant_count() == 0
    }

    pub fn has_participant(&self, user_id: &UserId) -> bool {
        self.participants.contains_key(user_id)
    }

    /// True if `user_id` may transmit in this channel (role/command-speaker rules).
    pub fn can_transmit(&self, user_id: &UserId) -> bool {
        if !self.participants.contains_key(user_id) {
            return false;
        }
        match self.channel_type {
            ChannelType::Command => {
                self.get_role(user_id).can_speak()
                    || self
                        .config
                        .read()
                        .command_speakers
                        .as_ref()
                        .is_some_and(|s| s.contains(user_id))
            }
            _ => true,
        }
    }

    /// Receivers for audio relayed from another node, filtered by their own preferences towards
    /// the remote `sender`. Positional channels attenuate/pan by the sender's pose (learned
    /// from the event bus); other channel types deliver to every local participant.
    /// `level` is the sender-reported loudness (`-dBov`) carried through the cascade, used
    /// to rank the remote speaker for ambient receivers.
    pub fn get_receivers_for_relayed_audio(
        &self,
        sender: &UserId,
        level: Option<u8>,
    ) -> Vec<(Arc<MediaSession>, Mix)> {
        let base = match self.channel_type {
            ChannelType::Positional => self.positional_receivers(sender),
            ChannelType::Echo => return Vec::new(),
            _ => self.all_others_full_volume(sender),
        };
        self.apply_receiver_prefs(sender, base, level)
    }

    /// Echo channels are local to the sender's node: nothing to relay through the cascade.
    pub fn relays_to_peers(&self) -> bool {
        self.channel_type != ChannelType::Echo
    }

    /// Receivers of a server announcement: every participant, at their channel-focus gain
    /// only (announcements come from no participant, so mutes/blocks/positions do not apply).
    pub fn get_receivers_for_announcement(&self) -> Vec<(Arc<MediaSession>, Mix)> {
        self.participants
            .iter()
            .map(|e| {
                let session = e.value().clone();
                let volume = session.focus_gain(&self.channel_id);
                (session, Mix::volume(volume))
            })
            .filter(|(_, mix)| mix.volume > 0.001)
            .collect()
    }

    /// Stores a member's pose (local or remote). With a roster radius, re-evaluates which
    /// pairs involving `user_id` are in sight and returns the transitions for local observers.
    pub fn update_position(
        &self,
        user_id: &UserId,
        position: Position3D,
        orientation: Orientation3D,
    ) -> Vec<RosterChange> {
        self.positions.insert(
            *user_id,
            Pose {
                position,
                orientation,
            },
        );
        self.refresh_visibility(user_id)
    }

    fn refresh_visibility(&self, user_id: &UserId) -> Vec<RosterChange> {
        let (Some(enter), Some(exit)) = ({
            let cfg = self.config.read();
            let p = cfg.positional_config.as_ref();
            (
                p.and_then(|p| p.roster_radius),
                p.and_then(|p| p.roster_exit_radius()),
            )
        }) else {
            return Vec::new();
        };
        let mover_local = self.participants.contains_key(user_id);
        if !mover_local && !self.remote.contains_key(user_id) {
            return Vec::new();
        }
        let position = match self.positions.get(user_id) {
            Some(p) => p.value().position.clone(),
            None => return Vec::new(),
        };
        let mut changes = Vec::new();
        let mut visible = self.visible.write();
        for other in self.positions.iter() {
            let other_id = *other.key();
            if other_id == *user_id {
                continue;
            }
            let other_local = self.participants.contains_key(&other_id);
            if !other_local && !self.remote.contains_key(&other_id) {
                continue;
            }
            let key = pair(*user_id, other_id);
            let distance = position.distance_to(&other.value().position);
            let was = visible.contains(&key);
            let now = if was {
                distance <= exit
            } else {
                distance <= enter
            };
            if now == was {
                continue;
            }
            if now {
                visible.insert(key);
            } else {
                visible.remove(&key);
            }
            if other_local {
                changes.push(RosterChange {
                    observer: other_id,
                    subject: *user_id,
                    visible: now,
                });
            }
            if mover_local {
                changes.push(RosterChange {
                    observer: *user_id,
                    subject: other_id,
                    visible: now,
                });
            }
        }
        changes
    }

    pub fn get_position(&self, user_id: &UserId) -> Option<Position3D> {
        self.positions
            .get(user_id)
            .map(|p| p.value().position.clone())
    }

    pub fn get_role(&self, user_id: &UserId) -> ChannelRole {
        self.participant_roles
            .get(user_id)
            .map(|r| *r.value())
            .unwrap_or(ChannelRole::Listener)
    }

    /// Returns `(session, mix)` for each receiver of audio from `sender_ssrc`: channel-type
    /// routing (command/whisper/positional/team) combined with every receiver's own
    /// preferences (local mute, per-sender gain, cross-mute).
    pub fn get_receivers_for_audio(&self, sender_ssrc: u32) -> Vec<(Arc<MediaSession>, Mix)> {
        let sender_uid = match self.ssrc_map.get(&sender_ssrc) {
            Some(uid) => *uid.value(),
            None => return Vec::new(),
        };
        let base = self.routed_receivers(&sender_uid);
        self.apply_receiver_prefs(&sender_uid, base, None)
    }

    /// `reported_level` is the frame's `-dBov` level when the sender is not a local
    /// participant (relayed audio); local senders are read from their session.
    fn apply_receiver_prefs(
        &self,
        sender_uid: &UserId,
        receivers: Vec<(Arc<MediaSession>, Mix)>,
        reported_level: Option<u8>,
    ) -> Vec<(Arc<MediaSession>, Mix)> {
        let ambient = self.config.read().ambient;
        let now = ambient.map(|_| Instant::now());
        // Sender-reported level of the current frame (unlabeled frames rank at nominal
        // loudness).
        let level = ambient
            .and_then(|_| {
                reported_level.or_else(|| {
                    self.participants
                        .get(sender_uid)
                        .map(|s| s.current_audio_level(AMBIENT_LEVEL_STALE_MS))
                })
            })
            .filter(|l| *l < aurix_common::protocol::AUDIO_LEVEL_SILENCE)
            .map(aurix_common::protocol::decode_audio_level)
            .unwrap_or(1.0);
        receivers
            .into_iter()
            .filter_map(|(receiver, mix)| {
                let gain = receiver.gain_for(sender_uid, &self.channel_id)?;
                let mut v = mix.volume * gain;
                if let (Some(cfg), Some(now)) = (ambient.as_ref(), now) {
                    if v > 0.001 {
                        v *= receiver.ambient.lock().gate(
                            &self.channel_id,
                            sender_uid,
                            v * level,
                            cfg,
                            now,
                        );
                    }
                }
                (v > 0.001).then_some((
                    receiver,
                    Mix {
                        volume: v,
                        direction: mix.direction,
                    },
                ))
            })
            .collect()
    }

    fn routed_receivers(&self, sender_uid: &UserId) -> Vec<(Arc<MediaSession>, Mix)> {
        let sender_uid = *sender_uid;
        // ── Command channel: only speakers/mods/admins may transmit ──
        if self.channel_type == ChannelType::Command {
            let sender_role = self.get_role(&sender_uid);
            let allowed = sender_role.can_speak()
                || self
                    .config
                    .read()
                    .command_speakers
                    .as_ref()
                    .is_some_and(|speakers| speakers.contains(&sender_uid));
            if !allowed {
                return Vec::new(); // Listeners cannot transmit in Command channels
            }
            // All other participants hear at full volume
            return self.all_others_full_volume(&sender_uid);
        }

        // ── Whisper channel: only send to the designated target ──
        if self.channel_type == ChannelType::Whisper {
            let target = self.config.read().whisper_target;
            if let Some(ref target) = target {
                return self
                    .participants
                    .get(target)
                    .filter(|_| *target != sender_uid)
                    .map(|s| vec![(s.value().clone(), Mix::UNITY)])
                    .unwrap_or_default();
            }
            // Fallback: first non-sender participant (point-to-point)
            return self
                .participants
                .iter()
                .filter(|e| *e.key() != sender_uid)
                .take(1)
                .map(|e| (e.value().clone(), Mix::UNITY))
                .collect();
        }

        if self.channel_type == ChannelType::Positional {
            return self.positional_receivers(&sender_uid);
        }

        // ── Echo channel: the sender is the only listener of their own audio ──
        if self.channel_type == ChannelType::Echo {
            return self
                .participants
                .get(&sender_uid)
                .map(|s| vec![(s.value().clone(), Mix::UNITY)])
                .unwrap_or_default();
        }

        // ── Team channel: everyone hears everyone at full volume ──
        self.all_others_full_volume(&sender_uid)
    }

    /// Positional channel: distance attenuation from the sender to every participant with a
    /// known position, plus the sender's direction in each listener's frame when the channel
    /// is directional.
    fn positional_receivers(&self, sender_uid: &UserId) -> Vec<(Arc<MediaSession>, Mix)> {
        let sender_pos = match self.positions.get(sender_uid) {
            Some(p) => p.value().position.clone(),
            None => return Vec::new(), // No position known for sender
        };
        let pos_cfg = match self.config.read().positional_config.clone() {
            Some(c) => c,
            None => return self.all_others_full_volume(sender_uid),
        };

        let mut receivers = Vec::new();
        for entry in self.participants.iter() {
            if entry.key() == sender_uid {
                continue;
            }
            let listener = match self.positions.get(entry.key()) {
                Some(p) => p.value().clone(),
                None => continue,
            };
            let distance = sender_pos.distance_to(&listener.position);
            if distance > pos_cfg.max_radius {
                continue;
            }

            let volume = if distance <= pos_cfg.near_distance {
                1.0_f32
            } else if distance >= pos_cfg.far_distance {
                0.0_f32
            } else {
                match pos_cfg.rolloff {
                    RolloffCurve::Linear => {
                        1.0 - ((distance - pos_cfg.near_distance)
                            / (pos_cfg.far_distance - pos_cfg.near_distance))
                    }
                    RolloffCurve::Logarithmic => (pos_cfg.near_distance / distance).clamp(0.0, 1.0),
                    RolloffCurve::CustomSpline => {
                        let t = ((distance - pos_cfg.near_distance)
                            / (pos_cfg.far_distance - pos_cfg.near_distance))
                            .clamp(0.0, 1.0);
                        1.0 - (t * t * t)
                    }
                }
            };
            if volume <= 0.001 {
                continue;
            }
            let direction = pos_cfg.directional.then(|| {
                Direction::from_listener(
                    &listener.position,
                    &listener.orientation,
                    &sender_pos,
                    pos_cfg.coordinate_system,
                )
                // Co-located or degenerate orientation: centred, but still marked directional
                // so the client keeps one consistent rendering path per channel.
                .unwrap_or(Direction::AHEAD)
            });
            receivers.push((entry.value().clone(), Mix { volume, direction }));
        }
        receivers
    }

    fn all_others_full_volume(&self, exclude: &UserId) -> Vec<(Arc<MediaSession>, Mix)> {
        self.participants
            .iter()
            .filter(|e| e.key() != exclude)
            .map(|e| (e.value().clone(), Mix::UNITY))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(app: AppId, ssrc: u32) -> Arc<MediaSession> {
        MediaSession::new(
            SessionId::new(),
            UserId::new(),
            app,
            format!("u{ssrc}"),
            ssrc,
            [ssrc as u8; 32],
        )
    }

    fn volumes(recv: &[(Arc<MediaSession>, Mix)]) -> Vec<(UserId, f32)> {
        let mut v: Vec<(UserId, f32)> = recv.iter().map(|(s, m)| (s.user_id, m.volume)).collect();
        v.sort_by_key(|(u, _)| u.0);
        v
    }

    #[test]
    fn local_gain_multiplies_positional_attenuation() {
        let app = AppId::new();
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Positional,
                positional_config: Some(PositionalConfig {
                    near_distance: 1.0,
                    far_distance: 11.0,
                    rolloff: RolloffCurve::Linear,
                    max_radius: 100.0,
                    ..PositionalConfig::default()
                }),
                ..ChannelConfig::default()
            },
        );
        let a = session(app, 1);
        let b = session(app, 2);
        let c = session(app, 3);
        for s in [&a, &b, &c] {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        let o = Orientation3D::default;
        ch.update_position(&a.user_id, Position3D::new(0.0, 0.0, 0.0), o());
        ch.update_position(&b.user_id, Position3D::new(6.0, 0.0, 0.0), o()); // linear -> 0.5
        ch.update_position(&c.user_id, Position3D::new(6.0, 0.0, 0.0), o());

        b.prefs.write().set_gain(a.user_id, 0.5);
        c.prefs.write().set_gain(a.user_id, 2.0);
        let got = ch.get_receivers_for_audio(1);
        let got = volumes(&got);
        assert_eq!(got.len(), 2);
        for (uid, vol) in got {
            if uid == b.user_id {
                assert!(
                    (vol - 0.25).abs() < 1e-5,
                    "0.5 positional x 0.5 gain = {vol}"
                );
            } else {
                assert!(
                    (vol - 1.0).abs() < 1e-5,
                    "0.5 positional x 2.0 boost = {vol}"
                );
            }
        }

        // Zero gain removes the receiver entirely (no packet is sent).
        c.prefs.write().set_gain(a.user_id, 0.0);
        let got = ch.get_receivers_for_audio(1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.user_id, b.user_id);
    }

    fn positional(app: AppId, directional: bool) -> MediaChannel {
        MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Positional,
                positional_config: Some(PositionalConfig {
                    near_distance: 10.0,
                    far_distance: 50.0,
                    rolloff: RolloffCurve::Linear,
                    max_radius: 100.0,
                    directional,
                    ..PositionalConfig::default()
                }),
                ..ChannelConfig::default()
            },
        )
    }

    fn facing(x: f32, z: f32) -> Orientation3D {
        Orientation3D {
            forward_x: x,
            forward_y: 0.0,
            forward_z: z,
            up_x: 0.0,
            up_y: 1.0,
            up_z: 0.0,
        }
    }

    #[test]
    fn positional_receivers_get_sender_direction_in_their_own_frame() {
        let app = AppId::new();
        let ch = positional(app, true);
        let speaker = session(app, 1);
        let facing_z = session(app, 2);
        let facing_x = session(app, 3);
        let colocated = session(app, 4);
        for s in [&speaker, &facing_z, &facing_x, &colocated] {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        // Speaker 5 m to the +X side of both listeners standing at the origin.
        ch.update_position(
            &speaker.user_id,
            Position3D::new(5.0, 0.0, 0.0),
            facing(0.0, 1.0),
        );
        ch.update_position(
            &facing_z.user_id,
            Position3D::new(0.0, 0.0, 0.0),
            facing(0.0, 1.0),
        );
        ch.update_position(
            &facing_x.user_id,
            Position3D::new(0.0, 0.0, 0.0),
            facing(1.0, 0.0),
        );
        ch.update_position(
            &colocated.user_id,
            Position3D::new(5.0, 0.0, 0.0),
            facing(0.0, 1.0),
        );

        let got = ch.get_receivers_for_audio(1);
        assert_eq!(got.len(), 3);
        for (receiver, mix) in got {
            assert_eq!(mix.volume, 1.0, "within near distance");
            let d = mix.direction.expect("directional channel");
            if receiver.user_id == facing_z.user_id {
                assert!(
                    (d.azimuth - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
                    "on the right"
                );
            } else if receiver.user_id == facing_x.user_id {
                assert!(d.azimuth.abs() < 1e-5, "straight ahead");
            } else {
                assert_eq!(d, Direction::AHEAD, "co-located listener hears centred");
            }
        }

        // Direction survives the receiver's own gain and rides along with attenuation.
        facing_z.prefs.write().set_gain(speaker.user_id, 0.5);
        ch.update_position(
            &speaker.user_id,
            Position3D::new(30.0, 0.0, 0.0),
            facing(0.0, 1.0),
        );
        let got = ch.get_receivers_for_audio(1);
        let (_, mix) = got
            .iter()
            .find(|(r, _)| r.user_id == facing_z.user_id)
            .unwrap();
        assert!(
            (mix.volume - 0.25).abs() < 1e-5,
            "0.5 linear x 0.5 gain = {}",
            mix.volume
        );
        assert!((mix.direction.unwrap().azimuth - std::f32::consts::FRAC_PI_2).abs() < 1e-5);

        // Relayed audio from a remote speaker with a known pose is spatialised the same way.
        let remote = UserId::new();
        ch.update_position(&remote, Position3D::new(-5.0, 0.0, 0.0), facing(0.0, 1.0));
        let got = ch.get_receivers_for_relayed_audio(&remote, None);
        let (_, mix) = got
            .iter()
            .find(|(r, _)| r.user_id == facing_z.user_id)
            .unwrap();
        assert!((mix.direction.unwrap().azimuth + std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }

    #[test]
    fn non_directional_positional_channel_stays_mono() {
        let app = AppId::new();
        let ch = positional(app, false);
        let a = session(app, 1);
        let b = session(app, 2);
        ch.add_participant(a.clone(), ChannelRole::Speaker).unwrap();
        ch.add_participant(b.clone(), ChannelRole::Speaker).unwrap();
        ch.update_position(&a.user_id, Position3D::new(5.0, 0.0, 0.0), facing(0.0, 1.0));
        ch.update_position(&b.user_id, Position3D::new(0.0, 0.0, 0.0), facing(0.0, 1.0));
        let got = ch.get_receivers_for_audio(1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, Mix::UNITY);

        // Team channels never carry a direction, whatever positions are known.
        let team = MediaChannel::new(ChannelId::new(), app, ChannelConfig::default());
        team.add_participant(a.clone(), ChannelRole::Speaker)
            .unwrap();
        team.add_participant(b.clone(), ChannelRole::Speaker)
            .unwrap();
        team.update_position(&a.user_id, Position3D::new(5.0, 0.0, 0.0), facing(0.0, 1.0));
        team.update_position(&b.user_id, Position3D::new(0.0, 0.0, 0.0), facing(0.0, 1.0));
        assert_eq!(team.get_receivers_for_audio(1)[0].1, Mix::UNITY);
    }

    #[test]
    fn whisper_and_command_routing_still_respect_local_mute() {
        let app = AppId::new();
        let a = session(app, 1);
        let b = session(app, 2);
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Whisper,
                whisper_target: Some(b.user_id),
                ..ChannelConfig::default()
            },
        );
        ch.add_participant(a.clone(), ChannelRole::Speaker).unwrap();
        ch.add_participant(b.clone(), ChannelRole::Speaker).unwrap();
        assert_eq!(ch.get_receivers_for_audio(1).len(), 1);
        b.prefs.write().set_muted(a.user_id, None, true);
        assert!(ch.get_receivers_for_audio(1).is_empty());
    }

    #[test]
    fn relayed_audio_applies_receiver_prefs_towards_remote_sender() {
        let app = AppId::new();
        let ch = MediaChannel::new(ChannelId::new(), app, ChannelConfig::default());
        let (a, b) = (session(app, 10), session(app, 11));
        ch.add_participant(a.clone(), ChannelRole::Speaker).unwrap();
        ch.add_participant(b.clone(), ChannelRole::Speaker).unwrap();
        let remote = UserId::new();
        assert_eq!(ch.get_receivers_for_relayed_audio(&remote, None).len(), 2);

        a.prefs.write().set_blocked(remote, true);
        b.prefs.write().set_gain(remote, 0.5);
        let got = ch.get_receivers_for_relayed_audio(&remote, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.ssrc, 11);
        assert_eq!(got[0].1.volume, 0.5);

        b.prefs.write().set_blocked_by(remote, true);
        assert!(ch.get_receivers_for_relayed_audio(&remote, None).is_empty());
    }

    #[test]
    fn echo_channel_returns_audio_to_its_sender_only() {
        let app = AppId::new();
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Echo,
                ..ChannelConfig::default()
            },
        );
        let (a, b) = (session(app, 1), session(app, 2));
        ch.add_participant(a.clone(), ChannelRole::Speaker).unwrap();
        ch.add_participant(b.clone(), ChannelRole::Speaker).unwrap();

        let got = ch.get_receivers_for_audio(1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.ssrc, 1, "alice hears herself");
        assert_eq!(got[0].1, Mix::UNITY);
        assert_eq!(
            ch.get_receivers_for_audio(2)[0].0.ssrc,
            2,
            "bob hears himself"
        );

        // Receiver-local volume still applies to the loopback; other nodes never get it.
        a.prefs.write().set_gain(a.user_id, 0.5);
        assert_eq!(ch.get_receivers_for_audio(1)[0].1.volume, 0.5);
        assert!(!ch.relays_to_peers());
        assert!(ch
            .get_receivers_for_relayed_audio(&UserId::new(), None)
            .is_empty());
    }

    fn radius_channel(app: AppId, roster: Option<f32>, text: Option<f32>) -> MediaChannel {
        MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Positional,
                positional_config: Some(PositionalConfig {
                    roster_radius: roster,
                    text_radius: text,
                    ..PositionalConfig::default()
                }),
                ..ChannelConfig::default()
            },
        )
    }

    #[test]
    fn roster_radius_reveals_and_hides_with_hysteresis() {
        let app = AppId::new();
        let ch = radius_channel(app, Some(10.0), None);
        let (a, b) = (session(app, 1), session(app, 2));
        ch.add_participant(a.clone(), ChannelRole::Speaker).unwrap();
        ch.add_participant(b.clone(), ChannelRole::Speaker).unwrap();
        let o = Orientation3D::default;

        // Nobody is visible until both poses are known.
        assert!(!ch.sees(&a.user_id, &b.user_id));
        assert!(ch.roster_for(&a.user_id).is_empty());
        assert!(ch
            .update_position(&a.user_id, Position3D::new(0.0, 0.0, 0.0), o())
            .is_empty());

        // Bob appears 5 m away: both local observers get an "enter" transition.
        let mut changes = ch.update_position(&b.user_id, Position3D::new(5.0, 0.0, 0.0), o());
        changes.sort_by_key(|c| c.observer.0);
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|c| c.visible));
        assert!(changes
            .iter()
            .any(|c| c.observer == a.user_id && c.subject == b.user_id));
        assert!(changes
            .iter()
            .any(|c| c.observer == b.user_id && c.subject == a.user_id));
        assert!(ch.sees(&a.user_id, &b.user_id) && ch.sees(&b.user_id, &a.user_id));
        let roster = ch.roster_for(&a.user_id);
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].user_id, b.user_id);
        assert!(roster[0].local);

        // 10.5 m: beyond the entry radius but inside the 1.1x exit radius -> still visible.
        assert!(ch
            .update_position(&b.user_id, Position3D::new(10.5, 0.0, 0.0), o())
            .is_empty());
        assert!(ch.sees(&a.user_id, &b.user_id));
        // 12 m: hidden for both.
        let changes = ch.update_position(&b.user_id, Position3D::new(12.0, 0.0, 0.0), o());
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|c| !c.visible));
        assert!(!ch.sees(&a.user_id, &b.user_id));
        // 10.5 m again: not back until inside the entry radius.
        assert!(ch
            .update_position(&b.user_id, Position3D::new(10.5, 0.0, 0.0), o())
            .is_empty());
        assert!(!ch.sees(&a.user_id, &b.user_id));
        assert_eq!(
            ch.update_position(&b.user_id, Position3D::new(9.0, 0.0, 0.0), o())
                .len(),
            2
        );

        // Leaving clears the pairs and reports who saw the leaver.
        assert_eq!(ch.observers_of(&b.user_id), Some(vec![a.user_id]));
        ch.remove_participant(&b.user_id);
        assert!(!ch.sees(&a.user_id, &b.user_id));
        assert_eq!(ch.observers_of(&b.user_id), Some(Vec::new()));
    }

    #[test]
    fn remote_members_join_the_scoped_roster_once_positioned() {
        let app = AppId::new();
        let ch = radius_channel(app, Some(10.0), None);
        let a = session(app, 1);
        ch.add_participant(a.clone(), ChannelRole::Speaker).unwrap();
        let o = Orientation3D::default;
        ch.update_position(&a.user_id, Position3D::new(0.0, 0.0, 0.0), o());

        // A pose for an unknown user is stored but produces no transition...
        let remote = UserId::new();
        assert!(ch
            .update_position(&remote, Position3D::new(3.0, 0.0, 0.0), o())
            .is_empty());
        assert!(!ch.sees(&a.user_id, &remote));
        // ...until the member is registered: only the local observer gets a transition.
        let changes = ch.add_remote(
            remote,
            RemoteParticipant {
                session_id: SessionId::new(),
                display_name: "Remote".into(),
                ssrc: 77,
                role: ChannelRole::Moderator,
                is_muted: true,
            },
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(
            (changes[0].observer, changes[0].subject),
            (a.user_id, remote)
        );
        let roster = ch.roster_for(&a.user_id);
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].ssrc, 77);
        assert_eq!(roster[0].role, ChannelRole::Moderator);
        assert!(roster[0].is_muted && !roster[0].local);
        assert_eq!(ch.remote_count(), 1);
        assert_eq!(
            ch.local_poses()
                .iter()
                .map(|p| p.user_id)
                .collect::<Vec<_>>(),
            vec![a.user_id]
        );

        assert_eq!(ch.remove_remote(&remote), Some(vec![a.user_id]));
        assert!(ch.roster_for(&a.user_id).is_empty());
        assert!(!ch.is_remote(&remote));
    }

    #[test]
    fn text_radius_scopes_text_but_not_roster() {
        let app = AppId::new();
        let ch = radius_channel(app, None, Some(4.0));
        let (a, b) = (session(app, 1), session(app, 2));
        ch.add_participant(a.clone(), ChannelRole::Speaker).unwrap();
        ch.add_participant(b.clone(), ChannelRole::Speaker).unwrap();
        let o = Orientation3D::default;

        // Without a roster radius everyone is listed; text needs both poses.
        assert_eq!(ch.roster_for(&a.user_id).len(), 1);
        assert!(ch.sees(&a.user_id, &b.user_id));
        assert!(!ch.text_reaches(&a.user_id, &b.user_id));
        assert!(ch.text_reaches(&a.user_id, &a.user_id), "own echo");

        assert!(ch
            .update_position(&a.user_id, Position3D::new(0.0, 0.0, 0.0), o())
            .is_empty());
        ch.update_position(&b.user_id, Position3D::new(3.0, 0.0, 0.0), o());
        assert!(ch.text_reaches(&a.user_id, &b.user_id));
        ch.update_position(&b.user_id, Position3D::new(4.5, 0.0, 0.0), o());
        assert!(!ch.text_reaches(&a.user_id, &b.user_id));

        // A channel without radii keeps whole-channel text.
        let plain = radius_channel(app, None, None);
        assert!(plain.text_reaches(&a.user_id, &b.user_id));
        assert_eq!(
            radius_channel(app, None, None).observers_of(&a.user_id),
            None
        );
    }

    #[test]
    fn ambient_mode_keeps_the_loudest_voices_and_dims_the_rest() {
        let app = AppId::new();
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Team,
                ambient: Some(AmbientConfig {
                    max_voices: 2,
                    ambient_gain: 0.2,
                }),
                ..ChannelConfig::default()
            },
        );
        let listener = session(app, 9);
        let speakers: Vec<_> = (1..=3).map(|i| session(app, i)).collect();
        ch.add_participant(listener.clone(), ChannelRole::Speaker)
            .unwrap();
        for s in &speakers {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        let vol_to = |got: &[(Arc<MediaSession>, Mix)], who: &Arc<MediaSession>| {
            got.iter()
                .find(|(s, _)| s.user_id == who.user_id)
                .map(|(_, m)| m.volume)
        };

        // Speakers 1 and 2 talk first (unlabeled frames rank at nominal loudness) ...
        assert_eq!(vol_to(&ch.get_receivers_for_audio(1), &listener), Some(1.0));
        assert_eq!(vol_to(&ch.get_receivers_for_audio(2), &listener), Some(1.0));
        // ... a third, equally loud voice becomes ambient for the listener ...
        let third = ch.get_receivers_for_audio(3);
        assert!((vol_to(&third, &listener).unwrap() - 0.2).abs() < 1e-6);
        // ... but speaker 1 (who is not hearing speaker 2 at all yet) still gets it in full:
        // slots are per receiver.
        assert_eq!(vol_to(&third, &speakers[0]), Some(1.0));
        // Holders keep their slots against an equally loud challenger.
        assert_eq!(vol_to(&ch.get_receivers_for_audio(1), &listener), Some(1.0));

        // A holder whose frames report a much quieter level loses the slot to the louder
        // challenger (ranking = delivery gain x sender-reported level).
        speakers[2].record_audio_level(Some(0), 0.0); // 0 dBov = full scale
        speakers[0].record_audio_level(Some(40), 0.0); // -40 dBov
        assert!((vol_to(&ch.get_receivers_for_audio(1), &listener).unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(vol_to(&ch.get_receivers_for_audio(3), &listener), Some(1.0));

        // Receiver-local volume still applies on top and a muted sender is not routed.
        listener.prefs.write().set_gain(speakers[1].user_id, 0.5);
        assert_eq!(vol_to(&ch.get_receivers_for_audio(2), &listener), Some(0.5));
        listener.prefs.write().set_gain(speakers[1].user_id, 0.0);
        assert_eq!(vol_to(&ch.get_receivers_for_audio(2), &listener), None);

        // Leaving frees the slot for the listener immediately.
        ch.remove_participant(&speakers[2].user_id);
        assert_eq!(vol_to(&ch.get_receivers_for_audio(1), &listener), Some(1.0));

        // A speaker on another node competes with the level its origin node relayed: a
        // full-scale remote voice takes a slot from the whispering speaker 1, a -60 dBov
        // remote voice stays ambient.
        let remote_loud = UserId::new();
        let remote_quiet = UserId::new();
        assert_eq!(
            vol_to(
                &ch.get_receivers_for_relayed_audio(&remote_loud, Some(0)),
                &listener
            ),
            Some(1.0)
        );
        let whisper = ch.get_receivers_for_relayed_audio(&remote_quiet, Some(60));
        assert!((vol_to(&whisper, &listener).unwrap() - 0.2).abs() < 1e-6);
    }
}
