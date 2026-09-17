use crate::errors::{ApiError, Json, Path, Query};
use crate::middleware::{ApiKeyContext, ClientIp};
use crate::state::AppState;
use aurix_auth::ValidatedToken;
use aurix_common::error::AurixError;
use aurix_common::types::*;
use axum::{
    extract::{Extension, State},
    http::HeaderMap,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

type JsonResult = Result<Json<serde_json::Value>, ApiError>;

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(value).map_err(|_| AurixError::Validation(format!("Invalid {field}")).into())
}

fn to_json<T: Serialize>(v: T) -> JsonResult {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| AurixError::Internal(format!("serialization failed: {e}")).into())
}

/// Tenant-checked user lookup; users of other apps are reported as not found.
async fn require_user(
    state: &AppState,
    app_id: AppId,
    user_id: UserId,
) -> Result<aurix_db::models::UserRow, ApiError> {
    aurix_db::queries::get_user(&state.control.pool, app_id.0, user_id.0)
        .await
        .map_err(|e| AurixError::Database(e.to_string()))?
        .ok_or_else(|| AurixError::UserNotFound(user_id.to_string()).into())
}

/// Moderation actions performed through an API key are attributed to the key unless the caller
/// names an explicit moderator, who must be a user of the same app.
async fn resolve_actor(
    state: &AppState,
    ctx: &ApiKeyContext,
    moderator_user_id: Option<&str>,
) -> Result<UserId, ApiError> {
    match moderator_user_id {
        Some(raw) => {
            let id = UserId::from_uuid(parse_uuid(raw, "moderator_user_id")?);
            require_user(state, ctx.app_id, id).await?;
            Ok(id)
        }
        None => Ok(ctx.actor()),
    }
}

fn client_ip_string(ip: Option<Extension<ClientIp>>) -> Option<String> {
    ip.map(|Extension(c)| c.0.to_string())
}

// ── Health / readiness ──

pub async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (participants, channels) = {
        let sfu = state.sfu.read();
        (sfu.active_participants(), sfu.active_channels())
    };
    Json(serde_json::json!({
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION"),
        "node_id": state.control.node_id,
        "timestamp": Utc::now().to_rfc3339(),
        "active_sessions": participants,
        "active_channels": channels,
    }))
}

/// Readiness: database (and Redis when configured) must answer.
pub async fn ready(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.control.pool)
        .await?;
    let redis_ok = match &state.control.redis {
        Some(r) => r.ping().await.is_ok(),
        None => !state.control.config.is_production(),
    };
    if !redis_ok {
        return Err(AurixError::Redis("Redis unreachable".into()).into());
    }
    Ok(Json(serde_json::json!({"status": "ready"})))
}

// ── Tokens (server-to-server) ──

#[derive(Deserialize)]
pub struct GenerateTokenRequest {
    /// Stable identifier of the player in the game's own account system.
    pub external_id: String,
    pub display_name: String,
    #[serde(default)]
    pub channels: Vec<ChannelPermission>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct TokenResponse {
    pub token: String,
    pub user_id: UserId,
    pub expires_at: String,
}

/// Issues an end-user JWT bound to the API key's app. The user row is upserted so bans, sessions
/// and moderation events reference a persisted identity.
pub async fn generate_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(req): Json<GenerateTokenRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    ctx.require("tokens:issue")?;
    let external_id = req.external_id.trim();
    if external_id.is_empty() || external_id.len() > 255 {
        return Err(AurixError::Validation("external_id must be 1..=255 characters".into()).into());
    }
    let display_name = req.display_name.trim();
    if display_name.is_empty() || display_name.len() > 64 {
        return Err(AurixError::Validation("display_name must be 1..=64 characters".into()).into());
    }
    let app_id = ctx.app_id;
    let user = aurix_db::models::UserRow {
        id: Uuid::now_v7(),
        app_id: app_id.0,
        external_id: external_id.to_string(),
        display_name: display_name.to_string(),
        metadata: req.metadata.clone(),
        is_banned: false,
        ban_reason: None,
        ban_expires_at: None,
        device_ids: Vec::new(),
        total_session_minutes: 0,
        last_seen_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let user = aurix_db::queries::upsert_user(&state.control.pool, &user).await?;
    if user.is_banned && user.ban_expires_at.map(|t| t > Utc::now()).unwrap_or(true) {
        return Err(AurixError::UserBanned(
            user.ban_reason.unwrap_or_else(|| "User is banned".into()),
        )
        .into());
    }
    if state.moderation.is_banned(app_id, UserId(user.id)).await? {
        return Err(AurixError::UserBanned("User is banned".into()).into());
    }
    let user_id = UserId(user.id);
    let token = state.control.jwt.generate_token(
        user_id,
        app_id,
        display_name,
        req.channels,
        req.metadata,
    )?;
    let expires_at = Utc::now() + Duration::seconds(state.control.config.auth.token_ttl_secs);
    Ok(Json(TokenResponse {
        token,
        user_id,
        expires_at: expires_at.to_rfc3339(),
    }))
}

// ── Channels ──

#[derive(Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    #[serde(default)]
    pub config: Option<ChannelConfig>,
}

pub async fn create_channel(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(req): Json<CreateChannelRequest>,
) -> JsonResult {
    ctx.require("channels:write")?;
    let app_id = ctx.app_id;
    let app = aurix_db::queries::get_app(&state.control.pool, app_id.0)
        .await?
        .ok_or_else(|| AurixError::AuthorizationDenied("Application is inactive".into()))?;
    let count = state.control.channels.count_channels(app_id).await?;
    if count >= app.max_channels as i64 {
        return Err(
            AurixError::Conflict("Channel quota reached for this application".into()).into(),
        );
    }
    let mut config = req.config.unwrap_or_default();
    if config.max_participants > app.max_participants_per_channel as u32 {
        config.max_participants = app.max_participants_per_channel as u32;
    }
    let channel = state
        .control
        .channels
        .create_channel(app_id, &req.name, config.clone())
        .await?;
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::ChannelCreated {
            app_id,
            channel_id: ChannelId(channel.id),
            channel_type: config.channel_type,
            timestamp: Utc::now(),
        });
    to_json(channel)
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    pub active_only: Option<bool>,
}

fn default_page() -> u32 {
    1
}
fn default_per_page() -> u32 {
    50
}

fn paging(page: u32, per_page: u32) -> (i64, i64) {
    let per_page = per_page.clamp(1, 200) as i64;
    let offset = (page.max(1) as i64 - 1) * per_page;
    (per_page, offset)
}

pub async fn list_channels(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<ListQuery>,
) -> JsonResult {
    ctx.require("channels:read")?;
    let app_id = ctx.app_id;
    let (limit, offset) = paging(query.page, query.per_page);
    let channels = if query.active_only.unwrap_or(false) {
        state
            .control
            .channels
            .list_active_channels(app_id, limit, offset)
            .await?
    } else {
        state
            .control
            .channels
            .list_channels(app_id, limit, offset)
            .await?
    };
    let total = state.control.channels.count_channels(app_id).await?;
    Ok(Json(
        serde_json::json!({ "data": channels, "page": query.page, "per_page": limit, "total": total }),
    ))
}

pub async fn get_channel(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("channels:read")?;
    let channel = state
        .control
        .channels
        .require_channel(ctx.app_id, ChannelId::from_uuid(channel_id))
        .await?;
    to_json(channel)
}

pub async fn update_channel(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
    Json(config): Json<ChannelConfig>,
) -> JsonResult {
    ctx.require("channels:write")?;
    if config.max_participants == 0 {
        return Err(AurixError::Validation("max_participants must be > 0".into()).into());
    }
    let channel_id = ChannelId::from_uuid(channel_id);
    state
        .control
        .channels
        .update_channel_config(ctx.app_id, channel_id, &config)
        .await?;
    let channel = state
        .control
        .channels
        .require_channel(ctx.app_id, channel_id)
        .await?;
    to_json(channel)
}

pub async fn delete_channel(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("channels:write")?;
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(channel_id);
    state
        .control
        .channels
        .delete_channel(app_id, channel_id)
        .await?;
    let participants = {
        let sfu = state.sfu.read();
        sfu.get_channel_participants(&channel_id)
    };
    {
        let sfu = state.sfu.read();
        for p in &participants {
            let _ = sfu.leave_channel(&p.session_id, &channel_id);
        }
    }
    for p in &participants {
        let _ = state
            .control
            .sessions
            .remove_channel_membership(channel_id, p.session_id)
            .await;
    }
    if let Some(rec) = &state.recording {
        rec.stop_channel(app_id, &channel_id).await;
    }
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::ChannelDestroyed {
            app_id,
            channel_id,
            timestamp: Utc::now(),
        });
    Ok(Json(serde_json::json!({"deleted": true})))
}

pub async fn get_channel_participants(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("channels:read")?;
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(channel_id);
    state
        .control
        .channels
        .require_channel(app_id, channel_id)
        .await?;
    let members = state
        .control
        .sessions
        .get_channel_members(app_id, channel_id)
        .await?;
    let live: Vec<serde_json::Value> = {
        let sfu = state.sfu.read();
        sfu.get_channel_participants(&channel_id)
            .into_iter()
            .filter(|s| s.app_id == app_id)
            .map(|s| {
                serde_json::json!({
                    "session_id": s.session_id,
                    "user_id": s.user_id,
                    "display_name": s.display_name,
                    "ssrc": s.ssrc,
                    "transport": format!("{:?}", *s.transport.read()),
                    "is_muted": s.is_muted.load(std::sync::atomic::Ordering::Relaxed),
                    "is_server_muted": s.is_server_muted.load(std::sync::atomic::Ordering::Relaxed),
                    "is_speaking": s.is_speaking.load(std::sync::atomic::Ordering::Relaxed),
                })
            })
            .collect()
    };
    Ok(Json(
        serde_json::json!({ "channel_id": channel_id, "memberships": members, "live_on_this_node": live }),
    ))
}

// ── Users ──

#[derive(Deserialize)]
pub struct UserSearchQuery {
    pub q: Option<String>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

pub async fn search_users(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<UserSearchQuery>,
) -> JsonResult {
    ctx.require("users:read")?;
    let (limit, offset) = paging(query.page, query.per_page);
    let q = query.q.unwrap_or_default();
    let users =
        aurix_db::queries::search_users(&state.control.pool, ctx.app_id.0, &q, limit, offset)
            .await?;
    to_json(users)
}

pub async fn get_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(user_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("users:read")?;
    let user = require_user(&state, ctx.app_id, UserId::from_uuid(user_id)).await?;
    let sessions = state
        .control
        .sessions
        .get_active_sessions_for_user(UserId(user.id))
        .await?;
    Ok(Json(
        serde_json::json!({ "user": user, "active_sessions": sessions }),
    ))
}

// ── Moderation ──

#[derive(Deserialize)]
pub struct BanRequest {
    pub user_id: String,
    pub scope: BanScope,
    pub reason: String,
    pub duration_hours: Option<i64>,
    pub device_id: Option<String>,
    pub ip_address: Option<String>,
    pub moderator_user_id: Option<String>,
}

pub async fn ban_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<BanRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let app_id = ctx.app_id;
    let user_id = UserId::from_uuid(parse_uuid(&req.user_id, "user_id")?);
    require_user(&state, app_id, user_id).await?;
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;
    if req.reason.trim().is_empty() || req.reason.len() > 1024 {
        return Err(AurixError::Validation("reason must be 1..=1024 characters".into()).into());
    }
    let duration = match req.duration_hours {
        Some(h) if h <= 0 => {
            return Err(AurixError::Validation("duration_hours must be positive".into()).into())
        }
        Some(h) => Some(Duration::hours(h)),
        None => None,
    };
    let ban = state
        .moderation
        .ban_user(
            app_id,
            user_id,
            req.scope,
            &req.reason,
            actor,
            duration,
            req.device_id.as_deref(),
            req.ip_address.as_deref(),
        )
        .await?;

    // Terminate live sessions of the banned user on this node.
    let sessions: Vec<SessionId> = {
        let sfu = state.sfu.read();
        sfu.sessions_for_user(&user_id)
            .into_iter()
            .filter(|s| s.app_id == app_id)
            .map(|s| s.session_id)
            .collect()
    };
    for sid in &sessions {
        let sfu = state.sfu.read();
        let _ = sfu.destroy_session(sid);
    }

    state.control.audit.log(
        Some(app_id),
        actor,
        AuditAction::UserBanned,
        "user",
        &user_id.to_string(),
        serde_json::json!({"reason": req.reason, "scope": format!("{:?}", req.scope), "ban_id": ban.id}),
        client_ip_string(ip),
    );
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::UserBanned {
            app_id,
            user_id,
            reason: req.reason,
            banned_by: actor,
            timestamp: Utc::now(),
        });
    to_json(ban)
}

#[derive(Deserialize)]
pub struct UnbanRequest {
    pub moderator_user_id: Option<String>,
}

pub async fn unban(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(ban_id): Path<Uuid>,
    Json(req): Json<UnbanRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;
    let user_id = state
        .moderation
        .unban_user(ctx.app_id, ban_id, actor)
        .await?;
    state.control.audit.log(
        Some(ctx.app_id),
        actor,
        AuditAction::UserUnbanned,
        "ban",
        &ban_id.to_string(),
        serde_json::json!({"user_id": user_id}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({"revoked": true, "ban_id": ban_id, "user_id": user_id}),
    ))
}

pub async fn unban_user_all(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(user_id): Path<Uuid>,
    Json(req): Json<UnbanRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let user_id = UserId::from_uuid(user_id);
    require_user(&state, ctx.app_id, user_id).await?;
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;
    let revoked = state
        .moderation
        .unban_user_all(ctx.app_id, user_id, actor)
        .await?;
    state.control.audit.log(
        Some(ctx.app_id),
        actor,
        AuditAction::UserUnbanned,
        "user",
        &user_id.to_string(),
        serde_json::json!({"revoked": revoked}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({"revoked": revoked, "user_id": user_id}),
    ))
}

#[derive(Deserialize)]
pub struct BanListQuery {
    pub user_id: Option<String>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

pub async fn list_bans(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<BanListQuery>,
) -> JsonResult {
    ctx.require("moderation:read")?;
    let (limit, offset) = paging(query.page, query.per_page);
    let user = match &query.user_id {
        Some(u) => Some(UserId::from_uuid(parse_uuid(u, "user_id")?)),
        None => None,
    };
    let bans = state
        .moderation
        .list_bans(ctx.app_id, user, limit, offset)
        .await?;
    to_json(bans)
}

#[derive(Deserialize)]
pub struct ServerMuteRequest {
    pub user_id: String,
    pub channel_id: String,
    pub muted: bool,
    pub moderator_user_id: Option<String>,
}

pub async fn server_mute(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<ServerMuteRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let app_id = ctx.app_id;
    let user_id = UserId::from_uuid(parse_uuid(&req.user_id, "user_id")?);
    let channel_id = ChannelId::from_uuid(parse_uuid(&req.channel_id, "channel_id")?);
    state
        .control
        .channels
        .require_channel(app_id, channel_id)
        .await?;
    require_user(&state, app_id, user_id).await?;
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;

    state
        .control
        .sessions
        .set_server_mute(app_id, channel_id, user_id, req.muted)
        .await?;
    {
        let sfu = state.sfu.read();
        for s in sfu.sessions_for_user(&user_id) {
            if s.app_id == app_id {
                s.is_server_muted
                    .store(req.muted, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    if let Some(ref redis) = state.control.redis {
        let _ = redis.set_global_mute(user_id, req.muted).await;
    }
    let action = if req.muted {
        AuditAction::UserMuted
    } else {
        AuditAction::UserUnmuted
    };
    state.control.audit.log(
        Some(app_id),
        actor,
        action,
        "user",
        &user_id.to_string(),
        serde_json::json!({"channel_id": channel_id, "muted": req.muted}),
        client_ip_string(ip),
    );
    let event = if req.muted {
        aurix_control::ServerEvent::UserMuted {
            app_id,
            channel_id,
            user_id,
            muted_by: actor,
            server_mute: true,
            timestamp: Utc::now(),
        }
    } else {
        aurix_control::ServerEvent::UserUnmuted {
            app_id,
            channel_id,
            user_id,
            unmuted_by: actor,
            timestamp: Utc::now(),
        }
    };
    state.control.events.publish(event);
    Ok(Json(serde_json::json!({"muted": req.muted})))
}

#[derive(Deserialize)]
pub struct KickRequest {
    pub user_id: String,
    pub channel_id: String,
    pub reason: String,
    pub moderator_user_id: Option<String>,
}

pub async fn kick_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<KickRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let app_id = ctx.app_id;
    let user_id = UserId::from_uuid(parse_uuid(&req.user_id, "user_id")?);
    let channel_id = ChannelId::from_uuid(parse_uuid(&req.channel_id, "channel_id")?);
    state
        .control
        .channels
        .require_channel(app_id, channel_id)
        .await?;
    require_user(&state, app_id, user_id).await?;
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;

    {
        let sfu = state.sfu.read();
        let _ = sfu.kick_user_from_channel(&user_id, &channel_id);
    }
    let removed = state
        .control
        .sessions
        .remove_user_from_channel(app_id, channel_id, user_id)
        .await?;
    state.control.audit.log(
        Some(app_id),
        actor,
        AuditAction::UserKicked,
        "user",
        &user_id.to_string(),
        serde_json::json!({"channel_id": channel_id, "reason": req.reason}),
        client_ip_string(ip),
    );
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::UserKicked {
            app_id,
            channel_id,
            user_id,
            kicked_by: actor,
            reason: req.reason,
            timestamp: Utc::now(),
        });
    Ok(Json(
        serde_json::json!({"kicked": true, "memberships_closed": removed}),
    ))
}

#[derive(Deserialize)]
pub struct ModerationListQuery {
    pub status: Option<String>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

pub async fn list_moderation_events(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<ModerationListQuery>,
) -> JsonResult {
    ctx.require("moderation:read")?;
    let (limit, offset) = paging(query.page, query.per_page);
    let events = state
        .moderation
        .list_events(ctx.app_id, query.status.as_deref(), limit, offset)
        .await?;
    to_json(events)
}

pub async fn get_moderation_event(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(event_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("moderation:read")?;
    let event = state
        .moderation
        .get_event(ctx.app_id, event_id)
        .await?
        .ok_or_else(|| AurixError::NotFound("Moderation event not found".into()))?;
    to_json(event)
}

#[derive(Deserialize)]
pub struct ResolveEventRequest {
    pub resolution: String,
    pub moderator_user_id: Option<String>,
}

pub async fn resolve_moderation_event(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(event_id): Path<Uuid>,
    Json(req): Json<ResolveEventRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    if req.resolution.trim().is_empty() || req.resolution.len() > 2048 {
        return Err(AurixError::Validation("resolution must be 1..=2048 characters".into()).into());
    }
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;
    state
        .moderation
        .resolve_event(ctx.app_id, event_id, actor, &req.resolution)
        .await?;
    state.control.audit.log(
        Some(ctx.app_id),
        actor,
        AuditAction::ModerationAction,
        "moderation_event",
        &event_id.to_string(),
        serde_json::json!({"resolution": req.resolution}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({"resolved": true, "event_id": event_id}),
    ))
}

#[derive(Deserialize)]
pub struct ReportRequest {
    pub target_user_id: String,
    pub reporter_user_id: String,
    pub channel_id: Option<String>,
    pub reason: String,
    pub evidence: Option<serde_json::Value>,
    pub recording_id: Option<String>,
}

/// Server-side report (game backend relays a player's report). Both users must exist in the app.
pub async fn report_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(req): Json<ReportRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let app_id = ctx.app_id;
    let target = UserId::from_uuid(parse_uuid(&req.target_user_id, "target_user_id")?);
    let reporter = UserId::from_uuid(parse_uuid(&req.reporter_user_id, "reporter_user_id")?);
    require_user(&state, app_id, target).await?;
    require_user(&state, app_id, reporter).await?;
    submit_report(
        &state,
        app_id,
        target,
        reporter,
        req.channel_id.as_deref(),
        &req.reason,
        req.evidence,
        req.recording_id.as_deref(),
    )
    .await
}

#[derive(Deserialize)]
pub struct SelfReportRequest {
    pub target_user_id: String,
    pub channel_id: Option<String>,
    pub reason: String,
    pub evidence: Option<serde_json::Value>,
    pub recording_id: Option<String>,
}

/// Player report authenticated with the player's own token; the reporter is the token subject.
pub async fn report_user_self(
    State(state): State<AppState>,
    Extension(token): Extension<ValidatedToken>,
    Json(req): Json<SelfReportRequest>,
) -> JsonResult {
    let target = UserId::from_uuid(parse_uuid(&req.target_user_id, "target_user_id")?);
    if target == token.user_id {
        return Err(AurixError::Validation("Cannot report yourself".into()).into());
    }
    require_user(&state, token.app_id, target).await?;
    let key = format!("report:{}", token.user_id);
    if !state.control.rate_limiter.check_with_cost(&key, 10.0) {
        return Err(AurixError::RateLimitExceeded("Too many reports".into()).into());
    }
    submit_report(
        &state,
        token.app_id,
        target,
        token.user_id,
        req.channel_id.as_deref(),
        &req.reason,
        req.evidence,
        req.recording_id.as_deref(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn submit_report(
    state: &AppState,
    app_id: AppId,
    target: UserId,
    reporter: UserId,
    channel_id: Option<&str>,
    reason: &str,
    evidence: Option<serde_json::Value>,
    recording_id: Option<&str>,
) -> JsonResult {
    let reason = reason.trim();
    if reason.is_empty() || reason.len() > 2048 {
        return Err(AurixError::Validation("reason must be 1..=2048 characters".into()).into());
    }
    let channel_id = match channel_id {
        Some(c) => {
            let cid = ChannelId::from_uuid(parse_uuid(c, "channel_id")?);
            state.control.channels.require_channel(app_id, cid).await?;
            Some(cid)
        }
        None => None,
    };
    let rec_id = match recording_id {
        Some(r) => {
            let rid = parse_uuid(r, "recording_id")?;
            let svc = state
                .recording
                .as_ref()
                .ok_or_else(|| AurixError::Validation("Recording is not enabled".into()))?;
            svc.get_recording(app_id, rid)
                .await?
                .ok_or_else(|| AurixError::NotFound("Recording not found".into()))?;
            Some(rid)
        }
        None => None,
    };
    let event = state
        .moderation
        .report_user(
            app_id, channel_id, target, reporter, reason, evidence, rec_id,
        )
        .await?;
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::ModerationEvent {
            app_id,
            event_type: "user_report".into(),
            target_user_id: target,
            details: serde_json::json!({"event_id": event.id, "channel_id": channel_id}),
            timestamp: Utc::now(),
        });
    to_json(event)
}

// ── Media nodes (admin) ──

pub async fn list_media_nodes(State(state): State<AppState>) -> JsonResult {
    state.control.nodes.refresh_from_db().await;
    to_json(state.control.nodes.get_all_nodes())
}

// ── Analytics ──

#[derive(Deserialize)]
pub struct AnalyticsQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

pub async fn get_analytics(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<AnalyticsQuery>,
) -> JsonResult {
    ctx.require("analytics:read")?;
    let app_id = ctx.app_id;
    let from = query
        .from
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now() - Duration::days(7));
    let to = query
        .to
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    if to < from || to - from > Duration::days(90) {
        return Err(AurixError::Validation(
            "Analytics range must be positive and at most 90 days".into(),
        )
        .into());
    }
    let snapshots =
        aurix_db::queries::get_analytics(&state.control.pool, app_id.0, from, to).await?;
    let active_sessions = state.control.sessions.count_active_sessions(app_id).await?;
    let active_channels = state.control.channels.count_active_channels(app_id).await?;
    let users = aurix_db::queries::count_users(&state.control.pool, app_id.0).await?;
    Ok(Json(serde_json::json!({
        "current": { "active_sessions": active_sessions, "active_channels": active_channels, "users": users },
        "history": snapshots,
    })))
}

// ── API keys ──

pub const DEFAULT_KEY_PERMISSIONS: &[&str] = &["*"];
const KNOWN_PERMISSIONS: &[&str] = &[
    "*",
    "tokens:issue",
    "turn:issue",
    "channels:read",
    "channels:write",
    "users:read",
    "moderation:read",
    "moderation:write",
    "recordings:read",
    "recordings:write",
    "keys:manage",
    "analytics:read",
    "audit:read",
];

fn validate_permissions(value: &serde_json::Value) -> Result<(), ApiError> {
    let arr = value
        .as_array()
        .ok_or_else(|| AurixError::Validation("permissions must be an array of strings".into()))?;
    if arr.is_empty() || arr.len() > 32 {
        return Err(
            AurixError::Validation("permissions must contain 1..=32 entries".into()).into(),
        );
    }
    for p in arr {
        let s = p
            .as_str()
            .ok_or_else(|| AurixError::Validation("permissions must be strings".into()))?;
        if !KNOWN_PERMISSIONS.contains(&s) {
            return Err(AurixError::Validation(format!("Unknown permission '{s}'")).into());
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub permissions: Option<serde_json::Value>,
    pub rate_limit: Option<i32>,
    pub expires_in_days: Option<i64>,
}

pub async fn create_api_key(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<CreateApiKeyRequest>,
) -> JsonResult {
    ctx.require("keys:manage")?;
    let name = req.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(AurixError::Validation("name must be 1..=128 characters".into()).into());
    }
    let permissions = req
        .permissions
        .unwrap_or_else(|| serde_json::json!(DEFAULT_KEY_PERMISSIONS));
    validate_permissions(&permissions)?;
    // A key cannot mint permissions it does not itself hold.
    if !ctx.has("*") {
        for p in permissions
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str())
        {
            if !ctx.has(p) {
                return Err(AurixError::AuthorizationDenied(format!(
                    "Cannot grant permission '{p}' not held by the calling key"
                ))
                .into());
            }
        }
    }
    let rate_limit = req.rate_limit.unwrap_or(100).clamp(1, 100_000);
    let expires_at = match req.expires_in_days {
        Some(d) if d <= 0 || d > 3650 => {
            return Err(
                AurixError::Validation("expires_in_days must be within 1..=3650".into()).into(),
            )
        }
        Some(d) => Some(Utc::now() + Duration::days(d)),
        None => None,
    };
    let (key_row, raw_key) = state
        .control
        .api_keys
        .create_key(ctx.app_id.0, name, permissions, rate_limit, expires_at)
        .await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::ApiKeyCreated,
        "api_key",
        &key_row.id.to_string(),
        serde_json::json!({"name": name}),
        client_ip_string(ip),
    );
    Ok(Json(serde_json::json!({
        "id": key_row.id,
        "key": raw_key,
        "name": key_row.name,
        "permissions": key_row.permissions,
        "created_at": key_row.created_at,
        "expires_at": key_row.expires_at,
    })))
}

pub async fn list_api_keys(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> JsonResult {
    ctx.require("keys:manage")?;
    let keys = state.control.api_keys.list_keys(ctx.app_id.0).await?;
    let redacted: Vec<serde_json::Value> = keys
        .into_iter()
        .map(|k| {
            serde_json::json!({
                "id": k.id, "name": k.name, "key_prefix": k.key_prefix, "permissions": k.permissions,
                "rate_limit": k.rate_limit, "active": k.active, "last_used_at": k.last_used_at,
                "expires_at": k.expires_at, "created_at": k.created_at, "revoked_at": k.revoked_at,
            })
        })
        .collect();
    to_json(redacted)
}

pub async fn revoke_api_key(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(key_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("keys:manage")?;
    state
        .control
        .api_keys
        .revoke_key(ctx.app_id.0, key_id)
        .await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::ApiKeyRevoked,
        "api_key",
        &key_id.to_string(),
        serde_json::json!({}),
        client_ip_string(ip),
    );
    Ok(Json(serde_json::json!({"revoked": true, "id": key_id})))
}

// ── Audit log ──

pub async fn list_audit_logs(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<ListQuery>,
) -> JsonResult {
    ctx.require("audit:read")?;
    let (limit, offset) = paging(query.page, query.per_page);
    let logs =
        aurix_db::queries::list_audit_logs(&state.control.pool, Some(ctx.app_id.0), limit, offset)
            .await?;
    to_json(logs)
}

pub async fn admin_list_audit_logs(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> JsonResult {
    let (limit, offset) = paging(query.page, query.per_page);
    let logs = aurix_db::queries::list_audit_logs(&state.control.pool, None, limit, offset).await?;
    to_json(logs)
}

// ── Recordings ──

fn recording_service(state: &AppState) -> Result<&aurix_recording::RecordingService, ApiError> {
    state
        .recording
        .as_deref()
        .ok_or_else(|| AurixError::InvalidConfiguration("Recording not enabled".into()).into())
}

#[derive(Deserialize)]
pub struct StartRecordingRequest {
    pub channel_id: String,
    pub user_id: String,
    pub session_id: Option<String>,
}

pub async fn start_recording(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<StartRecordingRequest>,
) -> JsonResult {
    ctx.require("recordings:write")?;
    let svc = recording_service(&state)?;
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(parse_uuid(&req.channel_id, "channel_id")?);
    let user_id = UserId::from_uuid(parse_uuid(&req.user_id, "user_id")?);
    state
        .control
        .channels
        .require_channel(app_id, channel_id)
        .await?;
    require_user(&state, app_id, user_id).await?;

    // The recording is tied to a live session of the user in this channel.
    let session_id = match &req.session_id {
        Some(s) => {
            let sid = SessionId::from_uuid(parse_uuid(s, "session_id")?);
            let row = state
                .control
                .sessions
                .get_session(app_id, sid)
                .await?
                .ok_or_else(|| AurixError::SessionNotFound(sid.to_string()))?;
            if row.user_id != user_id.0 || row.disconnected_at.is_some() {
                return Err(AurixError::Validation(
                    "session does not belong to user or is closed".into(),
                )
                .into());
            }
            sid
        }
        None => {
            let members = state
                .control
                .sessions
                .get_channel_members(app_id, channel_id)
                .await?;
            members
                .iter()
                .find(|m| m.user_id == user_id.0)
                .map(|m| SessionId(m.session_id))
                .ok_or_else(|| {
                    AurixError::Validation("user is not currently in the channel".into())
                })?
        }
    };

    let rec = svc
        .start_recording(app_id, channel_id, session_id, user_id, 48000, 1)
        .await?;
    state.control.audit.log(
        Some(app_id),
        ctx.actor(),
        AuditAction::RecordingStarted,
        "recording",
        &rec.id.to_string(),
        serde_json::json!({"channel_id": channel_id, "user_id": user_id}),
        client_ip_string(ip),
    );
    if svc.require_consent() {
        state
            .control
            .events
            .publish(aurix_control::ServerEvent::RecordingConsentRequired {
                app_id,
                channel_id,
                recording_id: rec.id,
                initiated_by: user_id,
                timestamp: Utc::now(),
            });
    }
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::RecordingStarted {
            app_id,
            channel_id,
            recording_id: rec.id,
            timestamp: Utc::now(),
        });
    to_json(rec)
}

pub async fn stop_recording(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(recording_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("recordings:write")?;
    let svc = recording_service(&state)?;
    let rec = svc.stop_recording(ctx.app_id, recording_id).await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::RecordingStopped,
        "recording",
        &recording_id.to_string(),
        serde_json::json!({"duration_secs": rec.duration_secs}),
        client_ip_string(ip),
    );
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::RecordingStopped {
            app_id: ctx.app_id,
            channel_id: ChannelId(rec.channel_id),
            recording_id,
            duration_secs: rec.duration_secs,
            timestamp: Utc::now(),
        });
    to_json(rec)
}

#[derive(Deserialize)]
pub struct RecordingListQuery {
    pub channel_id: Option<String>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

pub async fn list_recordings(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<RecordingListQuery>,
) -> JsonResult {
    ctx.require("recordings:read")?;
    let svc = recording_service(&state)?;
    let (limit, offset) = paging(query.page, query.per_page);
    let channel = match &query.channel_id {
        Some(c) => Some(ChannelId::from_uuid(parse_uuid(c, "channel_id")?)),
        None => None,
    };
    to_json(
        svc.list_recordings(ctx.app_id, channel, limit, offset)
            .await?,
    )
}

pub async fn get_recording(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(recording_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("recordings:read")?;
    let svc = recording_service(&state)?;
    let recording = svc
        .get_recording(ctx.app_id, recording_id)
        .await?
        .ok_or_else(|| AurixError::NotFound("Recording not found".into()))?;
    let download_url = svc.presigned_url(ctx.app_id, recording_id, 900)?;
    let consent = svc.consent_state(&recording_id);
    Ok(Json(
        serde_json::json!({ "recording": recording, "download_url": download_url, "consent": consent }),
    ))
}

pub async fn download_recording(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(recording_id): Path<Uuid>,
) -> Result<axum::response::Response, ApiError> {
    ctx.require("recordings:read")?;
    let svc = recording_service(&state)?;
    let (row, bytes) = svc.read_recording(ctx.app_id, recording_id).await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::RecordingAccessed,
        "recording",
        &recording_id.to_string(),
        serde_json::json!({"bytes": bytes.len()}),
        client_ip_string(ip),
    );
    let content_type = match row.format.as_str() {
        "ogg" | "ogg_opus" => "audio/ogg",
        _ => "application/octet-stream",
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        content_type
            .parse()
            .map_err(|_| AurixError::Internal("bad content type".into()))?,
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        format!(
            "attachment; filename=\"{recording_id}.{}\"",
            if content_type == "audio/ogg" {
                "ogg"
            } else {
                "bin"
            }
        )
        .parse()
        .map_err(|_| AurixError::Internal("bad disposition".into()))?,
    );
    Ok(axum::response::IntoResponse::into_response((
        headers, bytes,
    )))
}

pub async fn delete_recording(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(recording_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("recordings:write")?;
    let svc = recording_service(&state)?;
    svc.delete_recording(ctx.app_id, recording_id).await?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::RecordingDeleted,
        "recording",
        &recording_id.to_string(),
        serde_json::json!({}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({"deleted": true, "id": recording_id}),
    ))
}

#[derive(Deserialize)]
pub struct ConsentRequest {
    pub consent: RecordingConsent,
}

/// Player consent for a recording of a channel they are in (token-authenticated).
pub async fn recording_consent_self(
    State(state): State<AppState>,
    Extension(token): Extension<ValidatedToken>,
    Path(recording_id): Path<Uuid>,
    Json(req): Json<ConsentRequest>,
) -> JsonResult {
    let svc = recording_service(&state)?;
    svc.set_consent(token.app_id, recording_id, token.user_id, req.consent)
        .await?;
    Ok(Json(
        serde_json::json!({"recording_id": recording_id, "consent": req.consent}),
    ))
}

// ── TURN credentials ──

#[derive(Deserialize)]
pub struct TurnCredentialsRequest {
    pub user_id: String,
}

#[derive(Serialize)]
pub struct TurnCredentialsResponse {
    pub username: String,
    pub password: String,
    pub ttl: i64,
    pub expires_at: i64,
    pub uris: Vec<String>,
}

fn issue_turn_credentials(
    state: &AppState,
    app_id: AppId,
    user_id: UserId,
) -> Result<TurnCredentialsResponse, ApiError> {
    let turn = &state.control.config.turn;
    if !turn.enabled {
        return Err(AurixError::InvalidConfiguration("TURN is not enabled".into()).into());
    }
    let ttl = turn.credential_ttl_secs.clamp(60, 86_400);
    let creds = aurix_common::crypto::generate_turn_credentials(
        &turn.auth_secret,
        &format!("{}/{}", app_id.0, user_id.0),
        ttl,
        Utc::now().timestamp(),
    );
    let host = turn
        .external_ip
        .clone()
        .or_else(|| state.control.config.media.external_ip.clone())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| {
            if turn.host == "0.0.0.0" || turn.host == "::" {
                "127.0.0.1".to_string()
            } else {
                turn.host.clone()
            }
        });
    let uris = vec![
        format!("turn:{}:{}?transport=udp", host, turn.udp_port),
        format!("turn:{}:{}?transport=tcp", host, turn.tcp_port),
        format!("stun:{}:{}", host, turn.udp_port),
    ];
    Ok(TurnCredentialsResponse {
        username: creds.username,
        password: creds.password,
        ttl,
        expires_at: creds.expires_at,
        uris,
    })
}

/// Server-to-server: credentials for one of the app's users.
pub async fn get_turn_credentials(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(req): Json<TurnCredentialsRequest>,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    ctx.require("turn:issue")?;
    let user_id = UserId::from_uuid(parse_uuid(&req.user_id, "user_id")?);
    require_user(&state, ctx.app_id, user_id).await?;
    Ok(Json(issue_turn_credentials(&state, ctx.app_id, user_id)?))
}

/// Player-facing: credentials for the token subject.
pub async fn get_turn_credentials_self(
    State(state): State<AppState>,
    Extension(token): Extension<ValidatedToken>,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    Ok(Json(issue_turn_credentials(
        &state,
        token.app_id,
        token.user_id,
    )?))
}

// ── Admin authentication ──

#[derive(Deserialize)]
pub struct AdminLoginRequest {
    pub email: String,
    pub password: String,
}

pub async fn admin_login(
    State(state): State<AppState>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<AdminLoginRequest>,
) -> JsonResult {
    let ip_key = client_ip_string(ip).unwrap_or_else(|| "unknown".into());
    if !state
        .control
        .rate_limiter
        .check_with_cost(&format!("admin-login:{ip_key}"), 5.0)
    {
        return Err(AurixError::RateLimitExceeded("Too many login attempts".into()).into());
    }
    let (admin, token) = state
        .control
        .admin_auth
        .authenticate(&req.email, &req.password)
        .await?;
    state.control.audit.log(
        None,
        UserId(admin.id),
        AuditAction::AdminLogin,
        "admin",
        &admin.id.to_string(),
        serde_json::json!({}),
        client_ip_string(ip),
    );
    Ok(Json(serde_json::json!({
        "token": token,
        "expires_in_secs": state.control.config.auth.admin_token_ttl_secs,
        "admin": { "id": admin.id, "email": admin.email, "display_name": admin.display_name, "role": admin.role }
    })))
}

#[derive(Deserialize)]
pub struct BootstrapAdminRequest {
    pub email: String,
    pub password: String,
    pub display_name: String,
}

/// First-run bootstrap. Allowed only when no admin exists, or with `X-Bootstrap-Token` matching
/// `auth.admin_bootstrap_token`.
pub async fn admin_setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<BootstrapAdminRequest>,
) -> JsonResult {
    let ip_key = client_ip_string(ip).unwrap_or_else(|| "unknown".into());
    if !state
        .control
        .rate_limiter
        .check_with_cost(&format!("admin-setup:{ip_key}"), 10.0)
    {
        return Err(AurixError::RateLimitExceeded("Too many attempts".into()).into());
    }
    let presented = headers
        .get("x-bootstrap-token")
        .and_then(|v| v.to_str().ok());
    let admin = state
        .control
        .admin_auth
        .bootstrap_admin(presented, &req.email, &req.password, &req.display_name)
        .await?;
    state.control.audit.log(
        None,
        UserId(admin.id),
        AuditAction::AdminCreated,
        "admin",
        &admin.id.to_string(),
        serde_json::json!({"bootstrap": true}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({ "id": admin.id, "email": admin.email, "display_name": admin.display_name, "role": admin.role }),
    ))
}

#[derive(Deserialize)]
pub struct CreateAdminRequest {
    pub email: String,
    pub password: String,
    pub display_name: String,
    pub role: Option<String>,
}

pub async fn create_admin(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<CreateAdminRequest>,
) -> JsonResult {
    if admin.role != "superadmin" {
        return Err(AurixError::AuthorizationDenied(
            "Only superadmins can create administrators".into(),
        )
        .into());
    }
    let role = req.role.as_deref().unwrap_or("admin");
    let created = state
        .control
        .admin_auth
        .create_admin(&req.email, &req.password, &req.display_name, role)
        .await?;
    state.control.audit.log(
        None,
        UserId(admin.admin_id),
        AuditAction::AdminCreated,
        "admin",
        &created.id.to_string(),
        serde_json::json!({"role": role}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({ "id": created.id, "email": created.email, "display_name": created.display_name, "role": created.role }),
    ))
}

pub async fn admin_me(Extension(admin): Extension<AdminContext>) -> JsonResult {
    Ok(Json(
        serde_json::json!({ "id": admin.admin_id, "email": admin.email, "role": admin.role }),
    ))
}

// ── App management (admin) ──

#[derive(Deserialize)]
pub struct CreateAppRequest {
    pub name: String,
    pub description: Option<String>,
    pub max_channels: Option<i32>,
    pub max_participants_per_channel: Option<i32>,
}

pub async fn create_app(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<CreateAppRequest>,
) -> JsonResult {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(AurixError::Validation("name must be 1..=128 characters".into()).into());
    }
    let max_channels = req.max_channels.unwrap_or(10_000);
    let max_participants = req.max_participants_per_channel.unwrap_or(256);
    if max_channels <= 0 || max_participants <= 0 || max_participants > 10_000 {
        return Err(AurixError::Validation("Invalid quota values".into()).into());
    }
    let app_id = Uuid::now_v7();
    let app = aurix_db::models::AppRow {
        id: app_id,
        name: name.to_string(),
        description: req.description,
        owner_id: admin.admin_id,
        api_key_hash: String::new(),
        api_secret_hash: String::new(),
        active: true,
        max_channels,
        max_participants_per_channel: max_participants,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let created = aurix_db::queries::create_app(&state.control.pool, &app).await?;
    let (key_row, raw_key) = state
        .control
        .api_keys
        .create_key(
            app_id,
            "default",
            serde_json::json!(DEFAULT_KEY_PERMISSIONS),
            100,
            None,
        )
        .await?;
    aurix_db::queries::update_app_key_hash(&state.control.pool, app_id, &key_row.key_hash).await?;
    state.control.audit.log(
        Some(AppId(app_id)),
        UserId(admin.admin_id),
        AuditAction::AppCreated,
        "app",
        &app_id.to_string(),
        serde_json::json!({"name": name}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({ "id": created.id, "name": created.name, "api_key": raw_key, "api_key_id": key_row.id }),
    ))
}

pub async fn list_apps(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> JsonResult {
    let (limit, offset) = paging(query.page, query.per_page);
    let apps = aurix_db::queries::list_apps(&state.control.pool, limit, offset).await?;
    let total = aurix_db::queries::count_apps(&state.control.pool).await?;
    let redacted: Vec<serde_json::Value> = apps
        .into_iter()
        .map(|a| serde_json::json!({ "id": a.id, "name": a.name, "description": a.description, "owner_id": a.owner_id, "active": a.active, "max_channels": a.max_channels, "max_participants_per_channel": a.max_participants_per_channel, "created_at": a.created_at }))
        .collect();
    Ok(Json(
        serde_json::json!({ "data": redacted, "total": total }),
    ))
}

pub async fn get_app(State(state): State<AppState>, Path(app_id): Path<Uuid>) -> JsonResult {
    let a = aurix_db::queries::get_app(&state.control.pool, app_id)
        .await?
        .ok_or_else(|| AurixError::NotFound("Application not found".into()))?;
    let channels = state.control.channels.count_channels(AppId(app_id)).await?;
    let sessions = state
        .control
        .sessions
        .count_active_sessions(AppId(app_id))
        .await?;
    Ok(Json(
        serde_json::json!({ "id": a.id, "name": a.name, "description": a.description, "owner_id": a.owner_id, "active": a.active, "max_channels": a.max_channels, "max_participants_per_channel": a.max_participants_per_channel, "created_at": a.created_at, "channels": channels, "active_sessions": sessions }),
    ))
}

pub async fn delete_app(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
    ip: Option<Extension<ClientIp>>,
    Path(app_id): Path<Uuid>,
) -> JsonResult {
    if admin.role != "superadmin" {
        return Err(AurixError::AuthorizationDenied(
            "Only superadmins can delete applications".into(),
        )
        .into());
    }
    aurix_db::queries::delete_app(&state.control.pool, app_id).await?;
    state.control.audit.log(
        Some(AppId(app_id)),
        UserId(admin.admin_id),
        AuditAction::AppDeleted,
        "app",
        &app_id.to_string(),
        serde_json::json!({}),
        client_ip_string(ip),
    );
    Ok(Json(serde_json::json!({"deleted": true})))
}

/// Admin-issued replacement key for an app (e.g. after the original leaked).
pub async fn admin_rotate_app_key(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
    ip: Option<Extension<ClientIp>>,
    Path(app_id): Path<Uuid>,
) -> JsonResult {
    aurix_db::queries::get_app(&state.control.pool, app_id)
        .await?
        .ok_or_else(|| AurixError::NotFound("Application not found".into()))?;
    let (key_row, raw_key) = state
        .control
        .api_keys
        .create_key(
            app_id,
            "rotated",
            serde_json::json!(DEFAULT_KEY_PERMISSIONS),
            100,
            None,
        )
        .await?;
    state.control.audit.log(
        Some(AppId(app_id)),
        UserId(admin.admin_id),
        AuditAction::ApiKeyCreated,
        "api_key",
        &key_row.id.to_string(),
        serde_json::json!({"rotated_by_admin": true}),
        client_ip_string(ip),
    );
    Ok(Json(
        serde_json::json!({ "api_key": raw_key, "api_key_id": key_row.id }),
    ))
}

// ── WebRTC signaling (token-authenticated) ──

#[derive(Deserialize)]
pub struct WebRtcOfferRequest {
    pub sdp: String,
    /// Session obtained from the WebSocket `SessionInitAck`.
    pub session_id: String,
}

/// Attaches a browser transport to an existing session owned by the token subject.
pub async fn webrtc_offer(
    State(state): State<AppState>,
    Extension(token): Extension<ValidatedToken>,
    Json(req): Json<WebRtcOfferRequest>,
) -> JsonResult {
    let session_id = SessionId::from_uuid(parse_uuid(&req.session_id, "session_id")?);
    if req.sdp.len() > 64 * 1024 {
        return Err(AurixError::Validation("SDP too large".into()).into());
    }
    let answer = {
        let sfu = state.sfu.read();
        let session = sfu
            .get_session(&session_id)
            .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
        if session.user_id != token.user_id || session.app_id != token.app_id {
            return Err(
                AurixError::AuthorizationDenied("Session belongs to another user".into()).into(),
            );
        }
        sfu.attach_webrtc(&session_id, &req.sdp)?
    };
    Ok(Json(
        serde_json::json!({ "session_id": session_id, "sdp": answer }),
    ))
}
