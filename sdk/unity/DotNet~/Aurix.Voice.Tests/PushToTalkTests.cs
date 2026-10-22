using Aurix.Audio;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>Push-to-talk state machine shared by the Unity behaviour: hold, toggle, release delay.</summary>
    public class PushToTalkTests
    {
        [Fact]
        public void Hold_starts_on_press_and_stops_after_release_delay()
        {
            var ptt = new PushToTalkState(toggle: false, releaseDelay: 0.15f);
            Assert.True(ptt.Update(pressed: true, now: 0f));
            Assert.Null(ptt.Update(true, 0.1f));
            Assert.Null(ptt.Update(false, 0.2f));
            Assert.True(ptt.Transmitting);
            Assert.Null(ptt.Update(false, 0.3f));
            Assert.False(ptt.Update(false, 0.4f));
            Assert.False(ptt.Transmitting);
            Assert.Null(ptt.Update(false, 1f));
        }

        [Fact]
        public void Pressing_again_inside_the_delay_keeps_transmitting_without_a_stop()
        {
            var ptt = new PushToTalkState(false, 0.2f);
            Assert.True(ptt.Update(true, 0f));
            Assert.Null(ptt.Update(false, 0.1f));
            Assert.Null(ptt.Update(true, 0.2f));
            Assert.Null(ptt.Update(true, 1f));
            Assert.True(ptt.Transmitting);
            Assert.Null(ptt.Update(false, 1.1f));
            Assert.False(ptt.Update(false, 1.31f), "release timer restarts from the latest release");
        }

        [Fact]
        public void Zero_release_delay_stops_immediately()
        {
            var ptt = new PushToTalkState(false, 0f);
            Assert.True(ptt.Update(true, 0f));
            Assert.False(ptt.Update(false, 0.016f));
        }

        [Fact]
        public void Toggle_flips_on_each_press_edge_and_ignores_held_state()
        {
            var ptt = new PushToTalkState(toggle: true, releaseDelay: 0.15f);
            Assert.True(ptt.Update(pressed: true, now: 0f));
            Assert.Null(ptt.Update(false, 0.1f));
            Assert.Null(ptt.Update(false, 5f));
            Assert.True(ptt.Transmitting);
            Assert.Null(ptt.Update(true, 6f));
            Assert.True(ptt.Transmitting, "toggle-off also honours the release delay");
            Assert.False(ptt.Update(false, 6.2f));
            Assert.False(ptt.Transmitting);
            Assert.Null(ptt.Update(false, 7f));
        }

        [Fact]
        public void Reset_closes_a_pending_release_and_reports_whether_it_was_open()
        {
            var ptt = new PushToTalkState(false, 1f);
            Assert.False(ptt.Reset());
            ptt.Update(true, 0f);
            ptt.Update(false, 0.1f);
            Assert.True(ptt.Reset());
            Assert.False(ptt.Transmitting);
            Assert.Null(ptt.Update(false, 2f));
        }

        [Fact]
        public void Switching_mode_at_runtime_takes_effect_next_update()
        {
            var ptt = new PushToTalkState(false, 0f);
            Assert.True(ptt.Update(true, 0f));
            Assert.False(ptt.Update(false, 0.1f));
            ptt.Toggle = true;
            Assert.True(ptt.Update(true, 0.2f));
            Assert.Null(ptt.Update(false, 0.3f));
            Assert.True(ptt.Transmitting);
        }
    }
}
