//! Adaptive loss resilience of the uplink: how much redundancy (in-band FEC, expected loss,
//! Deep REDundancy) the encoder spends, chosen from the loss the **server** measures on our
//! packets and reported back in `NetworkQuality`. Escalation is immediate, relaxation waits
//! out a dwell time and a lower threshold so the profile does not flap on a noisy link.

use serde::Serialize;
use std::time::{Duration, Instant};

use crate::audio::EncoderSettings;

/// Redundancy tier of the uplink encoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LossProfile {
    /// Clean link: the baseline settings (channel policy / app) as they are.
    #[default]
    Low,
    /// Some loss: in-band FEC on, tuned for at least [`LossProfilePolicy::MODERATE_EXPECTED_LOSS`]
    /// (or the measured loss, whichever is higher).
    Moderate,
    /// Heavy or bursty loss: FEC tuned for at least [`LossProfilePolicy::HIGH_EXPECTED_LOSS`],
    /// DRED covering [`LossProfilePolicy::HIGH_DRED_DURATION_MS`] of history, and a bitrate
    /// floor so DRED gets bits (never above the channel's target).
    High,
}

impl LossProfile {
    pub const ALL: [Self; 3] = [Self::Low, Self::Moderate, Self::High];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::High => "high",
        }
    }
}

/// How the profile is chosen: from the server's uplink-loss reports, or pinned by the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossAdaptation {
    Auto,
    Fixed(LossProfile),
}

/// Thresholds and timing of the automatic profile choice, in uplink loss percent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LossProfilePolicy {
    /// `Low → Moderate` at or above this loss.
    pub moderate_enter_percent: f32,
    /// `Moderate → Low` below this loss (after `min_dwell`).
    pub moderate_exit_percent: f32,
    /// `Moderate → High` at or above this loss.
    pub high_enter_percent: f32,
    /// `High → Moderate` below this loss (after `min_dwell`).
    pub high_exit_percent: f32,
    /// A profile is not relaxed before it has held this long; escalation is immediate.
    pub min_dwell: Duration,
}

impl LossProfilePolicy {
    /// Expected-loss floor the `Moderate` profile tunes in-band FEC for, percent.
    pub const MODERATE_EXPECTED_LOSS: u8 = 10;
    /// Expected-loss floor the `High` profile tunes in-band FEC for, percent.
    pub const HIGH_EXPECTED_LOSS: u8 = 20;
    /// DRED history the `High` profile asks the encoder for. libopus codes as much of it
    /// as the bitrate affords (about 100-150 ms at 28-40 kbit/s, the full span only well
    /// above); bursts within what a packet carries are rebuilt, older frames concealed.
    pub const HIGH_DRED_DURATION_MS: u16 = 400;
    /// libopus gives DRED a share of `bitrate - 20 kbit/s` (with FEC on); below this it
    /// codes none, so the `High` profile lifts the bitrate to it when the channel allows.
    pub const DRED_MIN_BITRATE_BPS: u32 = 28_000;

    /// Next profile for `loss_percent` given the current one and how long it has held.
    pub fn next(&self, current: LossProfile, loss_percent: f32, held: Duration) -> LossProfile {
        let may_relax = held >= self.min_dwell;
        match current {
            LossProfile::Low if loss_percent >= self.high_enter_percent => LossProfile::High,
            LossProfile::Low if loss_percent >= self.moderate_enter_percent => {
                LossProfile::Moderate
            }
            LossProfile::Low => LossProfile::Low,
            LossProfile::Moderate if loss_percent >= self.high_enter_percent => LossProfile::High,
            LossProfile::Moderate if may_relax && loss_percent < self.moderate_exit_percent => {
                LossProfile::Low
            }
            LossProfile::Moderate => LossProfile::Moderate,
            LossProfile::High if may_relax && loss_percent < self.high_exit_percent => {
                LossProfile::Moderate
            }
            LossProfile::High => LossProfile::High,
        }
    }
}

impl Default for LossProfilePolicy {
    fn default() -> Self {
        Self {
            moderate_enter_percent: 3.0,
            moderate_exit_percent: 1.0,
            high_enter_percent: 10.0,
            high_exit_percent: 5.0,
            min_dwell: Duration::from_secs(6),
        }
    }
}

/// Tracks the uplink loss profile and shapes encoder settings with it.
#[derive(Debug, Clone, PartialEq)]
pub struct LossController {
    policy: LossProfilePolicy,
    adaptation: LossAdaptation,
    profile: LossProfile,
    changed_at: Option<Instant>,
    /// Last uplink loss the server reported, percent.
    uplink_loss_percent: f32,
}

impl LossController {
    pub fn new(adaptation: LossAdaptation, policy: LossProfilePolicy) -> Self {
        Self {
            policy,
            adaptation,
            profile: match adaptation {
                LossAdaptation::Auto => LossProfile::Low,
                LossAdaptation::Fixed(p) => p,
            },
            changed_at: None,
            uplink_loss_percent: 0.0,
        }
    }

    pub fn profile(&self) -> LossProfile {
        self.profile
    }

    pub fn adaptation(&self) -> LossAdaptation {
        self.adaptation
    }

    pub fn policy(&self) -> LossProfilePolicy {
        self.policy
    }

    pub fn uplink_loss_percent(&self) -> f32 {
        self.uplink_loss_percent
    }

    /// Feed one server report. `Some(profile)` when the profile changed.
    pub fn observe(&mut self, uplink_loss_percent: f32, now: Instant) -> Option<LossProfile> {
        let loss = if uplink_loss_percent.is_finite() {
            uplink_loss_percent.clamp(0.0, 100.0)
        } else {
            0.0
        };
        self.uplink_loss_percent = loss;
        let LossAdaptation::Auto = self.adaptation else {
            return None;
        };
        let held = self
            .changed_at
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(Duration::MAX);
        let next = self.policy.next(self.profile, loss, held);
        self.switch(next, now)
    }

    /// Pin a profile or return to automatic choice. `Some(profile)` when it changed.
    pub fn set_adaptation(
        &mut self,
        adaptation: LossAdaptation,
        now: Instant,
    ) -> Option<LossProfile> {
        self.adaptation = adaptation;
        match adaptation {
            LossAdaptation::Fixed(p) => self.switch(p, now),
            LossAdaptation::Auto => {
                let next =
                    self.policy
                        .next(LossProfile::Low, self.uplink_loss_percent, Duration::MAX);
                self.switch(next, now)
            }
        }
    }

    /// Forget the link history (new session): back to `Low` in auto mode.
    pub fn reset(&mut self) {
        self.uplink_loss_percent = 0.0;
        self.changed_at = None;
        if self.adaptation == LossAdaptation::Auto {
            self.profile = LossProfile::Low;
        }
    }

    fn switch(&mut self, next: LossProfile, now: Instant) -> Option<LossProfile> {
        if next == self.profile {
            return None;
        }
        self.profile = next;
        self.changed_at = Some(now);
        Some(next)
    }

    /// The current profile laid over `base` (the policy / app / server-commanded settings).
    /// `target_bitrate_bps` is the channel's target, the ceiling for the DRED bitrate floor
    /// (`None`: `base.bitrate_bps` is the ceiling).
    pub fn shape(&self, base: EncoderSettings, target_bitrate_bps: Option<u32>) -> EncoderSettings {
        shape_for_profile(
            self.profile,
            base,
            self.uplink_loss_percent,
            target_bitrate_bps,
        )
    }
}

impl Default for LossController {
    fn default() -> Self {
        Self::new(LossAdaptation::Auto, LossProfilePolicy::default())
    }
}

/// Encoder settings for `profile` over `base`; see [`LossController::shape`].
pub fn shape_for_profile(
    profile: LossProfile,
    base: EncoderSettings,
    uplink_loss_percent: f32,
    target_bitrate_bps: Option<u32>,
) -> EncoderSettings {
    let measured = uplink_loss_percent.clamp(0.0, 100.0).round() as u8;
    let mut s = base;
    match profile {
        LossProfile::Low => {}
        LossProfile::Moderate => {
            s.fec = true;
            s.expected_loss_percent = s
                .expected_loss_percent
                .max(LossProfilePolicy::MODERATE_EXPECTED_LOSS)
                .max(measured);
        }
        LossProfile::High => {
            s.fec = true;
            s.expected_loss_percent = s
                .expected_loss_percent
                .max(LossProfilePolicy::HIGH_EXPECTED_LOSS)
                .max(measured);
            s.dred_duration_ms = s
                .dred_duration_ms
                .max(LossProfilePolicy::HIGH_DRED_DURATION_MS);
            let ceiling = target_bitrate_bps
                .unwrap_or(base.bitrate_bps)
                .max(base.bitrate_bps);
            s.bitrate_bps = s
                .bitrate_bps
                .max(LossProfilePolicy::DRED_MIN_BITRATE_BPS.min(ceiling));
        }
    }
    s.clamped()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_secs(secs)
    }

    #[test]
    fn escalates_immediately_and_relaxes_with_hysteresis_and_dwell() {
        let mut c = LossController::default();
        assert_eq!(c.profile(), LossProfile::Low);
        assert_eq!(c.observe(2.9, at(0)), None);
        assert_eq!(c.observe(3.0, at(2)), Some(LossProfile::Moderate));
        // Below the entry threshold but above the exit one: stays.
        assert_eq!(c.observe(2.0, at(4)), None);
        // Below the exit threshold but within the dwell: stays.
        assert_eq!(c.observe(0.5, at(6)), None);
        assert_eq!(c.observe(0.5, at(8)), Some(LossProfile::Low));
        // Straight to High from Low on a burst.
        assert_eq!(c.observe(25.0, at(10)), Some(LossProfile::High));
        assert_eq!(c.observe(6.0, at(20)), None);
        assert_eq!(c.observe(4.0, at(22)), Some(LossProfile::Moderate));
        assert_eq!(c.observe(12.0, at(23)), Some(LossProfile::High));
        assert_eq!(c.uplink_loss_percent(), 12.0);
    }

    #[test]
    fn fixed_profile_ignores_reports_and_auto_resumes_from_the_last_report() {
        let mut c = LossController::default();
        assert_eq!(
            c.set_adaptation(LossAdaptation::Fixed(LossProfile::High), at(0)),
            Some(LossProfile::High)
        );
        assert_eq!(c.observe(0.0, at(1)), None);
        assert_eq!(c.profile(), LossProfile::High);
        assert_eq!(
            c.set_adaptation(LossAdaptation::Auto, at(2)),
            Some(LossProfile::Low)
        );
        c.observe(15.0, at(3));
        assert_eq!(
            c.set_adaptation(LossAdaptation::Fixed(LossProfile::Low), at(4)),
            Some(LossProfile::Low)
        );
        // Back to auto: re-evaluates the last report at once.
        assert_eq!(
            c.set_adaptation(LossAdaptation::Auto, at(5)),
            Some(LossProfile::High)
        );
        c.reset();
        assert_eq!(c.profile(), LossProfile::Low);
        assert_eq!(c.uplink_loss_percent(), 0.0);
    }

    #[test]
    fn nan_and_out_of_range_reports_are_sanitised() {
        let mut c = LossController::default();
        assert_eq!(c.observe(f32::NAN, at(0)), None);
        assert_eq!(c.uplink_loss_percent(), 0.0);
        assert_eq!(c.observe(250.0, at(1)), Some(LossProfile::High));
        assert_eq!(c.uplink_loss_percent(), 100.0);
    }

    #[test]
    fn profiles_shape_fec_expected_loss_dred_and_bitrate_floor() {
        let base = EncoderSettings {
            bitrate_bps: 16_000,
            fec: false,
            expected_loss_percent: 2,
            dred_duration_ms: 0,
            ..EncoderSettings::default()
        };
        let low = shape_for_profile(LossProfile::Low, base, 0.0, Some(32_000));
        assert_eq!(low, base);

        let moderate = shape_for_profile(LossProfile::Moderate, base, 4.4, Some(32_000));
        assert!(moderate.fec);
        assert_eq!(moderate.expected_loss_percent, 10);
        assert_eq!(moderate.dred_duration_ms, 0);
        assert_eq!(moderate.bitrate_bps, 16_000);
        // The measured loss wins over the floor.
        assert_eq!(
            shape_for_profile(LossProfile::Moderate, base, 14.6, None).expected_loss_percent,
            15
        );

        let high = shape_for_profile(LossProfile::High, base, 30.0, Some(32_000));
        assert!(high.fec);
        assert_eq!(high.expected_loss_percent, 30);
        assert_eq!(high.dred_duration_ms, 400);
        assert_eq!(high.bitrate_bps, 28_000, "lifted to the DRED floor");
        // The channel's target caps the floor…
        assert_eq!(
            shape_for_profile(LossProfile::High, base, 30.0, Some(24_000)).bitrate_bps,
            24_000
        );
        // …unless the base is already above it.
        let rich = EncoderSettings {
            bitrate_bps: 64_000,
            dred_duration_ms: 1_000,
            ..base
        };
        let shaped = shape_for_profile(LossProfile::High, rich, 30.0, Some(24_000));
        assert_eq!(shaped.bitrate_bps, 64_000);
        assert_eq!(
            shaped.dred_duration_ms, 1_000,
            "a longer app choice is kept"
        );
        // No target known: the base bitrate is the ceiling (no lift).
        assert_eq!(
            shape_for_profile(LossProfile::High, base, 30.0, None).bitrate_bps,
            16_000
        );
    }
}
