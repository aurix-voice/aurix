use aurix_common::error::AurixError;
use aurix_common::types::*;
use crate::session::MediaSession;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct MediaChannel {
    pub channel_id: ChannelId,
    pub app_id: AppId,
    pub channel_type: ChannelType,
    pub config: ChannelConfig,
    participants: DashMap<UserId, Arc<MediaSession>>,
    participant_roles: DashMap<UserId, ChannelRole>,
    ssrc_map: DashMap<u32, UserId>,
    participant_count: AtomicU32,
    positions: DashMap<UserId, Position3D>,
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

    pub fn add_participant(&self, session: Arc<MediaSession>, role: ChannelRole) -> Result<(), AurixError> {
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
        let reserved = self.participant_count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
            if c >= max { None } else { Some(c + 1) }
        });
        if reserved.is_err() {
            return Err(AurixError::ChannelFull(
                format!("Channel {} full ({}/{})", self.channel_id, max, max),
            ));
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
        self.ssrc_map.get(&ssrc)
            .and_then(|uid| self.participants.get(uid.value()).map(|s| s.value().clone()))
    }

    pub fn get_all_participants(&self) -> Vec<Arc<MediaSession>> {
        self.participants.iter().map(|e| e.value().clone()).collect()
    }

    pub fn get_other_participants(&self, exclude: &UserId) -> Vec<Arc<MediaSession>> {
        self.participants.iter()
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
                    || self.config.command_speakers.as_ref().is_some_and(|s| s.contains(user_id))
            }
            _ => true,
        }
    }

    /// Receivers for audio relayed from another node (sender is not a local participant).
    pub fn get_receivers_for_relayed_audio(&self) -> Vec<(Arc<MediaSession>, f32)> {
        self.participants.iter().map(|e| (e.value().clone(), 1.0f32)).collect()
    }

    pub fn update_position(&self, user_id: &UserId, position: Position3D) {
        self.positions.insert(*user_id, position);
    }

    pub fn get_position(&self, user_id: &UserId) -> Option<Position3D> {
        self.positions.get(user_id).map(|p| p.value().clone())
    }

    pub fn get_role(&self, user_id: &UserId) -> ChannelRole {
        self.participant_roles.get(user_id).map(|r| *r.value()).unwrap_or(ChannelRole::Listener)
    }

    /// Returns (session, volume_multiplier) for each receiver of audio from `sender_ssrc`.
    pub fn get_receivers_for_audio(&self, sender_ssrc: u32) -> Vec<(Arc<MediaSession>, f32)> {
        let sender_uid = match self.ssrc_map.get(&sender_ssrc) {
            Some(uid) => *uid.value(),
            None => return Vec::new(),
        };

        // ── Command channel: only speakers/mods/admins may transmit ──
        if self.channel_type == ChannelType::Command {
            let sender_role = self.get_role(&sender_uid);
            let allowed = sender_role.can_speak()
                || self.config.command_speakers.as_ref()
                    .map_or(false, |speakers| speakers.contains(&sender_uid));
            if !allowed {
                return Vec::new(); // Listeners cannot transmit in Command channels
            }
            // All other participants hear at full volume
            return self.participants.iter()
                .filter(|e| *e.key() != sender_uid)
                .map(|e| (e.value().clone(), 1.0f32))
                .collect();
        }

        // ── Whisper channel: only send to the designated target ──
        if self.channel_type == ChannelType::Whisper {
            if let Some(ref target) = self.config.whisper_target {
                return self.participants.get(target)
                    .filter(|_| *target != sender_uid)
                    .map(|s| vec![(s.value().clone(), 1.0f32)])
                    .unwrap_or_default();
            }
            // Fallback: first non-sender participant (point-to-point)
            return self.participants.iter()
                .filter(|e| *e.key() != sender_uid)
                .take(1)
                .map(|e| (e.value().clone(), 1.0f32))
                .collect();
        }

        // ── Positional channel: distance-based attenuation ──
        if self.channel_type == ChannelType::Positional {
            let sender_pos = match self.positions.get(&sender_uid) {
                Some(p) => p.value().clone(),
                None => return Vec::new(), // No position known for sender
            };
            let pos_cfg = match &self.config.positional_config {
                Some(c) => c,
                None => return self.all_others_full_volume(&sender_uid),
            };

            let mut receivers = Vec::new();
            for entry in self.participants.iter() {
                if *entry.key() == sender_uid { continue; }
                let recv_pos = match self.positions.get(entry.key()) {
                    Some(p) => p.value().clone(),
                    None => continue,
                };
                let distance = sender_pos.distance_to(&recv_pos);
                if distance > pos_cfg.max_radius { continue; }

                let volume = if distance <= pos_cfg.near_distance {
                    1.0_f32
                } else if distance >= pos_cfg.far_distance {
                    0.0_f32
                } else {
                    match pos_cfg.rolloff {
                        RolloffCurve::Linear => {
                            1.0 - ((distance - pos_cfg.near_distance) / (pos_cfg.far_distance - pos_cfg.near_distance))
                        }
                        RolloffCurve::Logarithmic => {
                            (pos_cfg.near_distance / distance).min(1.0).max(0.0)
                        }
                        RolloffCurve::CustomSpline => {
                            let t = ((distance - pos_cfg.near_distance) / (pos_cfg.far_distance - pos_cfg.near_distance)).clamp(0.0, 1.0);
                            1.0 - (t * t * t)
                        }
                    }
                };
                if volume > 0.001 {
                    receivers.push((entry.value().clone(), volume));
                }
            }
            return receivers;
        }

        // ── Team channel: everyone hears everyone at full volume ──
        self.all_others_full_volume(&sender_uid)
    }

    fn all_others_full_volume(&self, exclude: &UserId) -> Vec<(Arc<MediaSession>, f32)> {
        self.participants.iter()
            .filter(|e| e.key() != exclude)
            .map(|e| (e.value().clone(), 1.0f32))
            .collect()
    }
}