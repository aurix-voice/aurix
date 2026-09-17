//! Media tap interface: lets the SFU hand authenticated audio to consumers such as
//! the recording subsystem without a dependency on the media crate.

use crate::types::{ChannelId, UserId};

/// Receives every routed Opus packet for a channel (post-authentication, pre-forwarding).
pub trait AudioSink: Send + Sync {
    /// Returns `true` if the sink currently wants audio for `channel_id` (cheap check).
    fn wants_channel(&self, channel_id: &ChannelId) -> bool;

    /// `rtp_timestamp` is in 48 kHz units; `payload` is a raw Opus packet.
    fn on_audio(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        ssrc: u32,
        rtp_timestamp: u32,
        payload: &[u8],
    );

    /// Called when a participant leaves so per-user state can be released.
    fn on_participant_left(&self, channel_id: ChannelId, user_id: UserId);
}
