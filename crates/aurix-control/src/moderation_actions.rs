//! Kick / server-mute primitives shared by the REST API (API-key authenticated) and the
//! WebSocket control plane (action-token authenticated). Both paths persist state, update
//! the local SFU, write the audit log and publish the cluster event that every node
//! (including this one) turns into client notifications.

use crate::event_bus::ServerEvent;
use crate::plane::ControlPlane;
use aurix_common::error::Result;
use aurix_common::types::*;
use aurix_media::SfuNode;
use parking_lot::RwLock;
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

pub struct ModerationTarget {
    pub app_id: AppId,
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub actor: UserId,
    pub ip: Option<String>,
}

/// Scope of a channel-wide moderation act.
pub struct BulkTarget {
    pub app_id: AppId,
    pub channel_id: ChannelId,
    pub actor: UserId,
    pub ip: Option<String>,
    /// Users left untouched (e.g. the game master or the moderator itself).
    pub except: Vec<UserId>,
}

/// Result of a channel-wide act: who was affected, who was skipped because of `except`, and
/// per-user errors (the act continues past failures so one bad row cannot shield the rest).
#[derive(Debug, Default, serde::Serialize)]
pub struct BulkOutcome {
    pub affected: Vec<UserId>,
    pub skipped: Vec<UserId>,
    pub failed: Vec<BulkFailure>,
}

#[derive(Debug, serde::Serialize)]
pub struct BulkFailure {
    pub user_id: UserId,
    pub error: String,
}

/// Distinct users with an open membership in the channel, split by the exclusion list.
async fn channel_users(
    control: &ControlPlane,
    t: &BulkTarget,
) -> Result<(Vec<UserId>, Vec<UserId>)> {
    let members = control
        .sessions
        .get_channel_members(t.app_id, t.channel_id)
        .await?;
    let users: BTreeSet<UserId> = members.iter().map(|m| UserId(m.user_id)).collect();
    let (skipped, targets): (Vec<UserId>, Vec<UserId>) =
        users.into_iter().partition(|u| t.except.contains(u));
    Ok((targets, skipped))
}

/// Kicks every current participant of the channel (minus `except`). Each user goes through
/// `kick_from_channel`, so per-user audit rows and `user.kicked` events are emitted as for a
/// single kick; one extra audit row summarises the act.
pub async fn kick_all(
    control: &ControlPlane,
    sfu: &RwLock<SfuNode>,
    t: BulkTarget,
    reason: String,
) -> Result<BulkOutcome> {
    let (targets, skipped) = channel_users(control, &t).await?;
    let mut out = BulkOutcome {
        skipped,
        ..Default::default()
    };
    for user_id in targets {
        let r = kick_from_channel(
            control,
            sfu,
            ModerationTarget {
                app_id: t.app_id,
                channel_id: t.channel_id,
                user_id,
                actor: t.actor,
                ip: t.ip.clone(),
            },
            reason.clone(),
        )
        .await;
        match r {
            Ok(_) => out.affected.push(user_id),
            Err(e) => out.failed.push(BulkFailure {
                user_id,
                error: e.public_message(),
            }),
        }
    }
    control.audit.log(
        Some(t.app_id),
        t.actor,
        AuditAction::ChannelKickAll,
        "channel",
        &t.channel_id.to_string(),
        serde_json::json!({
            "reason": reason,
            "affected": out.affected,
            "skipped": out.skipped,
            "failed": out.failed.len(),
        }),
        t.ip,
    );
    Ok(out)
}

/// Server-mutes (or unmutes) every current participant of the channel (minus `except`).
/// Participants joining later are not affected; the game server re-issues the call or mutes
/// them individually.
pub async fn set_server_mute_all(
    control: &ControlPlane,
    sfu: &RwLock<SfuNode>,
    t: BulkTarget,
    muted: bool,
) -> Result<BulkOutcome> {
    let (targets, skipped) = channel_users(control, &t).await?;
    let mut out = BulkOutcome {
        skipped,
        ..Default::default()
    };
    for user_id in targets {
        let r = set_server_mute(
            control,
            sfu,
            ModerationTarget {
                app_id: t.app_id,
                channel_id: t.channel_id,
                user_id,
                actor: t.actor,
                ip: t.ip.clone(),
            },
            muted,
        )
        .await;
        match r {
            Ok(()) => out.affected.push(user_id),
            Err(e) => out.failed.push(BulkFailure {
                user_id,
                error: e.public_message(),
            }),
        }
    }
    control.audit.log(
        Some(t.app_id),
        t.actor,
        AuditAction::ChannelMuteAll,
        "channel",
        &t.channel_id.to_string(),
        serde_json::json!({
            "muted": muted,
            "affected": out.affected,
            "skipped": out.skipped,
            "failed": out.failed.len(),
        }),
        t.ip,
    );
    Ok(out)
}

/// Removes the user from the channel on this node, closes the persisted membership and
/// publishes `UserKicked`. Returns the number of memberships closed.
pub async fn kick_from_channel(
    control: &ControlPlane,
    sfu: &RwLock<SfuNode>,
    t: ModerationTarget,
    reason: String,
) -> Result<u64> {
    {
        let sfu = sfu.read();
        let _ = sfu.kick_user_from_channel(&t.user_id, &t.channel_id);
    }
    let removed = control
        .sessions
        .remove_user_from_channel(t.app_id, t.channel_id, t.user_id)
        .await?;
    control.audit.log(
        Some(t.app_id),
        t.actor,
        AuditAction::UserKicked,
        "user",
        &t.user_id.to_string(),
        serde_json::json!({"channel_id": t.channel_id, "reason": reason}),
        t.ip,
    );
    control.events.publish(ServerEvent::UserKicked {
        app_id: t.app_id,
        channel_id: t.channel_id,
        user_id: t.user_id,
        kicked_by: t.actor,
        reason,
        timestamp: chrono::Utc::now(),
    });
    Ok(removed)
}

/// Sets or lifts a server-mute: persisted per membership, mirrored on this node's sessions,
/// flagged in Redis for the other nodes and announced as `UserMuted` / `UserUnmuted`.
pub async fn set_server_mute(
    control: &ControlPlane,
    sfu: &RwLock<SfuNode>,
    t: ModerationTarget,
    muted: bool,
) -> Result<()> {
    control
        .sessions
        .set_server_mute(t.app_id, t.channel_id, t.user_id, muted)
        .await?;
    {
        let sfu = sfu.read();
        for s in sfu.sessions_for_user(&t.user_id) {
            if s.app_id == t.app_id {
                s.is_server_muted.store(muted, Ordering::Relaxed);
            }
        }
    }
    if let Some(ref redis) = control.redis {
        let _ = redis.set_global_mute(t.user_id, muted).await;
    }
    control.audit.log(
        Some(t.app_id),
        t.actor,
        if muted {
            AuditAction::UserMuted
        } else {
            AuditAction::UserUnmuted
        },
        "user",
        &t.user_id.to_string(),
        serde_json::json!({"channel_id": t.channel_id, "muted": muted}),
        t.ip,
    );
    let now = chrono::Utc::now();
    control.events.publish(if muted {
        ServerEvent::UserMuted {
            app_id: t.app_id,
            channel_id: t.channel_id,
            user_id: t.user_id,
            muted_by: t.actor,
            server_mute: true,
            timestamp: now,
        }
    } else {
        ServerEvent::UserUnmuted {
            app_id: t.app_id,
            channel_id: t.channel_id,
            user_id: t.user_id,
            unmuted_by: t.actor,
            timestamp: now,
        }
    });
    Ok(())
}
