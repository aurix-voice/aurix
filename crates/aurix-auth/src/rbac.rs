use aurix_common::error::{AurixError, Result};
use aurix_common::types::{ChannelId, ChannelPermission, ChannelRole};

pub struct RbacService;

impl RbacService {
    pub fn new() -> Self {
        Self
    }

    pub fn check_channel_join(
        &self,
        channel_id: &ChannelId,
        permissions: &[ChannelPermission],
    ) -> Result<()> {
        let perm = permissions
            .iter()
            .find(|p| p.channel_id == *channel_id)
            .ok_or_else(|| {
                AurixError::AuthorizationDenied(format!(
                    "No permission for channel {}",
                    channel_id
                ))
            })?;

        if !perm.join {
            return Err(AurixError::AuthorizationDenied(
                "Join permission denied for this channel".into(),
            ));
        }
        Ok(())
    }

    pub fn check_channel_speak(
        &self,
        channel_id: &ChannelId,
        permissions: &[ChannelPermission],
    ) -> Result<()> {
        let perm = permissions
            .iter()
            .find(|p| p.channel_id == *channel_id)
            .ok_or_else(|| {
                AurixError::AuthorizationDenied(format!(
                    "No permission for channel {}",
                    channel_id
                ))
            })?;

        if !perm.speak {
            return Err(AurixError::AuthorizationDenied(
                "Speak permission denied for this channel".into(),
            ));
        }
        Ok(())
    }

    pub fn check_channel_receive(
        &self,
        channel_id: &ChannelId,
        permissions: &[ChannelPermission],
    ) -> Result<()> {
        let perm = permissions
            .iter()
            .find(|p| p.channel_id == *channel_id)
            .ok_or_else(|| {
                AurixError::AuthorizationDenied(format!(
                    "No permission for channel {}",
                    channel_id
                ))
            })?;

        if !perm.receive {
            return Err(AurixError::AuthorizationDenied(
                "Receive permission denied for this channel".into(),
            ));
        }
        Ok(())
    }

    pub fn check_channel_moderate(
        &self,
        channel_id: &ChannelId,
        permissions: &[ChannelPermission],
    ) -> Result<()> {
        let perm = permissions
            .iter()
            .find(|p| p.channel_id == *channel_id)
            .ok_or_else(|| {
                AurixError::AuthorizationDenied(format!(
                    "No permission for channel {}",
                    channel_id
                ))
            })?;

        if !perm.moderate {
            return Err(AurixError::AuthorizationDenied(
                "Moderate permission denied for this channel".into(),
            ));
        }
        Ok(())
    }

    pub fn role_from_permissions(
        &self,
        channel_id: &ChannelId,
        permissions: &[ChannelPermission],
    ) -> ChannelRole {
        let perm = match permissions.iter().find(|p| p.channel_id == *channel_id) {
            Some(p) => p,
            None => return ChannelRole::Listener,
        };

        if perm.moderate {
            ChannelRole::Moderator
        } else if perm.speak {
            ChannelRole::Speaker
        } else {
            ChannelRole::Listener
        }
    }

    pub fn can_kick(actor_role: ChannelRole, target_role: ChannelRole) -> bool {
        actor_role.can_moderate() && actor_role.precedence() > target_role.precedence()
    }

    pub fn can_server_mute(actor_role: ChannelRole) -> bool {
        actor_role.can_moderate()
    }

    pub fn can_ban(actor_role: ChannelRole) -> bool {
        actor_role.can_administrate()
    }

    pub fn can_configure_channel(actor_role: ChannelRole) -> bool {
        actor_role.can_administrate()
    }

    pub fn can_start_recording(actor_role: ChannelRole) -> bool {
        actor_role.can_moderate()
    }
}

impl Default for RbacService {
    fn default() -> Self {
        Self::new()
    }
}