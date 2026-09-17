use crate::errors::ApiError;
use crate::state::AppState;
use aurix_common::error::AurixError;
use aurix_common::types::*;
use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Health ──

pub async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let sfu = state.sfu.read();
    Json(serde_json::json!({
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": Utc::now().to_rfc3339(),
        "active_sessions": sfu.active_participants(),
        "active_channels": sfu.active_channels(),
    }))
}

// ── Token Generation ──

#[derive(Deserialize)]
pub struct GenerateTokenRequest {
    pub user_id: String,
    pub app_id: String,
    pub display_name: String,
    #[serde(default)]
    pub channels: Vec<ChannelPermission>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct TokenResponse {
    pub token: String,
    pub expires_at: String,
}

pub async fn generate_token(
    State(state): State<AppState>,
    Json(req): Json<GenerateTokenRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    let user_id = UserId::from_uuid(
        Uuid::parse_str(&req.user_id)
            .map_err(|_| AurixError::Validation("Invalid user_id".into()))?,
    );
    let app_id = AppId::from_uuid(
        Uuid::parse_str(&req.app_id)
            .map_err(|_| AurixError::Validation("Invalid app_id".into()))?,
    );

    let token = state.control.jwt.generate_token(
        user_id,
        app_id,
        &req.display_name,
        req.channels,
        req.metadata,
    )?;

    let expires_at = Utc::now() + Duration::seconds(state.control.config.auth.token_ttl_secs);

    Ok(Json(TokenResponse {
        token,
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
    Extension(app_id): Extension<AppId>,
    Json(req): Json<CreateChannelRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = req.config.unwrap_or_default();
    let channel = state.control.channels.create_channel(app_id, &req.name, config).await?;
    Ok(Json(serde_json::to_value(channel).unwrap()))
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    pub active_only: Option<bool>,
}

fn default_page() -> u32 { 1 }
fn default_per_page() -> u32 { 50 }

pub async fn list_channels(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = query.per_page.min(200) as i64;
    let offset = ((query.page.saturating_sub(1)) * query.per_page) as i64;

    let channels = if query.active_only.unwrap_or(false) {
        state.control.channels.list_active_channels(app_id, limit, offset).await?
    } else {
        state.control.channels.list_channels(app_id, limit, offset).await?
    };

    let total = state.control.channels.count_channels(app_id).await?;

    Ok(Json(serde_json::json!({
        "data": channels,
        "page": query.page,
        "per_page": query.per_page,
        "total": total,
    })))
}

pub async fn get_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let channel = state
        .control
        .channels
        .get_channel(ChannelId::from_uuid(channel_id))
        .await?
        .ok_or(AurixError::ChannelNotFound(channel_id.to_string()))?;

    Ok(Json(serde_json::to_value(channel).unwrap()))
}

pub async fn delete_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .control
        .channels
        .delete_channel(ChannelId::from_uuid(channel_id))
        .await?;

    Ok(Json(serde_json::json!({"deleted": true})))
}

pub async fn get_channel_participants(
    State(state): State<AppState>,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let members = state
        .control
        .sessions
        .get_channel_members(ChannelId::from_uuid(channel_id))
        .await?;

    Ok(Json(serde_json::to_value(members).unwrap()))
}

// ── Users ──

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

pub async fn search_users(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let search_term = query.q.unwrap_or_default();
    let limit = query.per_page.min(200) as i64;
    let offset = ((query.page.saturating_sub(1)) * query.per_page) as i64;

    let users = aurix_db::queries::search_users(
        &state.control.pool, app_id.0, &search_term, limit, offset,
    ).await.map_err(|e| AurixError::Database(e.to_string()))?;

    let total = aurix_db::queries::count_users(&state.control.pool, app_id.0)
        .await.map_err(|e| AurixError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "data": users,
        "page": query.page,
        "per_page": query.per_page,
        "total": total,
    })))
}

pub async fn get_user(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = aurix_db::queries::get_user(&state.control.pool, user_id)
        .await.map_err(|e| AurixError::Database(e.to_string()))?
        .ok_or(AurixError::UserNotFound(user_id.to_string()))?;

    Ok(Json(serde_json::to_value(user).unwrap()))
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
}

pub async fn ban_user(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Json(req): Json<BanRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = UserId::from_uuid(
        Uuid::parse_str(&req.user_id).map_err(|_| AurixError::Validation("Invalid user_id".into()))?,
    );
    let admin_id = user_id;
    let duration = req.duration_hours.map(Duration::hours);
    let ban = state.moderation.ban_user(
        app_id, user_id, req.scope, &req.reason, admin_id, duration,
        req.device_id.as_deref(), req.ip_address.as_deref(),
    ).await?;

    if let Some(ref redis) = state.control.redis {
        let _ = redis.set_global_mute(user_id, true).await;
    }

    state.control.audit.log(
        admin_id, AuditAction::UserBanned,
        "user", &user_id.to_string(),
        serde_json::json!({"reason": req.reason, "scope": format!("{:?}", req.scope)}),
        None,
    );
    state.control.events.publish(aurix_control::ServerEvent::UserBanned {
        app_id, user_id, reason: req.reason, banned_by: admin_id, timestamp: Utc::now(),
    });
    Ok(Json(serde_json::to_value(ban).unwrap()))
}

#[derive(Deserialize)]
pub struct ServerMuteRequest {
    pub user_id: String,
    pub channel_id: String,
    pub muted: bool,
}

pub async fn server_mute(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Json(req): Json<ServerMuteRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = UserId::from_uuid(
        Uuid::parse_str(&req.user_id).map_err(|_| AurixError::Validation("Invalid user_id".into()))?,
    );
    let channel_id = ChannelId::from_uuid(
        Uuid::parse_str(&req.channel_id).map_err(|_| AurixError::Validation("Invalid channel_id".into()))?,
    );
    state.control.sessions.set_server_mute(channel_id, user_id, req.muted).await?;
    {
        let sfu = state.sfu.read();
        let _ = sfu.server_mute_user(&user_id, req.muted);
    }

    if let Some(ref redis) = state.control.redis {
        let _ = redis.set_global_mute(user_id, req.muted).await;
        let event = serde_json::to_string(&aurix_control::ServerEvent::UserMuted {
            app_id, channel_id, user_id,
            muted_by: user_id, server_mute: true, timestamp: Utc::now(),
        }).unwrap_or_default();
        let _ = redis.publish_event(&event).await;
    }
    Ok(Json(serde_json::json!({"muted": req.muted})))
}

#[derive(Deserialize)]
pub struct KickRequest {
    pub user_id: String,
    pub channel_id: String,
    pub reason: String,
}

pub async fn kick_user(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Json(req): Json<KickRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = UserId::from_uuid(
        Uuid::parse_str(&req.user_id).map_err(|_| AurixError::Validation("Invalid user_id".into()))?,
    );
    let channel_id = ChannelId::from_uuid(
        Uuid::parse_str(&req.channel_id).map_err(|_| AurixError::Validation("Invalid channel_id".into()))?,
    );

    {
        let sfu = state.sfu.read();
        let _ = sfu.kick_user_from_channel(&user_id, &channel_id);
    }

    state.control.sessions.remove_channel_membership(channel_id, user_id).await?;
    state.control.events.publish(aurix_control::ServerEvent::UserKicked {
        app_id, channel_id, user_id, kicked_by: user_id,
        reason: req.reason, timestamp: Utc::now(),
    });
    Ok(Json(serde_json::json!({"kicked": true})))
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
    Extension(app_id): Extension<AppId>,
    Query(query): Query<ModerationListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = query.per_page.min(200) as i64;
    let offset = ((query.page.saturating_sub(1)) * query.per_page) as i64;

    let events = state
        .moderation
        .list_events(app_id, query.status.as_deref(), limit, offset)
        .await?;

    Ok(Json(serde_json::to_value(events).unwrap()))
}

// ── Media Nodes ──

pub async fn list_media_nodes(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let nodes = state.control.nodes.get_all_nodes();
    Ok(Json(serde_json::to_value(nodes).unwrap()))
}

// ── Analytics ──

#[derive(Deserialize)]
pub struct AnalyticsQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

pub async fn get_analytics(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Query(query): Query<AnalyticsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
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

    let snapshots = aurix_db::queries::get_analytics(&state.control.pool, app_id.0, from, to)
        .await.map_err(|e| AurixError::Database(e.to_string()))?;

    let active_sessions = state.control.sessions.count_active_sessions(app_id).await?;
    let active_channels = state.control.channels.count_active_channels(app_id).await?;

    Ok(Json(serde_json::json!({
        "current": {
            "active_sessions": active_sessions,
            "active_channels": active_channels,
        },
        "history": snapshots,
    })))
}

// ── API Keys ──

#[derive(Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub permissions: Option<serde_json::Value>,
    pub rate_limit: Option<i32>,
    pub expires_in_days: Option<i64>,
}

pub async fn create_api_key(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let permissions = req.permissions.unwrap_or_else(|| serde_json::json!({"all": true}));
    let rate_limit = req.rate_limit.unwrap_or(100);
    let expires_at = req
        .expires_in_days
        .map(|d| Utc::now() + Duration::days(d));

    let (key_row, raw_key) = state
        .control
        .api_keys
        .create_key(app_id.0, &req.name, permissions, rate_limit, expires_at)
        .await?;

    Ok(Json(serde_json::json!({
        "id": key_row.id,
        "key": raw_key,
        "name": key_row.name,
        "created_at": key_row.created_at,
        "expires_at": key_row.expires_at,
    })))
}

pub async fn list_api_keys(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let keys = state.control.api_keys.list_keys(app_id.0).await?;
    Ok(Json(serde_json::to_value(keys).unwrap()))
}

pub async fn revoke_api_key(
    State(state): State<AppState>,
    Path(key_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.control.api_keys.revoke_key(key_id).await?;
    Ok(Json(serde_json::json!({"revoked": true})))
}

// ── Audit Log ──

pub async fn list_audit_logs(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = query.per_page.min(200) as i64;
    let offset = ((query.page.saturating_sub(1)) * query.per_page) as i64;

    let logs = aurix_db::queries::list_audit_logs(
        &state.control.pool, Some(app_id.0), limit, offset,
    ).await.map_err(|e| AurixError::Database(e.to_string()))?;

    Ok(Json(serde_json::to_value(logs).unwrap()))
}

// ── Recordings ──

pub async fn get_recording(
    State(state): State<AppState>,
    Path(recording_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let recording = state
        .recording
        .as_ref()
        .ok_or(AurixError::InvalidConfiguration("Recording not enabled".into()))?
        .get_recording(recording_id)
        .await?
        .ok_or(AurixError::Recording("Recording not found".into()))?;

    Ok(Json(serde_json::to_value(recording).unwrap()))
}

// ── TURN Credentials ──

#[derive(Deserialize)]
pub struct TurnCredentialsRequest {
    pub user_id: String,
}

#[derive(Serialize)]
pub struct TurnCredentialsResponse {
    pub username: String,
    pub password: String,
    pub ttl: i64,
    pub uris: Vec<String>,
}

pub async fn get_turn_credentials(
    State(state): State<AppState>,
    Json(req): Json<TurnCredentialsRequest>,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    let turn_config = &state.control.config.turn;
    let ttl = turn_config.allocation_lifetime_secs as i64;
    let timestamp = Utc::now().timestamp() + ttl;
    let username = format!("{}:{}", timestamp, req.user_id);

    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(turn_config.auth_secret.as_bytes())
        .map_err(|_| AurixError::Internal("HMAC key error".into()))?;
    mac.update(username.as_bytes());
    let result = mac.finalize();
    let password = base64::engine::general_purpose::STANDARD.encode(result.into_bytes());

    let uris = vec![
        format!("turn:{}:{}?transport=udp", turn_config.host, turn_config.udp_port),
        format!("turn:{}:{}?transport=tcp", turn_config.host, turn_config.tcp_port),
        format!("stun:{}:{}", turn_config.host, turn_config.udp_port),
    ];

    Ok(Json(TurnCredentialsResponse { username, password, ttl, uris }))
}

// ── Admin Authentication ──

#[derive(Deserialize)]
pub struct AdminLoginRequest {
    pub email: String,
    pub password: String,
}

pub async fn admin_login(
    State(state): State<AppState>,
    Json(req): Json<AdminLoginRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (admin, token) = state.control.admin_auth.authenticate(&req.email, &req.password).await?;
    Ok(Json(serde_json::json!({
        "token": token,
        "admin": {
            "id": admin.id,
            "email": admin.email,
            "display_name": admin.display_name,
            "role": admin.role,
        }
    })))
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
    Json(req): Json<CreateAdminRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let role = req.role.as_deref().unwrap_or("admin");
    let admin = state.control.admin_auth.create_admin(&req.email, &req.password, &req.display_name, role).await?;
    Ok(Json(serde_json::json!({
        "id": admin.id,
        "email": admin.email,
        "display_name": admin.display_name,
        "role": admin.role,
    })))
}

// ── App Management (FIXED: create app FIRST, then API key) ──

#[derive(Deserialize)]
pub struct CreateAppRequest {
    pub name: String,
    pub description: Option<String>,
}

pub async fn create_app(
    State(state): State<AppState>,
    Json(req): Json<CreateAppRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let app_id = Uuid::now_v7();

    // 1. Create the app FIRST (so foreign key exists)
    let app = aurix_db::models::AppRow {
        id: app_id,
        name: req.name,
        description: req.description,
        owner_id: Uuid::nil(),
        api_key_hash: String::new(),
        api_secret_hash: String::new(),
        active: true,
        max_channels: 10000,
        max_participants_per_channel: 256,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let created = aurix_db::queries::create_app(&state.control.pool, &app).await
        .map_err(|e| AurixError::Database(e.to_string()))?;

    // 2. THEN create the API key (now app_id exists in DB)
    let (key_row, raw_key) = state.control.api_keys.create_key(
        app_id,
        "default",
        serde_json::json!({"all": true}),
        100,
        None,
    ).await?;

    // 3. Update app with the key hash
    let _ = aurix_db::queries::update_app_key_hash(&state.control.pool, app_id, &key_row.key_hash).await;

    Ok(Json(serde_json::json!({
        "id": created.id,
        "name": created.name,
        "api_key": raw_key,
    })))
}

pub async fn list_apps(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = query.per_page.min(200) as i64;
    let offset = ((query.page.saturating_sub(1)) * query.per_page) as i64;
    let apps = aurix_db::queries::list_apps(&state.control.pool, limit, offset).await
        .map_err(|e| AurixError::Database(e.to_string()))?;
    let total = aurix_db::queries::count_apps(&state.control.pool).await
        .map_err(|e| AurixError::Database(e.to_string()))?;
    Ok(Json(serde_json::json!({ "data": apps, "total": total })))
}

pub async fn get_app(
    State(state): State<AppState>,
    Path(app_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let app = aurix_db::queries::get_app(&state.control.pool, app_id).await
        .map_err(|e| AurixError::Database(e.to_string()))?
        .ok_or(AurixError::ChannelNotFound(app_id.to_string()))?;
    Ok(Json(serde_json::to_value(app).unwrap()))
}

pub async fn delete_app(
    State(state): State<AppState>,
    Path(app_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    aurix_db::queries::delete_app(&state.control.pool, app_id).await
        .map_err(|e| AurixError::Database(e.to_string()))?;
    Ok(Json(serde_json::json!({"deleted": true})))
}

// ── User Reports ──

#[derive(Deserialize)]
pub struct ReportRequest {
    pub target_user_id: String,
    pub reporter_user_id: String,
    pub reason: String,
    pub evidence: Option<serde_json::Value>,
    pub recording_id: Option<String>,
}

pub async fn report_user(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Json(req): Json<ReportRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = UserId::from_uuid(Uuid::parse_str(&req.target_user_id)
        .map_err(|_| AurixError::Validation("Invalid target_user_id".into()))?);
    let reporter = UserId::from_uuid(Uuid::parse_str(&req.reporter_user_id)
        .map_err(|_| AurixError::Validation("Invalid reporter_user_id".into()))?);
    let rec_id = req.recording_id.as_ref().and_then(|s| Uuid::parse_str(s).ok());

    let event = state.moderation.report_user(
        app_id, None, target, reporter, &req.reason, req.evidence, rec_id,
    ).await?;
    Ok(Json(serde_json::to_value(event).unwrap()))
}

// ── Recording Start/Stop ──

#[derive(Deserialize)]
pub struct StartRecordingRequest {
    pub channel_id: String,
    pub user_id: String,
    pub session_id: String,
}

pub async fn start_recording(
    State(state): State<AppState>,
    Extension(app_id): Extension<AppId>,
    Json(req): Json<StartRecordingRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let recording_svc = state.recording.as_ref()
        .ok_or(AurixError::InvalidConfiguration("Recording not enabled".into()))?;
    let channel_id = ChannelId::from_uuid(Uuid::parse_str(&req.channel_id)
        .map_err(|_| AurixError::Validation("Invalid channel_id".into()))?);
    let user_id = UserId::from_uuid(Uuid::parse_str(&req.user_id)
        .map_err(|_| AurixError::Validation("Invalid user_id".into()))?);
    let session_id = SessionId::from_uuid(Uuid::parse_str(&req.session_id)
        .map_err(|_| AurixError::Validation("Invalid session_id".into()))?);

    let rec = recording_svc.start_recording(app_id, channel_id, session_id, user_id, 48000, 1).await?;

    state.control.events.publish(aurix_control::ServerEvent::RecordingConsentRequired {
        app_id, channel_id, recording_id: rec.id, initiated_by: user_id, timestamp: Utc::now(),
    });
    state.control.events.publish(aurix_control::ServerEvent::RecordingStarted {
        app_id, channel_id, recording_id: rec.id, timestamp: Utc::now(),
    });

    Ok(Json(serde_json::to_value(rec).unwrap()))
}

pub async fn stop_recording(
    State(state): State<AppState>,
    Path(recording_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let recording_svc = state.recording.as_ref()
        .ok_or(AurixError::InvalidConfiguration("Recording not enabled".into()))?;
    recording_svc.stop_recording(recording_id).await?;
    Ok(Json(serde_json::json!({"stopped": true, "recording_id": recording_id})))
}

// ── WebRTC Signaling ──

#[derive(Deserialize)]
pub struct WebRtcOfferRequest {
    pub sdp: String,
    pub user_id: String,
    pub app_id: String,
    pub display_name: String,
}

pub async fn webrtc_offer(
    State(state): State<AppState>,
    Json(req): Json<WebRtcOfferRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = UserId::from_uuid(
        Uuid::parse_str(&req.user_id).map_err(|_| AurixError::Validation("Invalid user_id".into()))?,
    );
    let app_id = AppId::from_uuid(
        Uuid::parse_str(&req.app_id).map_err(|_| AurixError::Validation("Invalid app_id".into()))?,
    );

    let session_id = SessionId::new();

    {
        let sfu = state.sfu.read();
        sfu.create_session(session_id, user_id, app_id, req.display_name)?;
    }

    let answer_sdp = {
        let sfu = state.sfu.read();
        let webrtc = sfu.webrtc_manager()
            .ok_or(AurixError::Internal("WebRTC not available".into()))?;
        webrtc.create_session(&req.sdp, session_id, user_id, app_id)?
    };

    Ok(Json(serde_json::json!({
        "session_id": session_id.0.to_string(),
        "sdp": answer_sdp,
    })))
}

use base64::Engine;