use crate::session::MediaSession;
use aurix_common::error::AurixError;
use aurix_common::types::*;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

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

pub struct MediaChannel {
    pub channel_id: ChannelId,
    pub app_id: AppId,
    pub channel_type: ChannelType,
    pub config: ChannelConfig,
    participants: DashMap<UserId, Arc<MediaSession>>,
    participant_roles: DashMap<UserId, ChannelRole>,
    ssrc_map: DashMap<u32, UserId>,
    /// Participants of this channel hosted on other nodes (learned from the event bus), so
    /// receiver preferences can be applied to relayed audio too.
    participant_count: AtomicU32,
    positions: DashMap<UserId, Pose>,
}

impl MediaChannel {
    pub fn new(channel_id: ChannelId, app_id: AppId, config: ChannelConfig) -> Self {
        Self {
            channel_id,
            app_id,
            channel_type: config.channel_type,
            config,
            participants: DashMap::new(),
            participant_roles: DashMap::new(),
            ssrc_map: DashMap::new(),
            participant_count: AtomicU32::new(0),
            positions: DashMap::new(),
        }
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
        let max = self.config.max_participants;
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
        self.participants.insert(session.user_id, session);
        Ok(())
    }

    pub fn remove_participant(&self, user_id: &UserId) -> Option<Arc<MediaSession>> {
        if let Some((_, session)) = self.participants.remove(user_id) {
            self.ssrc_map.remove(&session.ssrc);
            self.participant_roles.remove(user_id);
            self.positions.remove(user_id);
            self.participant_count.fetch_sub(1, Ordering::Relaxed);
            Some(session)
        } else {
            None
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
    pub fn get_receivers_for_relayed_audio(
        &self,
        sender: &UserId,
    ) -> Vec<(Arc<MediaSession>, Mix)> {
        let base = if self.channel_type == ChannelType::Positional {
            self.positional_receivers(sender)
        } else {
            self.all_others_full_volume(sender)
        };
        self.apply_receiver_prefs(sender, base)
    }

    pub fn update_position(
        &self,
        user_id: &UserId,
        position: Position3D,
        orientation: Orientation3D,
    ) {
        self.positions.insert(
            *user_id,
            Pose {
                position,
                orientation,
            },
        );
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
        self.apply_receiver_prefs(&sender_uid, base)
    }

    fn apply_receiver_prefs(
        &self,
        sender_uid: &UserId,
        receivers: Vec<(Arc<MediaSession>, Mix)>,
    ) -> Vec<(Arc<MediaSession>, Mix)> {
        receivers
            .into_iter()
            .filter_map(|(receiver, mix)| {
                let gain = receiver.gain_for(sender_uid, &self.channel_id)?;
                let v = mix.volume * gain;
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
            if let Some(ref target) = self.config.whisper_target {
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
        let pos_cfg = match &self.config.positional_config {
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
        let got = ch.get_receivers_for_relayed_audio(&remote);
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
        assert_eq!(ch.get_receivers_for_relayed_audio(&remote).len(), 2);

        a.prefs.write().set_blocked(remote, true);
        b.prefs.write().set_gain(remote, 0.5);
        let got = ch.get_receivers_for_relayed_audio(&remote);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.ssrc, 11);
        assert_eq!(got[0].1.volume, 0.5);

        b.prefs.write().set_blocked_by(remote, true);
        assert!(ch.get_receivers_for_relayed_audio(&remote).is_empty());
    }
}
