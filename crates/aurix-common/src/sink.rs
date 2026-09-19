//! Media tap interface: lets the SFU hand authenticated audio to consumers such as
//! the recording subsystem without a dependency on the media crate.

use crate::types::{AppId, ChannelId, UserId};
use crate::Result;
use async_trait::async_trait;

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

/// Stored media keyed to a user (recording files/objects and their rows). Called by user
/// erasure before the database rows about the user are removed.
#[async_trait]
pub trait UserMediaPurger: Send + Sync {
    /// Stops the user's live recordings on this node and removes every stored one.
    /// Returns the number of recordings removed.
    async fn purge_user_media(&self, app_id: AppId, user_id: UserId) -> Result<u64>;
}
