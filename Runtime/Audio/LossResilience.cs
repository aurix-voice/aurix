using System;

namespace Aurix.Audio
{
    /// <summary>
    /// Redundancy tier of the uplink encoder (mirrors the native core's <c>LossProfile</c>): how much
    /// in-band FEC, expected loss and Deep REDundancy the encoder spends, chosen from the loss the
    /// <b>server</b> measures on our packets or the worst of our receivers sees on its downlink,
    /// whichever is higher (<see cref="NetworkQuality.ProtectLossPercent"/>).
    /// </summary>
    public enum LossProfile
    {
        /// <summary>Clean link: the baseline settings (channel policy / app) as they are.</summary>
        Low = 0,
        /// <summary>Some loss: FEC on, tuned for at least <see cref="LossProfilePolicy.ModerateExpectedLoss"/> (or the measured loss).</summary>
        Moderate = 1,
        /// <summary>
        /// Heavy or bursty loss: FEC tuned for at least <see cref="LossProfilePolicy.HighExpectedLoss"/>, DRED covering
        /// <see cref="LossProfilePolicy.HighDredDurationMs"/> of history and a bitrate floor so DRED gets bits
        /// (never above the channel's target).
        /// </summary>
        High = 2,
    }

    /// <summary>Thresholds and timing of the automatic profile choice, in uplink loss percent.</summary>
    public struct LossProfilePolicy
    {
        /// <summary>Expected-loss floor the <see cref="LossProfile.Moderate"/> profile tunes in-band FEC for, percent.</summary>
        public const int ModerateExpectedLoss = 10;
        /// <summary>Expected-loss floor the <see cref="LossProfile.High"/> profile tunes in-band FEC for, percent.</summary>
        public const int HighExpectedLoss = 20;
        /// <summary>
        /// DRED history the <see cref="LossProfile.High"/> profile asks the encoder for. libopus codes as much of it as
        /// the bitrate affords (about 100–150 ms at 28–40 kbit/s); bursts within what a packet carries are rebuilt,
        /// older frames concealed.
        /// </summary>
        public const int HighDredDurationMs = 400;
        /// <summary>Below this bitrate libopus codes no DRED, so the <see cref="LossProfile.High"/> profile lifts the bitrate to it when the channel allows.</summary>
        public const int DredMinBitrateBps = 28000;

        /// <summary><c>Low → Moderate</c> at or above this loss.</summary>
        public float ModerateEnterPercent;
        /// <summary><c>Moderate → Low</c> below this loss (after <see cref="MinDwell"/>).</summary>
        public float ModerateExitPercent;
        /// <summary><c>Moderate → High</c> at or above this loss.</summary>
        public float HighEnterPercent;
        /// <summary><c>High → Moderate</c> below this loss (after <see cref="MinDwell"/>).</summary>
        public float HighExitPercent;
        /// <summary>A profile is not relaxed before it has held this long; escalation is immediate.</summary>
        public TimeSpan MinDwell;

        public static LossProfilePolicy Default => new LossProfilePolicy
        {
            ModerateEnterPercent = 3f,
            ModerateExitPercent = 1f,
            HighEnterPercent = 10f,
            HighExitPercent = 5f,
            MinDwell = TimeSpan.FromSeconds(6),
        };

        /// <summary>Next profile for <paramref name="lossPercent"/> given the current one and how long it has held.</summary>
        public LossProfile Next(LossProfile current, float lossPercent, TimeSpan held)
        {
            bool mayRelax = held >= MinDwell;
            switch (current)
            {
                case LossProfile.Low:
                    if (lossPercent >= HighEnterPercent) return LossProfile.High;
                    if (lossPercent >= ModerateEnterPercent) return LossProfile.Moderate;
                    return LossProfile.Low;
                case LossProfile.Moderate:
                    if (lossPercent >= HighEnterPercent) return LossProfile.High;
                    if (mayRelax && lossPercent < ModerateExitPercent) return LossProfile.Low;
                    return LossProfile.Moderate;
                default:
                    if (mayRelax && lossPercent < HighExitPercent) return LossProfile.Moderate;
                    return LossProfile.High;
            }
        }

        /// <summary>
        /// Encoder settings for <paramref name="profile"/> laid over <paramref name="baseline"/>.
        /// <paramref name="targetBitrateBps"/> is the channel's target (the ceiling for the DRED bitrate floor;
        /// null: the baseline's bitrate is the ceiling).
        /// </summary>
        public static OpusEncoderSettings Shape(LossProfile profile, OpusEncoderSettings baseline, float uplinkLossPercent, int? targetBitrateBps)
        {
            int measured = (int)Math.Round(Math.Clamp(float.IsFinite(uplinkLossPercent) ? uplinkLossPercent : 0f, 0f, 100f));
            var s = baseline;
            switch (profile)
            {
                case LossProfile.Moderate:
                    s.Fec = true;
                    s.ExpectedLossPercent = Math.Max(Math.Max(s.ExpectedLossPercent, ModerateExpectedLoss), measured);
                    break;
                case LossProfile.High:
                    s.Fec = true;
                    s.ExpectedLossPercent = Math.Max(Math.Max(s.ExpectedLossPercent, HighExpectedLoss), measured);
                    s.DredDurationMs = Math.Max(s.DredDurationMs, HighDredDurationMs);
                    int ceiling = Math.Max(targetBitrateBps ?? baseline.BitrateBps, baseline.BitrateBps);
                    s.BitrateBps = Math.Max(s.BitrateBps, Math.Min(DredMinBitrateBps, ceiling));
                    break;
            }
            return s.Clamped();
        }
    }

    /// <summary>
    /// Tracks the uplink loss profile from the server's loss reports (or a pinned tier) and shapes encoder
    /// settings with it. Escalation is immediate, relaxation waits out the policy's dwell time and a lower
    /// threshold so the profile does not flap on a noisy link. Not thread-safe; callers serialise access.
    /// </summary>
    public sealed class LossController
    {
        private readonly LossProfilePolicy _policy;
        private LossProfile? _pinned;
        private LossProfile _profile;
        private DateTime? _changedAt;

        public LossController() : this(null, LossProfilePolicy.Default) { }

        /// <param name="pinned">A fixed tier, or null for automatic choice from the server's reports.</param>
        public LossController(LossProfile? pinned, LossProfilePolicy policy)
        {
            _policy = policy;
            _pinned = pinned;
            _profile = pinned ?? LossProfile.Low;
        }

        public LossProfile Profile => _profile;
        /// <summary>The pinned tier, or null when the profile follows the server's reports.</summary>
        public LossProfile? Pinned => _pinned;
        public LossProfilePolicy Policy => _policy;
        /// <summary>Last uplink loss the server reported, percent.</summary>
        public float UplinkLossPercent { get; private set; }

        /// <summary>Feed one server report. True when the profile changed.</summary>
        public bool Observe(float uplinkLossPercent, DateTime now)
        {
            UplinkLossPercent = float.IsFinite(uplinkLossPercent) ? Math.Clamp(uplinkLossPercent, 0f, 100f) : 0f;
            if (_pinned.HasValue) return false;
            var held = _changedAt.HasValue ? now - _changedAt.Value : TimeSpan.MaxValue;
            if (held < TimeSpan.Zero) held = TimeSpan.Zero;
            return Switch(_policy.Next(_profile, UplinkLossPercent, held), now);
        }

        /// <summary>Pin a profile or (null) return to automatic choice, re-evaluating the last report at once. True when it changed.</summary>
        public bool SetPinned(LossProfile? pinned, DateTime now)
        {
            _pinned = pinned;
            return Switch(pinned ?? _policy.Next(LossProfile.Low, UplinkLossPercent, TimeSpan.MaxValue), now);
        }

        /// <summary>Forget the link history (new session): back to <see cref="LossProfile.Low"/> in auto mode.</summary>
        public void Reset()
        {
            UplinkLossPercent = 0f;
            _changedAt = null;
            if (!_pinned.HasValue) _profile = LossProfile.Low;
        }

        private bool Switch(LossProfile next, DateTime now)
        {
            if (next == _profile) return false;
            _profile = next;
            _changedAt = now;
            return true;
        }

        /// <summary>The current profile laid over <paramref name="baseline"/>; see <see cref="LossProfilePolicy.Shape"/>.</summary>
        public OpusEncoderSettings Shape(OpusEncoderSettings baseline, int? targetBitrateBps) =>
            LossProfilePolicy.Shape(_profile, baseline, UplinkLossPercent, targetBitrateBps);
    }
}
