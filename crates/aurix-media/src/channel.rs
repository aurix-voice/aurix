use crate::session::MediaSession;
use aurix_common::error::AurixError;
use aurix_common::types::*;
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A sender-reported level older than this no longer describes the frame being routed.
const AMBIENT_LEVEL_STALE_MS: i64 = 500;

/// A waiting member counts as wanting to speak while its last dropped uplink frame is at
/// most this old (`speaker_admission = "demote"` rotation).
const SPEAK_ATTEMPT_FRESH_MS: i64 = 1_000;

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
    pub is_priority: bool,
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
    pub is_priority: bool,
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

/// Outcome of [`MediaChannel::add_participant`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The effective role the member entered with (`Listener` while it waits for a
    /// speaker slot its grant entitles it to).
    pub role: ChannelRole,
    /// Whether the member is waiting for a speaker slot.
    pub waiting: bool,
    /// Speaker demoted to make room for the joiner (`speaker_admission = "demote"`).
    pub demoted: Option<RoleChange>,
}

/// What the control plane knows about a joiner beyond its role.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JoinHints {
    /// The grant makes the joiner a priority speaker (always gets a slot under
    /// `speaker_admission = "demote"`, like moderators and administrators).
    pub priority: bool,
    /// Speakers on other nodes the channel has not learned about yet (the first local
    /// joiner of a channel already live elsewhere); counted against `max_speakers`.
    pub remote_speakers: u32,
}

/// One local member's effective role changed at runtime (speaker-slot admission).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleChange {
    pub user_id: UserId,
    pub session_id: SessionId,
    pub role: ChannelRole,
    /// Local members to tell (`None`: every member). A demotion lists those who saw the
    /// member before it, an admission those who see it now (`roster_radius` scoping).
    pub observers: Option<Vec<UserId>>,
}

/// Effective role of a local member next to the role its grant carries, with the
/// bookkeeping the speaker-slot admission needs.
#[derive(Debug, Clone, Copy)]
struct LocalRole {
    role: ChannelRole,
    granted: ChannelRole,
    /// When the member got its current speaker slot (join or admission), Unix ms.
    slot_since_ms: i64,
    /// Set while the member holds a speaking grant but no slot.
    waiting_since: Option<Instant>,
    /// Last uplink frame dropped because the member was waiting, Unix ms.
    speak_attempt_ms: i64,
}

impl LocalRole {
    fn wants_slot(&self) -> bool {
        self.waiting_since.is_some() && self.granted.can_speak()
    }
}

fn pair(a: UserId, b: UserId) -> (UserId, UserId) {
    if a.0 <= b.0 {
        (a, b)
    } else {
        (b, a)
    }
}

/// Silence a speaker must have shown before it yields its slot to `privileged` (priority
/// speakers, moderators, administrators) or ordinary waiting members.
fn demote_idle_for(privileged: bool, audience: &AudienceConfig) -> i64 {
    if privileged {
        0
    } else {
        audience.demote_idle_ms as i64
    }
}

/// Ducking state of one channel (`ChannelConfig::ducking`): `depth` is how far the
/// non-priority voices are currently pushed towards `DuckingConfig::gain` (`0.0` = not at
/// all, `1.0` = fully), ramped linearly towards its target — `1.0` while a priority speaker
/// was audible within `hold_ms`, `0.0` otherwise — at the configured attack/release speed.
#[derive(Debug, Clone, Copy)]
pub struct DuckEnvelope {
    depth: f32,
    updated: Option<Instant>,
    held_until: Option<Instant>,
}

impl Default for DuckEnvelope {
    fn default() -> Self {
        Self {
            depth: 0.0,
            updated: None,
            held_until: None,
        }
    }
}

impl DuckEnvelope {
    /// A priority speaker's frame was audible at `now`.
    pub fn trigger(&mut self, now: Instant, cfg: &DuckingConfig) {
        self.advance(now, cfg);
        self.held_until = Some(now + Duration::from_millis(u64::from(cfg.hold_ms)));
    }

    /// Gain for a non-priority voice at `now`.
    pub fn gain(&mut self, now: Instant, cfg: &DuckingConfig) -> f32 {
        self.advance(now, cfg);
        1.0 - self.depth * (1.0 - cfg.gain)
    }

    /// Whether priority speech is holding the mix down (before the release completes).
    pub fn is_active(&self, now: Instant) -> bool {
        self.held_until.is_some_and(|t| t > now)
    }

    fn advance(&mut self, now: Instant, cfg: &DuckingConfig) {
        let Some(updated) = self.updated else {
            self.depth = if self.is_active(now) { 1.0 } else { 0.0 };
            self.updated = Some(now);
            return;
        };
        // A hold that ended between two frames: attack up to its end, release from there.
        if let Some(held) = self.held_until.filter(|h| *h > updated && *h <= now) {
            self.ramp(1.0, held.saturating_duration_since(updated), cfg);
            self.ramp(0.0, now.saturating_duration_since(held), cfg);
        } else {
            let target = if self.is_active(now) { 1.0 } else { 0.0 };
            self.ramp(target, now.saturating_duration_since(updated), cfg);
        }
        self.updated = Some(now);
    }

    fn ramp(&mut self, target: f32, elapsed: Duration, cfg: &DuckingConfig) {
        let elapsed_ms = elapsed.as_secs_f32() * 1000.0;
        let ramp_ms = if target > self.depth {
            cfg.attack_ms
        } else {
            cfg.release_ms
        } as f32;
        if ramp_ms <= 0.0 {
            self.depth = target;
            return;
        }
        let step = (elapsed_ms / ramp_ms).min(1.0);
        self.depth += (target - self.depth).clamp(-step, step);
    }
}

pub struct MediaChannel {
    pub channel_id: ChannelId,
    pub app_id: AppId,
    pub channel_type: ChannelType,
    /// Current configuration; operator edits are applied live via [`MediaChannel::update_config`].
    config: RwLock<ChannelConfig>,
    participants: DashMap<UserId, Arc<MediaSession>>,
    participant_roles: DashMap<UserId, LocalRole>,
    /// Local members that are priority speakers (grant or runtime promotion).
    priority: DashMap<UserId, ()>,
    ducking: Mutex<DuckEnvelope>,
    ssrc_map: DashMap<u32, UserId>,
    participant_count: AtomicU32,
    /// Local members whose role may speak (`audience.max_speakers` admission).
    local_speakers: AtomicU32,
    /// Serializes speaker-slot decisions (admission, demotion, promotion) so two joins
    /// cannot both take the last slot or demote the same speaker.
    admission: Mutex<()>,
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
            priority: DashMap::new(),
            ducking: Mutex::new(DuckEnvelope::default()),
            ssrc_map: DashMap::new(),
            participant_count: AtomicU32::new(0),
            local_speakers: AtomicU32::new(0),
            admission: Mutex::new(()),
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
    ) -> Result<Admission, AurixError> {
        self.add_participant_with(session, role, JoinHints::default())
    }

    /// Adds a local member with the role its grant carries. Whether it may actually speak
    /// right away depends on `audience.max_speakers` / `speaker_admission`; the returned
    /// [`Admission`] says what happened.
    pub fn add_participant_with(
        &self,
        session: Arc<MediaSession>,
        role: ChannelRole,
        hints: JoinHints,
    ) -> Result<Admission, AurixError> {
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
        let (max, audience) = {
            let cfg = self.config.read();
            (cfg.max_participants, cfg.audience.unwrap_or_default())
        };
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
        let now_ms = Utc::now().timestamp_millis();
        let mut local = LocalRole {
            role,
            granted: role,
            slot_since_ms: now_ms,
            waiting_since: None,
            speak_attempt_ms: 0,
        };
        let mut demoted = None;
        // Speaker slots are handed out under the admission lock (remote speakers count
        // against the cap as last seen; two nodes admitting the last slot at once is bounded
        // by the cascade's propagation delay).
        if role.can_speak() {
            let _slots = self.admission.lock();
            let max_speakers = audience.max_speakers;
            let remote = if self.remote.is_empty() {
                hints.remote_speakers
            } else {
                self.remote_speakers_except(&session.user_id)
            };
            let held = self.local_speakers.load(Ordering::Acquire) + remote;
            if max_speakers > 0 && held >= max_speakers {
                let idle_victim = match audience.speaker_admission {
                    SpeakerAdmission::Reject => {
                        self.participant_count.fetch_sub(1, Ordering::AcqRel);
                        return Err(AurixError::ChannelFull(format!(
                            "Channel {} has no speaker slot left ({max_speakers} speakers)",
                            self.channel_id
                        )));
                    }
                    SpeakerAdmission::Wait => None,
                    SpeakerAdmission::Demote => self.idle_speaker(
                        now_ms,
                        demote_idle_for(hints.priority || role.can_moderate(), &audience),
                    ),
                };
                match idle_victim {
                    Some(victim) => {
                        demoted = self.demote_locked_announced(&victim);
                    }
                    None => {
                        local.role = ChannelRole::Listener;
                        local.waiting_since = Some(Instant::now());
                    }
                }
            }
            if local.role.can_speak() {
                self.local_speakers.fetch_add(1, Ordering::AcqRel);
            }
        }
        self.ssrc_map.insert(session.ssrc, session.user_id);
        self.participant_roles.insert(session.user_id, local);
        if hints.priority {
            self.priority.insert(session.user_id, ());
        } else {
            self.priority.remove(&session.user_id);
        }
        self.remote.remove(&session.user_id);
        self.participants.insert(session.user_id, session);
        Ok(Admission {
            role: local.role,
            waiting: local.waiting_since.is_some(),
            demoted,
        })
    }

    fn remote_speakers_except(&self, user_id: &UserId) -> u32 {
        self.remote
            .iter()
            .filter(|r| r.key() != user_id && r.role.can_speak())
            .count() as u32
    }

    /// The local speaker to demote, if any: a plain speaker (moderators, administrators and
    /// priority speakers are exempt) silent for at least `idle_ms` since its last audible
    /// frame or since it got the slot — the longest silent first, the quietest among equals,
    /// then by id. Caller holds the admission lock.
    fn idle_speaker(&self, now_ms: i64, idle_ms: i64) -> Option<UserId> {
        self.participant_roles
            .iter()
            .filter(|e| e.value().role == ChannelRole::Speaker)
            .filter(|e| !self.priority.contains_key(e.key()))
            .filter_map(|e| {
                let session = self.participants.get(e.key())?;
                let last = session
                    .last_audio_at_ms
                    .load(Ordering::Relaxed)
                    .max(e.value().slot_since_ms);
                let idle = now_ms - last;
                (idle >= idle_ms).then(|| {
                    (
                        idle,
                        session.current_audio_level(AMBIENT_LEVEL_STALE_MS),
                        *e.key(),
                    )
                })
            })
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(b.2 .0.cmp(&a.2 .0)))
            .map(|(_, _, uid)| uid)
    }

    /// Takes the speaker slot away from a local speaker (caller holds the admission lock).
    fn demote_locked(&self, user_id: &UserId) {
        if let Some(mut r) = self.participant_roles.get_mut(user_id) {
            if r.role.can_speak() {
                self.local_speakers.fetch_sub(1, Ordering::AcqRel);
            }
            r.role = ChannelRole::Listener;
            r.waiting_since = Some(Instant::now());
            r.speak_attempt_ms = 0;
        }
    }

    /// Gives a waiting local member its granted role (caller holds the admission lock).
    fn admit_locked(&self, user_id: &UserId, now_ms: i64) {
        if let Some(mut r) = self.participant_roles.get_mut(user_id) {
            if !r.role.can_speak() && r.granted.can_speak() {
                self.local_speakers.fetch_add(1, Ordering::AcqRel);
            }
            r.role = r.granted;
            r.slot_since_ms = now_ms;
            r.waiting_since = None;
            r.speak_attempt_ms = 0;
        }
    }

    /// Local members waiting for a speaker slot, in admission order: priority speakers and
    /// moderators first, then by how long they have waited, then by id. Caller holds the
    /// admission lock.
    fn waiting_queue(&self, wanting_only: bool, now_ms: i64) -> Vec<UserId> {
        let mut queue: Vec<(bool, Instant, UserId)> = self
            .participant_roles
            .iter()
            .filter(|e| e.value().wants_slot())
            .filter(|e| {
                !wanting_only || now_ms - e.value().speak_attempt_ms <= SPEAK_ATTEMPT_FRESH_MS
            })
            .map(|e| {
                (
                    !(self.priority.contains_key(e.key()) || e.value().granted.can_moderate()),
                    e.value().waiting_since.unwrap_or_else(Instant::now),
                    *e.key(),
                )
            })
            .collect();
        queue.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2 .0.cmp(&b.2 .0)));
        queue.into_iter().map(|(_, _, uid)| uid).collect()
    }

    fn role_change(&self, user_id: &UserId, observers: Option<Vec<UserId>>) -> Option<RoleChange> {
        let session = self.participants.get(user_id)?;
        Some(RoleChange {
            user_id: *user_id,
            session_id: session.session_id,
            role: self.get_role(user_id),
            observers,
        })
    }

    /// Demotes `victim` and describes the change (caller holds the admission lock).
    fn demote_locked_announced(&self, victim: &UserId) -> Option<RoleChange> {
        let observers = self.observers_of(victim);
        self.demote_locked(victim);
        self.role_change(victim, observers)
    }

    fn admit_locked_announced(&self, user_id: &UserId, now_ms: i64) -> Option<RoleChange> {
        self.admit_locked(user_id, now_ms);
        let observers = self.observers_of(user_id);
        self.role_change(user_id, observers)
    }

    /// Hands free speaker slots to waiting local members (after a speaker left, a remote
    /// speaker went away or the cap was raised). Returns the members admitted.
    pub fn admit_waiting(&self) -> Vec<RoleChange> {
        let _slots = self.admission.lock();
        let max_speakers = self
            .config
            .read()
            .audience
            .map(|a| a.max_speakers)
            .unwrap_or(0);
        let now_ms = Utc::now().timestamp_millis();
        let queue = self.waiting_queue(false, now_ms);
        let mut admitted = Vec::new();
        for uid in queue {
            if max_speakers > 0 && self.speaker_count() >= max_speakers {
                break;
            }
            admitted.extend(self.admit_locked_announced(&uid, now_ms));
        }
        admitted
    }

    /// `speaker_admission = "demote"`: while members that are trying to speak wait for a slot,
    /// idle speakers yield theirs. Returns the changes in pairs (demoted, then admitted).
    pub fn rotate_idle_speakers(&self) -> Vec<RoleChange> {
        let Some(audience) = self.config.read().audience else {
            return Vec::new();
        };
        if audience.speaker_admission != SpeakerAdmission::Demote || audience.max_speakers == 0 {
            return Vec::new();
        }
        let _slots = self.admission.lock();
        let now_ms = Utc::now().timestamp_millis();
        let mut changes = Vec::new();
        for waiting in self.waiting_queue(true, now_ms) {
            if self.speaker_count() < audience.max_speakers {
                changes.extend(self.admit_locked_announced(&waiting, now_ms));
                continue;
            }
            let privileged = self.priority.contains_key(&waiting)
                || self
                    .participant_roles
                    .get(&waiting)
                    .is_some_and(|r| r.granted.can_moderate());
            let Some(victim) = self.idle_speaker(now_ms, demote_idle_for(privileged, &audience))
            else {
                break;
            };
            changes.extend(self.demote_locked_announced(&victim));
            changes.extend(self.admit_locked_announced(&waiting, now_ms));
        }
        changes
    }

    /// Records that a waiting member tried to send audio (the frame was dropped); such
    /// members are the ones an idle speaker yields to.
    pub fn note_speak_attempt(&self, user_id: &UserId) {
        if let Some(mut r) = self.participant_roles.get_mut(user_id) {
            if r.wants_slot() {
                r.speak_attempt_ms = Utc::now().timestamp_millis();
            }
        }
    }

    /// Whether a local member holds a speaking grant but no speaker slot.
    pub fn is_waiting_to_speak(&self, user_id: &UserId) -> bool {
        self.participant_roles
            .get(user_id)
            .is_some_and(|r| r.wants_slot())
    }

    /// The role a local member's grant carries (its effective role is [`MediaChannel::get_role`]).
    pub fn granted_role(&self, user_id: &UserId) -> Option<ChannelRole> {
        self.participant_roles.get(user_id).map(|r| r.granted)
    }

    /// Updates the effective role of a member hosted on another node. Returns `false` when
    /// they are unknown here.
    pub fn set_remote_role(&self, user_id: &UserId, role: ChannelRole) -> bool {
        match self.remote.get_mut(user_id) {
            Some(mut r) => {
                r.role = role;
                true
            }
            None => false,
        }
    }

    pub fn remove_participant(&self, user_id: &UserId) -> Option<Arc<MediaSession>> {
        if let Some((_, session)) = self.participants.remove(user_id) {
            self.ssrc_map.remove(&session.ssrc);
            self.priority.remove(user_id);
            if let Some((_, local)) = self.participant_roles.remove(user_id) {
                if local.role.can_speak() {
                    self.local_speakers.fetch_sub(1, Ordering::AcqRel);
                }
            }
            self.positions.remove(user_id);
            self.forget_visibility(user_id);
            for other in self.participants.iter() {
                let other = other.value();
                other
                    .ambient
                    .lock()
                    .forget_sender(&self.channel_id, user_id);
                other
                    .stream_cap
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
            let other = other.value();
            other
                .ambient
                .lock()
                .forget_sender(&self.channel_id, user_id);
            other
                .stream_cap
                .lock()
                .forget_sender(&self.channel_id, user_id);
        }
        observers
    }

    pub fn remote_count(&self) -> usize {
        self.remote.len()
    }

    /// Marks a member (local or remote) as a priority speaker or not. Returns `false` when
    /// they are not in the channel.
    pub fn set_priority(&self, user_id: &UserId, priority: bool) -> bool {
        if self.participants.contains_key(user_id) {
            if priority {
                self.priority.insert(*user_id, ());
            } else {
                self.priority.remove(user_id);
            }
            return true;
        }
        if let Some(mut r) = self.remote.get_mut(user_id) {
            r.is_priority = priority;
            return true;
        }
        false
    }

    /// Whether `user_id` was explicitly made a priority speaker (grant or promotion); the
    /// flag carried in rosters. See [`MediaChannel::is_priority`] for the effective state.
    pub fn has_priority_flag(&self, user_id: &UserId) -> bool {
        self.priority.contains_key(user_id)
            || self.remote.get(user_id).is_some_and(|r| r.is_priority)
    }

    /// Whether `user_id`'s speech ducks the others: the explicit flag, or a moderator role
    /// when `ducking.moderators` is on. Always `false` in channels without `ducking`.
    pub fn is_priority(&self, user_id: &UserId) -> bool {
        let Some(cfg) = self.config.read().ducking else {
            return false;
        };
        self.has_priority_flag(user_id)
            || (cfg.moderators && self.member_role(user_id).is_some_and(|r| r.can_moderate()))
    }

    pub fn ducking_config(&self) -> Option<DuckingConfig> {
        self.config.read().ducking
    }

    /// Whether a priority speaker is currently holding the channel ducked.
    pub fn ducking_active(&self) -> bool {
        self.ducking.lock().is_active(Instant::now())
    }

    /// Ducking gain to apply to `sender_uid`'s current frame (`1.0` = none). A priority
    /// speaker's audible frame re-arms the hold instead and is never ducked itself.
    fn ducking_gain(&self, sender_uid: &UserId, audible: bool, now: Instant) -> f32 {
        let Some(cfg) = self.config.read().ducking else {
            return 1.0;
        };
        let mut env = self.ducking.lock();
        if self.is_priority(sender_uid) {
            if audible {
                env.trigger(now, &cfg);
            }
            return 1.0;
        }
        env.gain(now, &cfg)
    }

    /// Members (local and remote) that may speak.
    pub fn speaker_count(&self) -> u32 {
        let remote = self.remote.iter().filter(|r| r.role.can_speak()).count() as u32;
        self.local_speakers.load(Ordering::Relaxed) + remote
    }

    /// Role of a local or remote member (`Listener` for strangers).
    pub fn role_of(&self, user_id: &UserId) -> ChannelRole {
        self.member_role(user_id).unwrap_or(ChannelRole::Listener)
    }

    /// Role of a current member, local or remote (`None`: not in the channel).
    pub fn member_role(&self, user_id: &UserId) -> Option<ChannelRole> {
        if let Some(r) = self.participant_roles.get(user_id) {
            return Some(r.value().role);
        }
        self.remote.get(user_id).map(|r| r.role)
    }

    /// `ChannelConfig::audience.hide_listeners`: receive-only members stay out of presence.
    pub fn hides_listeners(&self) -> bool {
        self.config.read().hides_listeners()
    }

    /// Whether `user_id` is a receive-only member that presence must not disclose.
    pub fn is_hidden_listener(&self, user_id: &UserId) -> bool {
        let Some(role) = self.member_role(user_id) else {
            return false;
        };
        if role.can_speak() {
            return false;
        }
        let cfg = self.config.read();
        cfg.hides_listeners()
            && !(self.channel_type == ChannelType::Command
                && cfg
                    .command_speakers
                    .as_ref()
                    .is_some_and(|s| s.contains(user_id)))
    }

    /// Whether native receivers in `DownlinkMode::Mixed` must be served (`mix_for_listeners`
    /// makes every listener mixed).
    pub fn wants_mix(&self, receiver: &MediaSession) -> bool {
        receiver.downlink_mode() == DownlinkMode::Mixed
            || (self.config.read().mixes_for_listeners()
                && !self.role_of(&receiver.user_id).can_speak())
    }

    /// True when every member of this channel hears the same audio apart from their own
    /// receiver preferences: no distance attenuation / direction, no ambient slot table, no
    /// whisper target. Such channels can share one server mix between receivers.
    pub fn supports_shared_mix(&self) -> bool {
        matches!(self.channel_type, ChannelType::Team | ChannelType::Command)
            && self.config.read().ambient.is_none()
    }

    /// Cheap pre-check for the router: could any member of this channel be served by a
    /// server mix right now?
    pub fn may_mix(&self) -> bool {
        self.config.read().mixes_for_listeners()
            || self
                .participants
                .iter()
                .any(|p| p.value().downlink_mode() == DownlinkMode::Mixed)
    }

    /// Frames into this channel must be end-to-end encrypted (`ChannelConfig::e2ee`).
    pub fn is_e2ee(&self) -> bool {
        self.config.read().e2ee
    }

    /// Senders may encode two channels (`ChannelConfig::stereo`).
    pub fn is_stereo(&self) -> bool {
        self.config.read().stereo
    }

    /// Uplinks into this channel are denoised by the node (`ChannelConfig::noise_suppression`).
    pub fn requires_noise_suppression(&self) -> bool {
        self.config.read().noise_suppression
    }

    /// The per-receiver stream cap (`audience.max_streams`) as a slot table configuration:
    /// `max_streams` slots, losers silenced.
    fn stream_cap(&self) -> Option<AmbientConfig> {
        let max = self.config.read().audience.map(|a| a.max_streams)?;
        (max > 0).then_some(AmbientConfig {
            max_voices: max,
            ambient_gain: 0.0,
        })
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
        if self.is_hidden_listener(user_id) {
            return Some(Vec::new());
        }
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

    pub fn positional_config(&self) -> Option<PositionalConfig> {
        self.config.read().positional_config.clone()
    }

    /// True when a browser can reproduce the receiver-side gain of this channel's speakers
    /// from what it knows (its own preferences, focus and positions): the gain then contains
    /// no ambient slot table, whose state only the server tracks. Speakers of channels where
    /// this is false stay in the browser's mixed track.
    pub fn browser_reproducible_gain(&self) -> bool {
        self.config.read().ambient.is_none()
    }

    /// Whether `observer` currently sees `subject` in this channel: never when `subject` is a
    /// hidden listener, always without a roster radius, otherwise only while the pair is
    /// within it (both poses known). Everybody sees themselves.
    pub fn sees(&self, observer: &UserId, subject: &UserId) -> bool {
        if observer == subject {
            return true;
        }
        if self.is_hidden_listener(subject) {
            return false;
        }
        if self.roster_radius().is_none() {
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

    /// The two worst downlink loss reports among local members, as `(user, loss percent)`,
    /// worst first; the second lets a sender exclude its own report. Members that reported
    /// nothing yet count as lossless.
    pub fn worst_receiver_loss(&self) -> [Option<(UserId, f32)>; 2] {
        let mut worst: [Option<(UserId, f32)>; 2] = [None, None];
        for e in self.participants.iter() {
            let loss = e.value().get_quality().packet_loss_percent;
            let loss = if loss.is_finite() {
                loss.clamp(0.0, 100.0)
            } else {
                0.0
            };
            let entry = (*e.key(), loss);
            if worst[0].is_none_or(|(_, l)| loss > l) {
                worst[1] = worst[0];
                worst[0] = Some(entry);
            } else if worst[1].is_none_or(|(_, l)| loss > l) {
                worst[1] = Some(entry);
            }
        }
        worst
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
                is_priority: self.priority.contains_key(&s.user_id),
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
                is_priority: r.is_priority,
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
                is_priority: self.priority.contains_key(&s.user_id),
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
            is_priority: r.is_priority,
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

    /// True if `user_id` may transmit in this channel: members whose grant had `speak: false`
    /// (`ChannelRole::Listener`) are receive-only in every channel type; Command channels
    /// additionally admit the configured `command_speakers`.
    pub fn can_transmit(&self, user_id: &UserId) -> bool {
        if !self.participants.contains_key(user_id) {
            return false;
        }
        self.get_role(user_id).can_speak()
            || (self.channel_type == ChannelType::Command
                && self
                    .config
                    .read()
                    .command_speakers
                    .as_ref()
                    .is_some_and(|s| s.contains(user_id)))
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
        // Hidden listeners take part in the distance bookkeeping (they must see others) but
        // never appear as a subject.
        let mover_hidden = self.is_hidden_listener(user_id);
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
            let other_hidden = self.is_hidden_listener(&other_id);
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
            if other_local && !mover_hidden {
                changes.push(RosterChange {
                    observer: other_id,
                    subject: *user_id,
                    visible: now,
                });
            }
            if mover_local && !other_hidden {
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
            .map(|r| r.value().role)
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
        let cap = self.stream_cap();
        let ducking = self.config.read().ducking.is_some();
        let ranked = ambient.is_some() || cap.is_some();
        let now = (ranked || ducking).then(Instant::now);
        // Sender-reported level of the current frame (unlabeled frames rank at nominal
        // loudness).
        let level = now
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
        // Priority ducking: one gain per frame for every receiver, computed before the
        // per-receiver ranking so a ducked voice competes for slots at what is heard. A frame
        // counts as speech when its label says so, or — unlabeled — when the sender's
        // speaking state (just refreshed by the router for local senders) does.
        let duck = match now {
            Some(now) if ducking => {
                let audible = match reported_level {
                    Some(l) => l < aurix_common::protocol::AUDIO_LEVEL_SILENCE,
                    None => self
                        .participants
                        .get(sender_uid)
                        .is_none_or(|s| s.is_speaking.load(Ordering::Relaxed)),
                };
                self.ducking_gain(sender_uid, audible, now)
            }
            _ => 1.0,
        };
        receivers
            .into_iter()
            .filter_map(|(receiver, mix)| {
                let gain = receiver.gain_for(sender_uid, &self.channel_id)?;
                let mut v = mix.volume * gain * duck;
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
                // Stream cap: each receiver keeps only the `max_streams` loudest concurrent
                // voices (by what *they* would hear: gain × sender level); the rest are
                // dropped for them. Ranked after ambient dimming so focused voices win.
                if let (Some(cfg), Some(now)) = (cap.as_ref(), now) {
                    if v > 0.001
                        && receiver.stream_cap.lock().gate(
                            &self.channel_id,
                            sender_uid,
                            v * level,
                            cfg,
                            now,
                        ) <= 0.0
                    {
                        aurix_metrics::STREAMS_CAPPED.inc();
                        return None;
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
    use aurix_common::protocol::AUDIO_LEVEL_SILENCE;
    use std::time::Duration;

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
    fn worst_receiver_loss_ranks_reports_and_keeps_a_runner_up() {
        let app = AppId::new();
        let ch = MediaChannel::new(ChannelId::new(), app, ChannelConfig::default());
        assert_eq!(ch.worst_receiver_loss(), [None, None]);
        let a = session(app, 1);
        let b = session(app, 2);
        let c = session(app, 3);
        for s in [&a, &b, &c] {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        let report = |s: &MediaSession, loss: f32| {
            s.update_quality(QualityMetrics {
                rtt_ms: 30.0,
                jitter_ms: 2.0,
                packet_loss_percent: loss,
                bitrate_kbps: 32,
                mos_score: 0.0,
            })
        };
        report(&a, 12.0);
        report(&b, f32::NAN);
        report(&c, 4.0);
        let [first, second] = ch.worst_receiver_loss();
        assert_eq!(first, Some((a.user_id, 12.0)));
        assert_eq!(second, Some((c.user_id, 4.0)));
        report(&b, 250.0);
        let [first, second] = ch.worst_receiver_loss();
        assert_eq!(first, Some((b.user_id, 100.0)));
        assert_eq!(second, Some((a.user_id, 12.0)));
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
                is_priority: false,
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

    fn audience(cfg: AudienceConfig) -> ChannelConfig {
        ChannelConfig {
            channel_type: ChannelType::Team,
            audience: Some(cfg),
            ..ChannelConfig::default()
        }
    }

    fn hears(got: &[(Arc<MediaSession>, Mix)], who: &Arc<MediaSession>) -> Option<f32> {
        got.iter()
            .find(|(s, _)| s.user_id == who.user_id)
            .map(|(_, m)| m.volume)
    }

    #[test]
    fn stream_cap_ranks_per_receiver_and_drops_the_rest() {
        let app = AppId::new();
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            audience(AudienceConfig {
                max_streams: 2,
                ..AudienceConfig::default()
            }),
        );
        let alice = session(app, 10);
        let bob = session(app, 11);
        let speakers: Vec<_> = (1..=3).map(|i| session(app, i)).collect();
        for s in [&alice, &bob] {
            ch.add_participant(s.clone(), ChannelRole::Listener)
                .unwrap();
        }
        for s in &speakers {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        // Bob barely hears speaker 1; Alice hears everyone at nominal volume.
        bob.prefs.write().set_gain(speakers[0].user_id, 0.05);

        let f1 = ch.get_receivers_for_audio(1);
        let f2 = ch.get_receivers_for_audio(2);
        assert_eq!(hears(&f1, &alice), Some(1.0));
        assert_eq!(hears(&f1, &bob), Some(0.05));
        assert_eq!(hears(&f2, &alice), Some(1.0));
        assert_eq!(hears(&f2, &bob), Some(1.0));

        // The third equally loud voice does not fit for Alice (both slots are held) but it
        // pushes the quiet speaker 1 out for Bob: the cap is ranked per receiver.
        let f3 = ch.get_receivers_for_audio(3);
        assert_eq!(hears(&f3, &alice), None, "alice: capped");
        assert_eq!(
            hears(&f3, &bob),
            Some(1.0),
            "bob: speaker 3 replaces the quiet one"
        );
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &alice), Some(1.0));
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &bob), None);
        // Other speakers keep hearing everyone: they have slots of their own.
        assert_eq!(hears(&f3, &speakers[0]), Some(1.0));
        assert_eq!(hears(&f3, &speakers[1]), Some(1.0));

        // A speaker Alice muted never takes one of her slots, so the third voice fits.
        alice
            .prefs
            .write()
            .set_muted(speakers[1].user_id, Some(ch.channel_id), true);
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &alice), None);
        ch.remove_participant(&speakers[1].user_id);
        ch.add_participant(speakers[1].clone(), ChannelRole::Speaker)
            .unwrap();
        assert_eq!(hears(&ch.get_receivers_for_audio(3), &alice), Some(1.0));

        // Holders keep their slots against an equally loud challenger, but a holder whose
        // frames report a much quieter level loses it (rank = delivery gain × sender level).
        speakers[1].record_audio_level(Some(0), 0.0);
        alice
            .prefs
            .write()
            .set_muted(speakers[1].user_id, Some(ch.channel_id), false);
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &alice), None);
        speakers[0].record_audio_level(Some(40), 0.0);
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &alice), None);
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &alice), Some(1.0));
        assert_eq!(hears(&ch.get_receivers_for_audio(3), &alice), Some(1.0));
    }

    #[test]
    fn stream_cap_ranks_by_what_the_receiver_would_hear() {
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
                audience: Some(AudienceConfig {
                    max_streams: 1,
                    ..AudienceConfig::default()
                }),
                ..ChannelConfig::default()
            },
        );
        let listener = session(app, 10);
        let near = session(app, 1);
        let far = session(app, 2);
        let blocked = session(app, 3);
        for s in [&listener, &near, &far, &blocked] {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        let o = Orientation3D::default;
        ch.update_position(&listener.user_id, Position3D::new(0.0, 0.0, 0.0), o());
        ch.update_position(&near.user_id, Position3D::new(0.0, 0.0, 0.0), o());
        ch.update_position(&far.user_id, Position3D::new(6.0, 0.0, 0.0), o()); // 0.5
        ch.update_position(&blocked.user_id, Position3D::new(0.0, 0.0, 0.0), o());
        listener.prefs.write().set_blocked(blocked.user_id, true);

        // A blocked speaker is filtered before ranking: they never occupy the slot.
        assert_eq!(hears(&ch.get_receivers_for_audio(3), &listener), None);
        // The far speaker takes the single slot at positional 0.5 ...
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &listener), Some(0.5));
        // ... and loses it to the near speaker, who ranks at 1.0 > 0.5 x stickiness.
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &listener), Some(1.0));
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &listener), None);
        // Slots are per receiver: the blocked speaker (audible to everyone else) holds the
        // far speaker's slot at 0.5 x stickiness, so near (0.5 to them) is dropped for far,
        // while near takes the slot from far at the blocked speaker.
        let f1 = ch.get_receivers_for_audio(1);
        assert_eq!(hears(&f1, &far), None);
        assert_eq!(hears(&f1, &blocked), Some(1.0));

        // Focus attenuation applies to the ranked volume as well.
        std::thread::sleep(crate::ambient::VOICE_HOLD + Duration::from_millis(20));
        ch.update_position(&far.user_id, Position3D::new(0.0, 0.0, 0.0), o());
        listener.prefs.write().set_focus(Some(ChannelId::new()));
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &listener), Some(0.5));
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &listener), None);
        listener.prefs.write().set_focus(Some(ch.channel_id));
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &listener), Some(1.0));
        // Near holds the slot against an equally loud challenger (1.0 x 1.2 stickiness) ...
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &listener), None);
        // ... until it falls silent for VOICE_HOLD: the stale voice is dropped and the slot
        // goes to whoever speaks next.
        std::thread::sleep(crate::ambient::VOICE_HOLD + Duration::from_millis(20));
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &listener), Some(1.0));
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &listener), None);
    }

    #[test]
    fn listeners_never_transmit_in_any_channel_type() {
        let app = AppId::new();
        for channel_type in [
            ChannelType::Team,
            ChannelType::Positional,
            ChannelType::Echo,
            ChannelType::Command,
            ChannelType::Whisper,
        ] {
            let ch = MediaChannel::new(
                ChannelId::new(),
                app,
                ChannelConfig {
                    channel_type,
                    ..ChannelConfig::default()
                },
            );
            let listener = session(app, 1);
            let speaker = session(app, 2);
            ch.add_participant(listener.clone(), ChannelRole::Listener)
                .unwrap();
            ch.add_participant(speaker.clone(), ChannelRole::Speaker)
                .unwrap();
            assert!(!ch.can_transmit(&listener.user_id), "{channel_type:?}");
            assert!(ch.can_transmit(&speaker.user_id) || channel_type == ChannelType::Command);
            let speaker_receivers = ch.get_receivers_for_audio(2);
            if matches!(channel_type, ChannelType::Team | ChannelType::Whisper) {
                assert_eq!(hears(&speaker_receivers, &listener), Some(1.0));
            }
        }
        // Command channels: `command_speakers` may transmit even with a listener grant.
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Command,
                ..ChannelConfig::default()
            },
        );
        let commander = session(app, 1);
        let listener = session(app, 2);
        ch.add_participant(commander.clone(), ChannelRole::Listener)
            .unwrap();
        ch.add_participant(listener.clone(), ChannelRole::Listener)
            .unwrap();
        assert!(!ch.can_transmit(&commander.user_id));
        ch.update_config(ChannelConfig {
            channel_type: ChannelType::Command,
            command_speakers: Some(vec![commander.user_id]),
            ..ChannelConfig::default()
        });
        assert!(ch.can_transmit(&commander.user_id));
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &listener), Some(1.0));
    }

    #[test]
    fn hidden_listeners_are_invisible_to_others_but_see_everyone() {
        let app = AppId::new();
        let ch = MediaChannel::new(ChannelId::new(), app, audience(AudienceConfig::default()));
        let speaker = session(app, 1);
        let listener = session(app, 2);
        let other_listener = session(app, 3);
        ch.add_participant(speaker.clone(), ChannelRole::Speaker)
            .unwrap();
        ch.add_participant(listener.clone(), ChannelRole::Listener)
            .unwrap();
        ch.add_participant(other_listener.clone(), ChannelRole::Listener)
            .unwrap();
        let remote_listener = UserId::new();
        ch.add_remote(
            remote_listener,
            RemoteParticipant {
                session_id: SessionId::new(),
                display_name: "remote".into(),
                ssrc: 99,
                role: ChannelRole::Listener,
                is_muted: false,
                is_priority: false,
            },
        );

        assert!(ch.hides_listeners());
        assert!(ch.is_hidden_listener(&listener.user_id));
        assert!(ch.is_hidden_listener(&remote_listener));
        assert!(!ch.is_hidden_listener(&speaker.user_id));
        assert!(
            !ch.is_hidden_listener(&UserId::new()),
            "non-members are not hidden"
        );

        let names = |who: &UserId| -> Vec<String> {
            let mut v: Vec<String> = ch
                .roster_for(who)
                .into_iter()
                .map(|e| e.display_name)
                .collect();
            v.sort();
            v
        };
        assert_eq!(names(&speaker.user_id), Vec::<String>::new());
        assert_eq!(names(&listener.user_id), vec!["u1".to_string()]);
        assert_eq!(names(&other_listener.user_id), vec!["u1".to_string()]);
        assert!(ch.sees(&listener.user_id, &listener.user_id), "self");
        assert!(!ch.sees(&speaker.user_id, &listener.user_id));
        assert!(ch.sees(&listener.user_id, &speaker.user_id));
        // Counts still include them.
        assert_eq!(ch.participant_count(), 3);
        assert_eq!(ch.remote_count(), 1);
        assert_eq!(ch.speaker_count(), 1);

        // Audio still reaches them (mixed by default), and they may switch to streams.
        assert!(ch.wants_mix(&listener) && ch.may_mix());
        assert!(!ch.wants_mix(&speaker));
        assert!(ch.supports_shared_mix());
        assert_eq!(hears(&ch.get_receivers_for_audio(1), &listener), Some(1.0));

        // Turning `hide_listeners` off reveals them.
        ch.update_config(audience(AudienceConfig {
            hide_listeners: false,
            ..AudienceConfig::default()
        }));
        assert_eq!(names(&speaker.user_id).len(), 3);
    }

    #[test]
    fn hidden_listeners_stay_out_of_roster_radius_transitions() {
        let app = AppId::new();
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                channel_type: ChannelType::Positional,
                positional_config: Some(PositionalConfig {
                    roster_radius: Some(10.0),
                    ..PositionalConfig::default()
                }),
                audience: Some(AudienceConfig::default()),
                ..ChannelConfig::default()
            },
        );
        let speaker = session(app, 1);
        let listener = session(app, 2);
        ch.add_participant(speaker.clone(), ChannelRole::Speaker)
            .unwrap();
        ch.add_participant(listener.clone(), ChannelRole::Listener)
            .unwrap();
        let o = Orientation3D::default;
        assert!(ch
            .update_position(&speaker.user_id, Position3D::new(0.0, 0.0, 0.0), o())
            .is_empty());
        let changes = ch.update_position(&listener.user_id, Position3D::new(0.0, 0.0, 0.0), o());
        // Only the listener learns about the speaker; the speaker is told nothing.
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].observer, listener.user_id);
        assert_eq!(changes[0].subject, speaker.user_id);
        assert!(changes[0].visible);
        assert_eq!(ch.observers_of(&listener.user_id), Some(Vec::new()));
        assert_eq!(
            ch.observers_of(&speaker.user_id),
            Some(vec![listener.user_id])
        );
    }

    #[test]
    fn speaker_admission_is_atomic_and_counts_remote_speakers() {
        let app = AppId::new();
        let ch = Arc::new(MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                max_participants: 1000,
                ..audience(AudienceConfig {
                    max_speakers: 8,
                    ..AudienceConfig::default()
                })
            },
        ));
        ch.add_remote(
            UserId::new(),
            RemoteParticipant {
                session_id: SessionId::new(),
                display_name: "remote".into(),
                ssrc: 5000,
                role: ChannelRole::Speaker,
                is_muted: false,
                is_priority: false,
            },
        );
        let admitted = Arc::new(AtomicU32::new(0));
        let listeners_admitted = Arc::new(AtomicU32::new(0));
        let threads: Vec<_> = (0..16)
            .map(|t| {
                let ch = ch.clone();
                let admitted = admitted.clone();
                let listeners_admitted = listeners_admitted.clone();
                std::thread::spawn(move || {
                    for i in 0..20u32 {
                        let ssrc = 1 + t * 100 + i;
                        let role = if i % 2 == 0 {
                            ChannelRole::Speaker
                        } else {
                            ChannelRole::Listener
                        };
                        if ch.add_participant(session(app, ssrc), role).is_ok() {
                            if role.can_speak() {
                                admitted.fetch_add(1, Ordering::Relaxed);
                            } else {
                                listeners_admitted.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // 7 local + 1 remote = 8; listeners are never turned away by the speaker cap.
        assert_eq!(admitted.load(Ordering::Relaxed), 7);
        assert_eq!(listeners_admitted.load(Ordering::Relaxed), 160);
        assert_eq!(ch.speaker_count(), 8);
        assert_eq!(ch.participant_count(), 167);

        // A leaving speaker frees exactly one slot; a re-joining speaker keeps theirs.
        let one = ch
            .participants
            .iter()
            .find(|e| ch.get_role(e.key()).can_speak())
            .map(|e| e.value().clone())
            .unwrap();
        assert!(ch
            .add_participant(one.clone(), ChannelRole::Speaker)
            .is_ok());
        assert_eq!(ch.speaker_count(), 8);
        assert!(ch
            .add_participant(session(app, 9000), ChannelRole::Speaker)
            .is_err());
        ch.remove_participant(&one.user_id);
        assert_eq!(ch.speaker_count(), 7);
        assert!(ch
            .add_participant(session(app, 9000), ChannelRole::Speaker)
            .is_ok());
        assert!(ch
            .add_participant(session(app, 9001), ChannelRole::Speaker)
            .is_err());
        // Moderators need a slot too; the speaker cap does not apply to listeners.
        assert!(ch
            .add_participant(session(app, 9002), ChannelRole::Moderator)
            .is_err());
        assert!(ch
            .add_participant(session(app, 9003), ChannelRole::Listener)
            .is_ok());
    }

    #[test]
    fn channels_grow_past_the_old_default_and_stop_at_max_participants() {
        let app = AppId::new();
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                max_participants: 2000,
                ..ChannelConfig::default()
            },
        );
        let speaker = session(app, 1);
        ch.add_participant(speaker.clone(), ChannelRole::Speaker)
            .unwrap();
        for i in 2..=2000u32 {
            ch.add_participant(session(app, i), ChannelRole::Listener)
                .unwrap();
        }
        assert_eq!(ch.participant_count(), 2000);
        assert!(matches!(
            ch.add_participant(session(app, 2001), ChannelRole::Listener),
            Err(AurixError::ChannelFull(_))
        ));
        // One speaker frame fans out to all 1999 listeners (and not back to the speaker).
        let got = ch.get_receivers_for_audio(1);
        assert_eq!(got.len(), 1999);
        assert!(got
            .iter()
            .all(|(s, m)| s.user_id != speaker.user_id && m.volume == 1.0));
    }

    #[test]
    fn duck_envelope_attacks_holds_and_releases() {
        let cfg = DuckingConfig {
            gain: 0.2,
            attack_ms: 100,
            release_ms: 400,
            hold_ms: 250,
            moderators: false,
        };
        let t0 = Instant::now();
        let mut env = DuckEnvelope::default();
        assert_eq!(env.gain(t0, &cfg), 1.0, "idle: unity");
        env.trigger(t0, &cfg);
        assert!(env.is_active(t0));
        // Half-way through the attack the depth is 0.5 → gain 0.6.
        let g = env.gain(t0 + Duration::from_millis(50), &cfg);
        assert!((g - 0.6).abs() < 1e-4, "mid-attack gain {g}");
        let g = env.gain(t0 + Duration::from_millis(200), &cfg);
        assert!((g - 0.2).abs() < 1e-4, "fully ducked {g}");
        // Still held at 240 ms; release starts after the hold elapses.
        assert!(env.is_active(t0 + Duration::from_millis(240)));
        assert!(!env.is_active(t0 + Duration::from_millis(251)));
        let g = env.gain(t0 + Duration::from_millis(450), &cfg);
        assert!((g - 0.6).abs() < 1e-3, "half released {g}");
        // A new trigger during the release ramps back up along the attack.
        env.trigger(t0 + Duration::from_millis(450), &cfg);
        let g = env.gain(t0 + Duration::from_millis(500), &cfg);
        assert!((g - 0.2).abs() < 1e-3, "re-attacked {g}");
        let g = env.gain(t0 + Duration::from_millis(2000), &cfg);
        assert_eq!(g, 1.0, "released to unity");
    }

    fn ducked(cfg: DuckingConfig) -> ChannelConfig {
        ChannelConfig {
            channel_type: ChannelType::Team,
            ducking: Some(cfg),
            ..ChannelConfig::default()
        }
    }

    const INSTANT_DUCK: DuckingConfig = DuckingConfig {
        gain: 0.25,
        attack_ms: 0,
        release_ms: 0,
        hold_ms: 10_000,
        moderators: false,
    };

    #[test]
    fn priority_speech_ducks_everyone_else_but_keeps_local_prefs() {
        let app = AppId::new();
        let ch = MediaChannel::new(ChannelId::new(), app, ducked(INSTANT_DUCK));
        let leader = session(app, 1);
        let bob = session(app, 2);
        let carol = session(app, 3);
        for s in [&leader, &bob, &carol] {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        assert!(ch.set_priority(&leader.user_id, true));
        assert!(!ch.set_priority(&UserId::new(), true), "non-member");
        assert!(ch.is_priority(&leader.user_id));
        assert!(!ch.is_priority(&bob.user_id));
        carol.prefs.write().set_gain(bob.user_id, 0.5);

        // Nobody with priority is talking: unity everywhere.
        let f = ch.get_receivers_for_audio(2);
        assert_eq!(hears(&f, &leader), Some(1.0));
        assert_eq!(hears(&f, &carol), Some(0.5));
        assert!(!ch.ducking_active());

        // The leader's own frames arrive at full volume and arm the envelope.
        leader.mark_audio_activity();
        let f = ch.get_receivers_for_audio(1);
        assert_eq!(hears(&f, &bob), Some(1.0));
        assert_eq!(hears(&f, &carol), Some(1.0));
        assert!(ch.ducking_active());

        // Bob is ducked for everyone — including the leader — on top of local gain.
        let f = ch.get_receivers_for_audio(2);
        assert_eq!(hears(&f, &leader), Some(0.25));
        assert_eq!(hears(&f, &carol), Some(0.125));
        // Local mute and zero gain still remove the voice entirely.
        carol
            .prefs
            .write()
            .set_muted(bob.user_id, Some(ch.channel_id), true);
        assert_eq!(hears(&ch.get_receivers_for_audio(2), &carol), None);

        // A second priority speaker is never ducked, even while the first one talks.
        assert!(ch.set_priority(&carol.user_id, true));
        carol.mark_audio_activity();
        let f = ch.get_receivers_for_audio(3);
        assert_eq!(hears(&f, &leader), Some(1.0));
        assert_eq!(hears(&f, &bob), Some(1.0));

        // Demotion takes effect on the next frame.
        assert!(ch.set_priority(&carol.user_id, false));
        assert_eq!(hears(&ch.get_receivers_for_audio(3), &bob), Some(0.25));
    }

    #[test]
    fn ducking_needs_channel_config_and_may_include_moderators() {
        let app = AppId::new();
        let plain = MediaChannel::new(ChannelId::new(), app, ChannelConfig::default());
        let leader = session(app, 1);
        let bob = session(app, 2);
        for s in [&leader, &bob] {
            plain
                .add_participant(s.clone(), ChannelRole::Moderator)
                .unwrap();
        }
        plain.set_priority(&leader.user_id, true);
        leader.mark_audio_activity();
        assert!(plain.has_priority_flag(&leader.user_id));
        assert!(!plain.is_priority(&leader.user_id), "no ducking config");
        plain.get_receivers_for_audio(1);
        assert_eq!(hears(&plain.get_receivers_for_audio(2), &leader), Some(1.0));
        assert!(!plain.ducking_active());

        let mods = MediaChannel::new(
            ChannelId::new(),
            app,
            ducked(DuckingConfig {
                moderators: true,
                ..INSTANT_DUCK
            }),
        );
        let gm = session(app, 3);
        let player = session(app, 4);
        mods.add_participant(gm.clone(), ChannelRole::Moderator)
            .unwrap();
        mods.add_participant(player.clone(), ChannelRole::Speaker)
            .unwrap();
        assert!(mods.is_priority(&gm.user_id));
        assert!(!mods.has_priority_flag(&gm.user_id), "role, not flag");
        gm.mark_audio_activity();
        mods.get_receivers_for_audio(3);
        assert_eq!(hears(&mods.get_receivers_for_audio(4), &gm), Some(0.25));

        // Relayed frames from a remote priority speaker arm the envelope by their level label.
        let remote = MediaChannel::new(ChannelId::new(), app, ducked(INSTANT_DUCK));
        let listener = session(app, 5);
        let talker = session(app, 6);
        remote
            .add_participant(listener.clone(), ChannelRole::Speaker)
            .unwrap();
        remote
            .add_participant(talker.clone(), ChannelRole::Speaker)
            .unwrap();
        let far = UserId::new();
        remote.add_remote(
            far,
            RemoteParticipant {
                session_id: SessionId::new(),
                display_name: "far".into(),
                ssrc: 99,
                role: ChannelRole::Speaker,
                is_muted: false,
                is_priority: true,
            },
        );
        remote.get_receivers_for_relayed_audio(&far, Some(AUDIO_LEVEL_SILENCE));
        assert!(!remote.ducking_active(), "silence label does not arm");
        remote.get_receivers_for_relayed_audio(&far, Some(30));
        assert!(remote.ducking_active());
        assert_eq!(
            hears(&remote.get_receivers_for_audio(6), &listener),
            Some(0.25)
        );
    }

    fn capped(app: AppId, max_speakers: u32, admission: SpeakerAdmission) -> MediaChannel {
        MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                audience: Some(AudienceConfig {
                    hide_listeners: false,
                    max_speakers,
                    speaker_admission: admission,
                    demote_idle_ms: 50,
                    ..AudienceConfig::default()
                }),
                ..ChannelConfig::default()
            },
        )
    }

    fn remote(role: ChannelRole) -> RemoteParticipant {
        RemoteParticipant {
            session_id: SessionId::new(),
            display_name: "remote".into(),
            ssrc: 900,
            role,
            is_muted: false,
            is_priority: false,
        }
    }

    fn speak(session: &MediaSession, level: u8) {
        session.record_audio_level(Some(level), 0.0);
    }

    #[test]
    fn speaker_cap_rejects_waits_or_admits_by_policy() {
        let app = AppId::new();
        for admission in [SpeakerAdmission::Reject, SpeakerAdmission::Wait] {
            let ch = capped(app, 1, admission);
            let first = session(app, 1);
            let second = session(app, 2);
            let listener = session(app, 3);
            let a = ch
                .add_participant(first.clone(), ChannelRole::Speaker)
                .unwrap();
            assert_eq!(
                (a.role, a.waiting, a.demoted),
                (ChannelRole::Speaker, false, None)
            );
            // Listeners never compete for speaker slots.
            let l = ch
                .add_participant(listener.clone(), ChannelRole::Listener)
                .unwrap();
            assert!(!l.waiting);
            match admission {
                SpeakerAdmission::Reject => {
                    assert!(matches!(
                        ch.add_participant(second.clone(), ChannelRole::Speaker),
                        Err(AurixError::ChannelFull(_))
                    ));
                    // A refused join leaves the participant count untouched.
                    assert_eq!(ch.participant_count(), 2);
                }
                _ => {
                    let a = ch
                        .add_participant(second.clone(), ChannelRole::Speaker)
                        .unwrap();
                    assert_eq!((a.role, a.waiting), (ChannelRole::Listener, true));
                    assert_eq!(ch.get_role(&second.user_id), ChannelRole::Listener);
                    assert_eq!(ch.granted_role(&second.user_id), Some(ChannelRole::Speaker));
                    assert!(ch.is_waiting_to_speak(&second.user_id));
                    assert!(!ch.can_transmit(&second.user_id));
                    assert_eq!(ch.speaker_count(), 1);
                    assert_eq!(ch.participant_count(), 3);
                    // Nothing to rotate in `wait` mode however hard the newcomer tries.
                    ch.note_speak_attempt(&second.user_id);
                    std::thread::sleep(Duration::from_millis(60));
                    assert!(ch.rotate_idle_speakers().is_empty());
                    // The slot goes to the waiting member when the holder leaves.
                    ch.remove_participant(&first.user_id);
                    let admitted = ch.admit_waiting();
                    assert_eq!(admitted.len(), 1);
                    assert_eq!(admitted[0].user_id, second.user_id);
                    assert_eq!(admitted[0].session_id, second.session_id);
                    assert_eq!(admitted[0].role, ChannelRole::Speaker);
                    assert!(admitted[0].observers.is_none());
                    assert!(ch.can_transmit(&second.user_id));
                    assert!(!ch.is_waiting_to_speak(&second.user_id));
                    assert_eq!(ch.speaker_count(), 1);
                    assert!(ch.admit_waiting().is_empty());
                }
            }
        }
    }

    #[test]
    fn unlimited_speakers_never_wait() {
        let app = AppId::new();
        let ch = capped(app, 0, SpeakerAdmission::Demote);
        for i in 1..=5 {
            let a = ch
                .add_participant(session(app, i), ChannelRole::Speaker)
                .unwrap();
            assert!(!a.waiting && a.demoted.is_none());
        }
        assert_eq!(ch.speaker_count(), 5);
        assert!(ch.rotate_idle_speakers().is_empty());
        assert!(ch.admit_waiting().is_empty());
    }

    #[test]
    fn demote_mode_takes_the_slot_of_the_idlest_quietest_plain_speaker() {
        let app = AppId::new();
        let ch = capped(app, 3, SpeakerAdmission::Demote);
        let talker = session(app, 1);
        let quiet = session(app, 2);
        let loud = session(app, 3);
        for s in [&talker, &quiet, &loud] {
            ch.add_participant(s.clone(), ChannelRole::Speaker).unwrap();
        }
        // quiet and loud last spoke at the same instant (level bytes: higher = quieter);
        // the talker keeps talking.
        speak(&quiet, 100);
        speak(&loud, 20);
        let spoke_at = Utc::now().timestamp_millis();
        quiet.last_audio_at_ms.store(spoke_at, Ordering::Relaxed);
        loud.last_audio_at_ms.store(spoke_at, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(60));
        speak(&talker, 10);
        let newcomer = session(app, 4);
        let a = ch
            .add_participant(newcomer.clone(), ChannelRole::Speaker)
            .unwrap();
        assert_eq!((a.role, a.waiting), (ChannelRole::Speaker, false));
        let demoted = a.demoted.expect("an idle speaker yields");
        // Among the two idle speakers the quieter (higher level byte) one goes first.
        assert_eq!(demoted.user_id, quiet.user_id);
        assert_eq!(demoted.role, ChannelRole::Listener);
        assert_eq!(ch.get_role(&quiet.user_id), ChannelRole::Listener);
        assert_eq!(ch.granted_role(&quiet.user_id), Some(ChannelRole::Speaker));
        assert!(ch.is_waiting_to_speak(&quiet.user_id));
        assert!(!ch.can_transmit(&quiet.user_id));
        assert!(ch.can_transmit(&newcomer.user_id));
        assert_eq!(ch.speaker_count(), 3);
        assert_eq!(ch.participant_count(), 4);

        // The demoted member gets a slot back as soon as it tries to speak while another
        // speaker has gone idle (the newcomer is fresh, loud is idle).
        ch.note_speak_attempt(&quiet.user_id);
        speak(&talker, 10);
        speak(&newcomer, 10);
        let changes = ch.rotate_idle_speakers();
        assert_eq!(changes.len(), 2);
        assert_eq!(
            (changes[0].user_id, changes[0].role),
            (loud.user_id, ChannelRole::Listener)
        );
        assert_eq!(
            (changes[1].user_id, changes[1].role),
            (quiet.user_id, ChannelRole::Speaker)
        );
        assert_eq!(ch.speaker_count(), 3);
        // Without a fresh attempt nobody rotates, however idle the holders are.
        std::thread::sleep(Duration::from_millis(60));
        assert!(ch.rotate_idle_speakers().is_empty());
    }

    #[test]
    fn demotion_spares_priority_speakers_moderators_and_administrators() {
        let app = AppId::new();
        let ch = capped(app, 3, SpeakerAdmission::Demote);
        let moderator = session(app, 1);
        let admin = session(app, 2);
        let priority = session(app, 3);
        ch.add_participant(moderator.clone(), ChannelRole::Moderator)
            .unwrap();
        ch.add_participant(admin.clone(), ChannelRole::Administrator)
            .unwrap();
        ch.add_participant_with(
            priority.clone(),
            ChannelRole::Speaker,
            JoinHints {
                priority: true,
                remote_speakers: 0,
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let newcomer = session(app, 4);
        let a = ch
            .add_participant(newcomer.clone(), ChannelRole::Speaker)
            .unwrap();
        assert_eq!(
            (a.role, a.waiting, a.demoted),
            (ChannelRole::Listener, true, None)
        );
        ch.note_speak_attempt(&newcomer.user_id);
        assert!(ch.rotate_idle_speakers().is_empty());
        for s in [&moderator, &admin, &priority] {
            assert!(ch.can_transmit(&s.user_id));
        }
        assert_eq!(ch.speaker_count(), 3);
    }

    #[test]
    fn privileged_joiners_preempt_the_least_active_speaker_at_once() {
        let app = AppId::new();
        let ch = capped(app, 2, SpeakerAdmission::Demote);
        let active = session(app, 1);
        let idle = session(app, 2);
        ch.add_participant(active.clone(), ChannelRole::Speaker)
            .unwrap();
        ch.add_participant(idle.clone(), ChannelRole::Speaker)
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        speak(&active, 10);
        // Neither speaker has been idle for `demote_idle_ms`: a plain joiner waits ...
        let plain = session(app, 3);
        let a = ch
            .add_participant(plain.clone(), ChannelRole::Speaker)
            .unwrap();
        assert!(a.waiting && a.demoted.is_none());
        // ... a moderator takes the less recently active speaker's slot right away ...
        let moderator = session(app, 4);
        let a = ch
            .add_participant(moderator.clone(), ChannelRole::Moderator)
            .unwrap();
        assert_eq!(a.role, ChannelRole::Moderator);
        assert_eq!(a.demoted.map(|d| d.user_id), Some(idle.user_id));
        // ... and so does a priority speaker (the moderator itself is exempt).
        let vip = session(app, 5);
        let a = ch
            .add_participant_with(
                vip.clone(),
                ChannelRole::Speaker,
                JoinHints {
                    priority: true,
                    remote_speakers: 0,
                },
            )
            .unwrap();
        assert_eq!(a.role, ChannelRole::Speaker);
        assert_eq!(a.demoted.map(|d| d.user_id), Some(active.user_id));
        assert_eq!(ch.speaker_count(), 2);
        // Freed slots go to privileged waiters first, then by waiting time.
        ch.remove_participant(&vip.user_id);
        ch.remove_participant(&moderator.user_id);
        let admitted: Vec<UserId> = ch.admit_waiting().into_iter().map(|c| c.user_id).collect();
        assert_eq!(admitted, vec![plain.user_id, idle.user_id]);
        assert_eq!(ch.speaker_count(), 2);
        assert!(ch.is_waiting_to_speak(&active.user_id));
    }

    #[test]
    fn remote_speakers_count_against_the_cap() {
        let app = AppId::new();
        let ch = capped(app, 2, SpeakerAdmission::Wait);
        let far = UserId::new();
        ch.add_remote(far, remote(ChannelRole::Speaker));
        ch.add_remote(UserId::new(), remote(ChannelRole::Listener));
        let a = session(app, 1);
        let b = session(app, 2);
        assert!(
            !ch.add_participant(a.clone(), ChannelRole::Speaker)
                .unwrap()
                .waiting
        );
        assert!(
            ch.add_participant(b.clone(), ChannelRole::Speaker)
                .unwrap()
                .waiting
        );
        assert_eq!(ch.speaker_count(), 2);
        // The remote speaker is demoted on its node: its slot frees here.
        assert!(ch.set_remote_role(&far, ChannelRole::Listener));
        assert_eq!(ch.admit_waiting().len(), 1);
        assert!(ch.can_transmit(&b.user_id));
        // A channel not yet live here counts the speakers the caller learned from the fleet.
        let fresh = capped(app, 1, SpeakerAdmission::Wait);
        let c = session(app, 3);
        let admission = fresh
            .add_participant_with(
                c,
                ChannelRole::Speaker,
                JoinHints {
                    priority: false,
                    remote_speakers: 1,
                },
            )
            .unwrap();
        assert!(admission.waiting);
    }

    #[test]
    fn concurrent_joins_never_exceed_the_speaker_cap() {
        let app = AppId::new();
        let ch = Arc::new(capped(app, 4, SpeakerAdmission::Wait));
        let handles: Vec<_> = (1..=32u32)
            .map(|i| {
                let ch = ch.clone();
                std::thread::spawn(move || {
                    ch.add_participant(session(app, i), ChannelRole::Speaker)
                        .unwrap()
                        .waiting
                })
            })
            .collect();
        let waiting = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|w| *w)
            .count();
        assert_eq!(waiting, 28);
        assert_eq!(ch.speaker_count(), 4);
        assert_eq!(ch.participant_count(), 32);
    }

    #[test]
    fn raising_the_cap_admits_waiting_members_and_hidden_channels_scope_observers() {
        let app = AppId::new();
        let ch = MediaChannel::new(
            ChannelId::new(),
            app,
            ChannelConfig {
                audience: Some(AudienceConfig {
                    hide_listeners: true,
                    max_speakers: 1,
                    speaker_admission: SpeakerAdmission::Wait,
                    ..AudienceConfig::default()
                }),
                ..ChannelConfig::default()
            },
        );
        let holder = session(app, 1);
        let waiting = session(app, 2);
        ch.add_participant(holder.clone(), ChannelRole::Speaker)
            .unwrap();
        ch.add_participant(waiting.clone(), ChannelRole::Speaker)
            .unwrap();
        // A waiting speaker is a listener for presence purposes too.
        assert!(ch.is_hidden_listener(&waiting.user_id));
        let mut cfg = ch.config.read().clone();
        cfg.audience = Some(AudienceConfig {
            max_speakers: 2,
            ..cfg.audience.unwrap()
        });
        ch.update_config(cfg);
        let admitted = ch.admit_waiting();
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].user_id, waiting.user_id);
        assert!(!ch.is_hidden_listener(&waiting.user_id));
        assert_eq!(ch.speaker_count(), 2);
    }
}
