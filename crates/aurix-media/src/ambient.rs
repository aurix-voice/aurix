//! Cocktail-party mixing (`ChannelConfig::ambient`): per receiver and channel, the speakers
//! heard right now are ranked by how loud they would arrive (delivery gain after distance,
//! volume and focus × the level the sender reported for the frame, RFC 6464); the loudest
//! `max_voices` keep their full gain, the rest are attenuated to `ambient_gain`.
//!
//! State is a small table of currently active speakers per (receiver, channel). It is
//! updated on every frame routed to the receiver — the hot path — so the table stays tiny
//! (only speakers with a frame in the last [`VOICE_HOLD`]) and the ranking is a partial sort.

use aurix_common::types::{AmbientConfig, ChannelId, UserId};
use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// A speaker without a frame for this long is no longer competing for a slot (covers DTX
/// gaps and jitter; a real pause hands the slot over).
pub const VOICE_HOLD: Duration = Duration::from_millis(400);
/// A challenger must be this much louder than a slot holder to take the slot over.
const STICKINESS: f32 = 1.2;

#[derive(Debug, Clone, Copy)]
struct Voice {
    volume: f32,
    last_seen: Instant,
    focused: bool,
}

/// `K` identifies a competing voice (the sender's user, for both the cocktail-party mix and
/// the per-receiver stream cap of `ChannelConfig::audience.max_streams`).
#[derive(Debug)]
pub struct AmbientState<K = UserId> {
    channels: HashMap<ChannelId, HashMap<K, Voice>>,
}

impl<K> Default for AmbientState<K> {
    fn default() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }
}

impl<K: Copy + Eq + Hash + Ord> AmbientState<K> {
    /// Records a frame from `sender` at `volume` for `channel` and returns the multiplier to
    /// apply: `1.0` for a focused speaker, `cfg.ambient_gain` for an ambient one.
    pub fn gate(
        &mut self,
        channel: &ChannelId,
        sender: &K,
        volume: f32,
        cfg: &AmbientConfig,
        now: Instant,
    ) -> f32 {
        let voices = self.channels.entry(*channel).or_default();
        voices.retain(|_, v| now.duration_since(v.last_seen) < VOICE_HOLD);
        let entry = voices.entry(*sender).or_insert(Voice {
            volume,
            last_seen: now,
            focused: false,
        });
        entry.volume = volume;
        entry.last_seen = now;
        let max = usize::from(cfg.max_voices.max(1));
        if voices.len() <= max {
            for v in voices.values_mut() {
                v.focused = true;
            }
            return 1.0;
        }
        // Holders compete with a bonus so equally loud voices do not swap slots every frame.
        let mut ranked: Vec<(f32, K)> = voices
            .iter()
            .map(|(uid, v)| {
                let weight = if v.focused {
                    v.volume * STICKINESS
                } else {
                    v.volume
                };
                (weight, *uid)
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        for (rank, (_, uid)) in ranked.iter().enumerate() {
            if let Some(v) = voices.get_mut(uid) {
                v.focused = rank < max;
            }
        }
        if voices.get(sender).is_some_and(|v| v.focused) {
            1.0
        } else {
            cfg.ambient_gain
        }
    }

    /// Drops the state of a channel the receiver left.
    pub fn forget_channel(&mut self, channel: &ChannelId) {
        self.channels.remove(channel);
    }

    /// Drops a speaker who left (so their slot frees immediately).
    pub fn forget_sender(&mut self, channel: &ChannelId, sender: &K) {
        if let Some(voices) = self.channels.get_mut(channel) {
            voices.remove(sender);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_voices: u8) -> AmbientConfig {
        AmbientConfig {
            max_voices,
            ambient_gain: 0.1,
        }
    }

    #[test]
    fn up_to_max_voices_pass_untouched() {
        let mut st = AmbientState::default();
        let ch = ChannelId::new();
        let now = Instant::now();
        let a = UserId::new();
        let b = UserId::new();
        assert_eq!(st.gate(&ch, &a, 1.0, &cfg(2), now), 1.0);
        assert_eq!(st.gate(&ch, &b, 0.3, &cfg(2), now), 1.0);
    }

    #[test]
    fn quieter_extra_speakers_become_ambient_and_slots_are_sticky() {
        let mut st = AmbientState::default();
        let ch = ChannelId::new();
        let c = cfg(2);
        let now = Instant::now();
        let loud = UserId::new();
        let mid = UserId::new();
        let quiet = UserId::new();
        st.gate(&ch, &loud, 1.0, &c, now);
        st.gate(&ch, &mid, 0.6, &c, now);
        // Third voice, quieter than both holders: ambient.
        assert_eq!(st.gate(&ch, &quiet, 0.4, &c, now), 0.1);
        // Holders keep their slots.
        assert_eq!(st.gate(&ch, &loud, 1.0, &c, now), 1.0);
        assert_eq!(st.gate(&ch, &mid, 0.6, &c, now), 1.0);
        // Slightly louder than a holder is not enough (stickiness)…
        assert_eq!(st.gate(&ch, &quiet, 0.65, &c, now), 0.1);
        // …clearly louder takes the slot over, and the displaced holder becomes ambient.
        assert_eq!(st.gate(&ch, &quiet, 0.9, &c, now), 1.0);
        assert_eq!(st.gate(&ch, &mid, 0.6, &c, now), 0.1);
    }

    #[test]
    fn silent_speakers_free_their_slot() {
        let mut st = AmbientState::default();
        let ch = ChannelId::new();
        let c = cfg(1);
        let t0 = Instant::now();
        let a = UserId::new();
        let b = UserId::new();
        assert_eq!(st.gate(&ch, &a, 1.0, &c, t0), 1.0);
        assert_eq!(st.gate(&ch, &b, 1.0, &c, t0), 0.1);
        let later = t0 + VOICE_HOLD + Duration::from_millis(1);
        assert_eq!(st.gate(&ch, &b, 1.0, &c, later), 1.0);
    }

    #[test]
    fn equal_volumes_keep_first_speakers() {
        let mut st = AmbientState::default();
        let ch = ChannelId::new();
        let c = cfg(2);
        let now = Instant::now();
        let first = UserId::new();
        let second = UserId::new();
        let third = UserId::new();
        st.gate(&ch, &first, 1.0, &c, now);
        st.gate(&ch, &second, 1.0, &c, now);
        for _ in 0..5 {
            assert_eq!(st.gate(&ch, &third, 1.0, &c, now), 0.1);
            assert_eq!(st.gate(&ch, &first, 1.0, &c, now), 1.0);
            assert_eq!(st.gate(&ch, &second, 1.0, &c, now), 1.0);
        }
        st.forget_sender(&ch, &first);
        assert_eq!(st.gate(&ch, &third, 1.0, &c, now), 1.0);
    }
}
