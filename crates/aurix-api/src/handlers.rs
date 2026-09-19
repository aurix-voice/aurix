use crate::errors::{ApiError, Json, Path, Query};
use crate::middleware::{ApiKeyContext, ClientIp};
use crate::state::AppState;
use aurix_auth::ValidatedToken;
use aurix_common::error::AurixError;
use aurix_common::types::*;
use aurix_control::chat::{OutgoingMessage, SYSTEM_USER};
use aurix_control::moderation_actions::{self, ModerationTarget};
use aurix_control::safety::{event_type_for_source, is_safety_event, IncidentExport};
use aurix_control::SelectionHint;
use axum::{
    extract::{Extension, State},
    http::HeaderMap,
    response::IntoResponse,
};
use base64::Engine;
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

/// The OpenAPI 3.1 description of this API (`api/openapi.json` in the repository), served
/// unauthenticated so tooling can be pointed straight at a running node.
pub const OPENAPI_JSON: &str = include_str!("../../../api/openapi.json");

pub async fn openapi() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        OPENAPI_JSON,
    )
}

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
    pub channels: Vec<ChannelGrant>,
    pub metadata: Option<serde_json::Value>,
    /// Region the game wants the player in (matchmaking region); see `endpoint` in the response.
    #[serde(default)]
    pub region: Option<Region>,
    /// Player's approximate coordinates, if the game knows them; used to order regions.
    #[serde(default)]
    pub location: Option<GeoLocation>,
}

/// One channel entry of a token request. Either an existing `channel_id` or an `ad_hoc`
/// template (whose id is derived from the name); both may be given if they agree.
#[derive(Deserialize)]
pub struct ChannelGrant {
    #[serde(default)]
    pub channel_id: Option<ChannelId>,
    #[serde(default = "default_true")]
    pub join: bool,
    #[serde(default = "default_true")]
    pub speak: bool,
    #[serde(default = "default_true")]
    pub receive: bool,
    #[serde(default)]
    pub moderate: bool,
    #[serde(default)]
    pub ad_hoc: Option<AdHocChannel>,
}

/// Turns a grant into the permission carried by the token. Ad-hoc templates are validated
/// and clamped to the app's per-channel limit here so a bad grant fails at the game server.
fn resolve_ad_hoc(
    app_id: AppId,
    channel_id: Option<ChannelId>,
    ad_hoc: Option<AdHocChannel>,
    app: &aurix_db::models::AppRow,
) -> Result<(ChannelId, Option<AdHocChannel>), ApiError> {
    match ad_hoc {
        Some(mut template) => {
            template.name = template.name.trim().to_string();
            let config = aurix_control::channel_manager::ChannelManager::validate_ad_hoc(
                &template,
                app.max_participants_per_channel as u32,
            )?;
            template.max_participants = Some(config.max_participants);
            let derived = template.channel_id(app_id);
            if let Some(given) = channel_id {
                if given != derived {
                    return Err(AurixError::Validation(format!(
                        "channel_id {given} does not match ad_hoc channel '{}' ({derived})",
                        template.name
                    ))
                    .into());
                }
            }
            Ok((derived, Some(template)))
        }
        None => match channel_id {
            Some(id) => Ok((id, None)),
            None => Err(AurixError::Validation(
                "Each channel grant needs channel_id or ad_hoc".into(),
            )
            .into()),
        },
    }
}

async fn load_app(state: &AppState, app_id: AppId) -> Result<aurix_db::models::AppRow, ApiError> {
    Ok(aurix_db::queries::get_app(&state.control.pool, app_id.0)
        .await?
        .ok_or_else(|| AurixError::AuthorizationDenied("Application is inactive".into()))?)
}

#[derive(Serialize)]
pub struct TokenResponse {
    pub token: String,
    pub user_id: UserId,
    pub expires_at: String,
    /// Resolved grants; ad-hoc entries carry the derived `channel_id` the game server can
    /// use for moderation calls before anyone has joined.
    pub channels: Vec<ChannelPermission>,
    /// Recommended node for this player (preferred region → nearest → least loaded). `null`
    /// when no node advertises a public WebSocket URL; clients then use a configured URL.
    pub endpoint: Option<RegionEndpoint>,
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
    if req.location.is_some_and(|loc| !loc.is_valid()) {
        return Err(AurixError::Validation(
            "location.latitude must be within -90..=90 and longitude within -180..=180".into(),
        )
        .into());
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
    let mut channels = Vec::with_capacity(req.channels.len());
    if !req.channels.is_empty() {
        let app = load_app(&state, app_id).await?;
        for grant in req.channels {
            let (channel_id, ad_hoc) =
                resolve_ad_hoc(app_id, grant.channel_id, grant.ad_hoc, &app)?;
            channels.push(ChannelPermission {
                channel_id,
                join: grant.join,
                speak: grant.speak,
                receive: grant.receive,
                moderate: grant.moderate,
                ad_hoc,
            });
        }
    }
    let token = state.control.jwt.generate_token(
        user_id,
        app_id,
        display_name,
        channels.clone(),
        req.metadata,
    )?;
    let expires_at = Utc::now() + Duration::seconds(state.control.config.auth.token_ttl_secs);
    let endpoint = state.control.nodes.recommend(&SelectionHint {
        region: req.region,
        location: req.location,
    });
    Ok(Json(TokenResponse {
        token,
        user_id,
        expires_at: expires_at.to_rfc3339(),
        channels,
        endpoint,
    }))
}

// ── Region discovery ──

#[derive(Deserialize)]
pub struct RegionsQuery {
    #[serde(default)]
    pub region: Option<Region>,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
}

impl RegionsQuery {
    fn hint(&self) -> Result<SelectionHint, ApiError> {
        let location = match (self.latitude, self.longitude) {
            (None, None) => None,
            (Some(latitude), Some(longitude)) => {
                let loc = GeoLocation {
                    latitude,
                    longitude,
                };
                if !loc.is_valid() {
                    return Err(AurixError::Validation(
                        "latitude must be within -90..=90 and longitude within -180..=180".into(),
                    )
                    .into());
                }
                Some(loc)
            }
            _ => {
                return Err(AurixError::Validation(
                    "latitude and longitude must be given together".into(),
                )
                .into())
            }
        };
        Ok(SelectionHint {
            region: self.region,
            location,
        })
    }
}

#[derive(Serialize)]
pub struct RegionsResponse {
    /// Best-first. Clients that can measure RTT should probe `probe_url` of each entry and
    /// connect to the lowest; the order is the server's fallback when they cannot.
    pub regions: Vec<RegionEndpoint>,
    pub recommended: Option<RegionEndpoint>,
}

fn regions_response(state: &AppState, hint: &SelectionHint) -> RegionsResponse {
    let regions = state.control.nodes.regions(hint);
    RegionsResponse {
        recommended: regions.first().cloned(),
        regions,
    }
}

/// Server-to-server region discovery (`tokens:issue`): lets the game backend pick the node
/// before minting a token, or list regions for its own matchmaking. Served from the node
/// registry, which follows the fleet's heartbeats with at most ~15 s of lag.
pub async fn list_regions(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<RegionsQuery>,
) -> Result<Json<RegionsResponse>, ApiError> {
    ctx.require("tokens:issue")?;
    Ok(Json(regions_response(&state, &query.hint()?)))
}

/// Player-facing region discovery: the SDKs call this with the session token, probe every
/// `probe_url` and connect to the fastest node.
pub async fn list_regions_self(
    State(state): State<AppState>,
    Extension(_token): Extension<ValidatedToken>,
    Query(query): Query<RegionsQuery>,
) -> Result<Json<RegionsResponse>, ApiError> {
    Ok(Json(regions_response(&state, &query.hint()?)))
}

/// Subject of `POST /v1/tokens/action`: either an existing user by id or an
/// external identity that is upserted like `POST /v1/tokens` does.
#[derive(Deserialize)]
pub struct ActionTokenRequest {
    pub action: ActionKind,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub target_user_id: Option<String>,
    #[serde(default = "default_true")]
    pub speak: bool,
    #[serde(default = "default_true")]
    pub receive: bool,
    #[serde(default)]
    pub moderate: bool,
    /// `join` only: create the channel on first join instead of requiring it to exist.
    #[serde(default)]
    pub ad_hoc: Option<AdHocChannel>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
pub struct ActionTokenResponse {
    pub token: String,
    pub jti: String,
    pub action: ActionKind,
    pub user_id: UserId,
    pub expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<ChannelId>,
}

async fn ensure_not_banned(
    state: &AppState,
    app_id: AppId,
    user: &aurix_db::models::UserRow,
) -> Result<(), ApiError> {
    if user.is_banned && user.ban_expires_at.map(|t| t > Utc::now()).unwrap_or(true) {
        return Err(AurixError::UserBanned(
            user.ban_reason
                .clone()
                .unwrap_or_else(|| "User is banned".into()),
        )
        .into());
    }
    if state.moderation.is_banned(app_id, UserId(user.id)).await? {
        return Err(AurixError::UserBanned("User is banned".into()).into());
    }
    Ok(())
}

/// Issues a one-time action token. `login`/`join` need `tokens:issue`; `kick`/`mute`/`unmute`
/// additionally need `moderation:write` because the bearer performs a moderation act.
pub async fn generate_action_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Json(req): Json<ActionTokenRequest>,
) -> Result<Json<ActionTokenResponse>, ApiError> {
    ctx.require("tokens:issue")?;
    if req.action.needs_target() {
        ctx.require("moderation:write")?;
    }
    let app_id = ctx.app_id;
    let svc = &state.control.action_tokens;
    let ttl_secs = svc.resolve_ttl(req.ttl_secs)?;

    let user = match (&req.user_id, &req.external_id) {
        (Some(raw), _) => {
            let id = parse_uuid(raw, "user_id")?;
            aurix_db::queries::get_user(&state.control.pool, app_id.0, id)
                .await?
                .ok_or_else(|| AurixError::UserNotFound(raw.clone()))?
        }
        (None, Some(external_id)) => {
            let external_id = external_id.trim();
            if external_id.is_empty() || external_id.len() > 255 {
                return Err(AurixError::Validation(
                    "external_id must be 1..=255 characters".into(),
                )
                .into());
            }
            let display_name = req.display_name.as_deref().unwrap_or("").trim();
            if display_name.is_empty() || display_name.len() > 64 {
                return Err(AurixError::Validation(
                    "display_name must be 1..=64 characters when external_id is used".into(),
                )
                .into());
            }
            let row = aurix_db::models::UserRow {
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
            aurix_db::queries::upsert_user(&state.control.pool, &row).await?
        }
        (None, None) => {
            return Err(
                AurixError::Validation("Either user_id or external_id is required".into()).into(),
            );
        }
    };
    ensure_not_banned(&state, app_id, &user).await?;
    let user_id = UserId(user.id);
    let display_name = req
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(&user.display_name)
        .to_string();

    let requested_channel = match &req.channel_id {
        Some(raw) => Some(ChannelId::from_uuid(parse_uuid(raw, "channel_id")?)),
        None => None,
    };
    let (channel_id, ad_hoc) = match (requested_channel, req.ad_hoc) {
        (_, Some(_)) if req.action != ActionKind::Join => {
            return Err(
                AurixError::Validation("ad_hoc is only valid for action 'join'".into()).into(),
            );
        }
        (given, Some(template)) => {
            let app = load_app(&state, app_id).await?;
            let (id, template) = resolve_ad_hoc(app_id, given, Some(template), &app)?;
            (Some(id), template)
        }
        (Some(id), None) => {
            state.control.channels.require_channel(app_id, id).await?;
            (Some(id), None)
        }
        (None, None) if req.action.needs_channel() => {
            return Err(AurixError::Validation(format!(
                "channel_id is required for action '{}'",
                req.action
            ))
            .into());
        }
        (None, None) => (None, None),
    };
    let target_user_id = match &req.target_user_id {
        Some(raw) => {
            let id = UserId::from_uuid(parse_uuid(raw, "target_user_id")?);
            if id == user_id {
                return Err(
                    AurixError::Validation("target_user_id must differ from user".into()).into(),
                );
            }
            require_user(&state, app_id, id).await?;
            Some(id)
        }
        None if req.action.needs_target() => {
            return Err(AurixError::Validation(format!(
                "target_user_id is required for action '{}'",
                req.action
            ))
            .into());
        }
        None => None,
    };

    let spec = aurix_auth::ActionTokenSpec {
        action: req.action,
        user_id,
        app_id,
        display_name,
        channel_id,
        target_user_id,
        speak: req.speak,
        receive: req.receive,
        moderate: req.moderate,
        ad_hoc,
        metadata: req.metadata,
        ttl_secs,
    };
    let (token, jti, exp) = svc.mint(&spec)?;
    let expires_at = chrono::DateTime::<Utc>::from_timestamp(exp, 0)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default();
    Ok(Json(ActionTokenResponse {
        token,
        jti,
        action: req.action,
        user_id,
        expires_at,
        channel_id,
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
    config
        .validate(state.control.config.media.max_bitrate)
        .map_err(AurixError::Validation)?;
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
    config
        .validate(state.control.config.media.max_bitrate)
        .map_err(AurixError::Validation)?;
    let channel_id = ChannelId::from_uuid(channel_id);
    state
        .control
        .channels
        .update_channel_config(ctx.app_id, channel_id, &config)
        .await?;
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::ChannelConfigUpdated {
            app_id: ctx.app_id,
            channel_id,
            config: config.clone(),
            timestamp: Utc::now(),
        });
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
                    "quality": s.get_network_quality(),
                })
            })
            .collect()
    };
    Ok(Json(
        serde_json::json!({ "channel_id": channel_id, "memberships": members, "live_on_this_node": live }),
    ))
}

/// Media-plane statistics of one live session. Only sessions hosted on this node are
/// visible (the row in `/v1/users/:id` tells which node holds a session).
pub async fn get_session_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(session_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("channels:read")?;
    let session_id = SessionId::from_uuid(session_id);
    let session = {
        let sfu = state.sfu.read();
        sfu.get_session(&session_id)
            .filter(|s| s.app_id == ctx.app_id && s.is_active())
    }
    .ok_or_else(|| AurixError::SessionNotFound(session_id.to_string()))?;
    let stats = session.stats();
    Ok(Json(serde_json::json!({
        "session_id": session.session_id,
        "user_id": session.user_id,
        "transport": format!("{:?}", *session.transport.read()),
        "media_path": session.transport_kind(),
        "channels": session.get_channels(),
        "packets_sent": stats.packets_sent,
        "bytes_sent": stats.bytes_sent,
        "packets_received": stats.packets_received,
        "bytes_received": stats.bytes_received,
        "client_report": stats.quality,
        "quality": session.get_network_quality(),
    })))
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

#[derive(Deserialize, Default)]
pub struct DeleteUserQuery {
    /// Also remove moderation events about the user and their bans (kept by default as
    /// operator evidence).
    #[serde(default)]
    pub purge_moderation: bool,
}

pub async fn delete_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(user_id): Path<Uuid>,
    Query(query): Query<DeleteUserQuery>,
) -> JsonResult {
    ctx.require("users:erase")?;
    let user_id = UserId::from_uuid(user_id);
    let deletion = state
        .control
        .users
        .delete_user(aurix_control::DeleteUserRequest {
            app_id: ctx.app_id,
            user_id,
            actor: ctx.actor(),
            ip: client_ip_string(ip),
            purge_moderation: query.purge_moderation,
            automatic: false,
        })
        .await?
        .ok_or_else(|| AurixError::UserNotFound(user_id.to_string()))?;
    to_json(deletion)
}

pub async fn export_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(user_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("users:export")?;
    let user_id = UserId::from_uuid(user_id);
    let export = state
        .control
        .users
        .export_user(ctx.app_id, user_id, ctx.actor(), client_ip_string(ip))
        .await?
        .ok_or_else(|| AurixError::UserNotFound(user_id.to_string()))?;
    to_json(export)
}

// ── Cross-mute (block list) ──

pub async fn list_user_blocks(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(user_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("users:read")?;
    let user_id = UserId::from_uuid(user_id);
    require_user(&state, ctx.app_id, user_id).await?;
    let blocked = state.control.blocks.list(ctx.app_id, user_id).await?;
    Ok(Json(
        serde_json::json!({ "user_id": user_id, "blocked_users": blocked }),
    ))
}

#[derive(Deserialize)]
pub struct BlockRequest {
    pub blocked_user_id: String,
}

pub async fn add_user_block(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(user_id): Path<Uuid>,
    Json(req): Json<BlockRequest>,
) -> JsonResult {
    ctx.require("users:write")?;
    let user_id = UserId::from_uuid(user_id);
    let target = UserId::from_uuid(parse_uuid(&req.blocked_user_id, "blocked_user_id")?);
    require_user(&state, ctx.app_id, user_id).await?;
    let created = state
        .control
        .blocks
        .block(ctx.app_id, user_id, target)
        .await?;
    publish_block_change(&state, ctx.app_id, user_id, target, true);
    Ok(Json(serde_json::json!({
        "user_id": user_id, "blocked_user_id": target, "blocked": true, "created": created
    })))
}

pub async fn remove_user_block(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path((user_id, blocked_user_id)): Path<(Uuid, Uuid)>,
) -> JsonResult {
    ctx.require("users:write")?;
    let user_id = UserId::from_uuid(user_id);
    let target = UserId::from_uuid(blocked_user_id);
    require_user(&state, ctx.app_id, user_id).await?;
    let removed = state
        .control
        .blocks
        .unblock(ctx.app_id, user_id, target)
        .await?;
    publish_block_change(&state, ctx.app_id, user_id, target, false);
    Ok(Json(serde_json::json!({
        "user_id": user_id, "blocked_user_id": target, "blocked": false, "removed": removed
    })))
}

fn publish_block_change(
    state: &AppState,
    app_id: AppId,
    user_id: UserId,
    blocked_user_id: UserId,
    blocked: bool,
) {
    state
        .control
        .events
        .publish(aurix_control::ServerEvent::UserBlockChanged {
            app_id,
            user_id,
            blocked_user_id,
            blocked,
            timestamp: Utc::now(),
        });
}

// ── Text chat ──

#[derive(Deserialize)]
pub struct ChatHistoryQuery {
    /// Return messages sent strictly before this timestamp (RFC 3339); newest first.
    pub before: Option<chrono::DateTime<Utc>>,
    pub limit: Option<i64>,
}

fn history_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(50).clamp(1, 200)
}

pub async fn list_channel_messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
    Query(query): Query<ChatHistoryQuery>,
) -> JsonResult {
    ctx.require("chat:read")?;
    let channel_id = ChannelId::from_uuid(channel_id);
    state
        .control
        .channels
        .require_channel(ctx.app_id, channel_id)
        .await?;
    let messages = state
        .control
        .chat
        .channel_history(
            ctx.app_id,
            channel_id,
            query.before,
            history_limit(query.limit),
        )
        .await?;
    Ok(Json(serde_json::json!({ "messages": messages })))
}

pub async fn list_user_messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(user_id): Path<Uuid>,
    Query(query): Query<ChatHistoryQuery>,
) -> JsonResult {
    ctx.require("chat:read")?;
    let user_id = UserId::from_uuid(user_id);
    require_user(&state, ctx.app_id, user_id).await?;
    let messages = state
        .control
        .chat
        .user_history(
            ctx.app_id,
            user_id,
            query.before,
            history_limit(query.limit),
        )
        .await?;
    Ok(Json(serde_json::json!({ "messages": messages })))
}

/// Server-originated message (announcements, match events, invites). The sender is the
/// nil `SYSTEM_USER`; `display_name` defaults to "Server".
#[derive(Deserialize)]
pub struct SystemMessageRequest {
    pub text: String,
    pub display_name: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

fn system_display_name(name: Option<String>) -> Result<String, ApiError> {
    let name = name.unwrap_or_else(|| "Server".to_string());
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 64 {
        return Err(AurixError::Validation("display_name must be 1..=64 characters".into()).into());
    }
    Ok(name)
}

pub async fn send_channel_message(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<SystemMessageRequest>,
) -> JsonResult {
    ctx.require("chat:write")?;
    let channel_id = ChannelId::from_uuid(channel_id);
    state
        .control
        .channels
        .require_channel(ctx.app_id, channel_id)
        .await?;
    let message = state
        .control
        .chat
        .accept(OutgoingMessage {
            app_id: ctx.app_id,
            channel_id: Some(channel_id),
            to_user_id: None,
            from_user_id: SYSTEM_USER,
            display_name: system_display_name(req.display_name)?,
            text: req.text,
            metadata: req.metadata,
            client_ref: None,
            from_session_id: None,
        })
        .await?;
    to_json(message)
}

pub async fn send_user_message(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(user_id): Path<Uuid>,
    Json(req): Json<SystemMessageRequest>,
) -> JsonResult {
    ctx.require("chat:write")?;
    let user_id = UserId::from_uuid(user_id);
    require_user(&state, ctx.app_id, user_id).await?;
    let message = state
        .control
        .chat
        .accept(OutgoingMessage {
            app_id: ctx.app_id,
            channel_id: None,
            to_user_id: Some(user_id),
            from_user_id: SYSTEM_USER,
            display_name: system_display_name(req.display_name)?,
            text: req.text,
            metadata: req.metadata,
            client_ref: None,
            from_session_id: None,
        })
        .await?;
    to_json(message)
}

// ── Text-to-speech ──

#[derive(Deserialize)]
pub struct AnnounceRequest {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
}

/// Server announcement spoken into a channel with the configured TTS provider. Every node
/// hosting participants of the channel plays it to them; progress is published as
/// `tts.status` events (SSE/webhooks) under the returned `request_id`.
pub async fn announce_in_channel(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<AnnounceRequest>,
) -> JsonResult {
    ctx.require("tts:write")?;
    let channel_id = ChannelId::from_uuid(channel_id);
    state
        .control
        .channels
        .require_channel(ctx.app_id, channel_id)
        .await?;
    let request_id =
        state
            .control
            .speech
            .announce(ctx.app_id, channel_id, &req.text, req.voice.as_deref())?;
    Ok(Json(serde_json::json!({
        "request_id": request_id,
        "channel_id": channel_id,
        "state": aurix_common::protocol::TtsState::Queued,
    })))
}

/// Voices offered by the TTS configuration (first is the default); `enabled: false` when TTS
/// is not configured on this node.
pub async fn tts_voices(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> JsonResult {
    ctx.require("channels:read")?;
    let speech = &state.control.speech;
    Ok(Json(serde_json::json!({
        "enabled": speech.enabled(),
        "client_requests": speech.enabled() && speech.config().allow_client_requests,
        "voices": if speech.enabled() { speech.voices() } else { Vec::new() },
        "max_text_chars": speech.config().max_text_chars,
        "max_audio_secs": speech.config().max_audio_secs,
    })))
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
    moderation_actions::set_server_mute(
        &state.control,
        &state.sfu,
        ModerationTarget {
            app_id,
            channel_id,
            user_id,
            actor,
            ip: client_ip_string(ip),
        },
        req.muted,
    )
    .await?;
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
    let removed = moderation_actions::kick_from_channel(
        &state.control,
        &state.sfu,
        ModerationTarget {
            app_id,
            channel_id,
            user_id,
            actor,
            ip: client_ip_string(ip),
        },
        req.reason,
    )
    .await?;
    Ok(Json(
        serde_json::json!({"kicked": true, "memberships_closed": removed}),
    ))
}

const MAX_BULK_EXCEPT: usize = 256;

#[derive(Deserialize)]
pub struct MuteAllRequest {
    pub channel_id: String,
    pub muted: bool,
    #[serde(default)]
    pub except: Vec<String>,
    pub moderator_user_id: Option<String>,
}

#[derive(Deserialize)]
pub struct KickAllRequest {
    pub channel_id: String,
    pub reason: String,
    #[serde(default)]
    pub except: Vec<String>,
    pub moderator_user_id: Option<String>,
}

fn parse_except(raw: &[String]) -> Result<Vec<UserId>, ApiError> {
    if raw.len() > MAX_BULK_EXCEPT {
        return Err(AurixError::Validation(format!(
            "except may list at most {MAX_BULK_EXCEPT} users"
        ))
        .into());
    }
    raw.iter()
        .map(|r| Ok(UserId::from_uuid(parse_uuid(r, "except")?)))
        .collect()
}

/// Server-mutes/unmutes everyone currently in the channel except the listed users. Only the
/// present participants are affected; later joiners come in unmuted.
pub async fn mute_all(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<MuteAllRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(parse_uuid(&req.channel_id, "channel_id")?);
    state
        .control
        .channels
        .require_channel(app_id, channel_id)
        .await?;
    let except = parse_except(&req.except)?;
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;
    let outcome = moderation_actions::set_server_mute_all(
        &state.control,
        &state.sfu,
        moderation_actions::BulkTarget {
            app_id,
            channel_id,
            actor,
            ip: client_ip_string(ip),
            except,
        },
        req.muted,
    )
    .await?;
    Ok(Json(serde_json::json!({
        "muted": req.muted,
        "affected": outcome.affected,
        "skipped": outcome.skipped,
        "failed": outcome.failed,
    })))
}

/// Removes everyone currently in the channel except the listed users. Unlike deleting the
/// channel, the channel itself (and its configuration) stays.
pub async fn kick_all(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Json(req): Json<KickAllRequest>,
) -> JsonResult {
    ctx.require("moderation:write")?;
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(parse_uuid(&req.channel_id, "channel_id")?);
    state
        .control
        .channels
        .require_channel(app_id, channel_id)
        .await?;
    let reason = req.reason.trim();
    if reason.is_empty() || reason.len() > 512 {
        return Err(AurixError::Validation("reason must be 1..=512 characters".into()).into());
    }
    let except = parse_except(&req.except)?;
    let actor = resolve_actor(&state, &ctx, req.moderator_user_id.as_deref()).await?;
    let outcome = moderation_actions::kick_all(
        &state.control,
        &state.sfu,
        moderation_actions::BulkTarget {
            app_id,
            channel_id,
            actor,
            ip: client_ip_string(ip),
            except,
        },
        reason.to_string(),
    )
    .await?;
    Ok(Json(serde_json::json!({
        "kicked": outcome.affected.len(),
        "affected": outcome.affected,
        "skipped": outcome.skipped,
        "failed": outcome.failed,
    })))
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

// ── Content safety ──

#[derive(Deserialize)]
pub struct SafetyIncidentsQuery {
    pub user_id: Option<String>,
    /// `voice` or `text`.
    pub source: Option<String>,
    pub status: Option<String>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

/// Incidents produced by the safety pipeline (a filtered view of the moderation events).
pub async fn list_safety_incidents(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Query(query): Query<SafetyIncidentsQuery>,
) -> JsonResult {
    ctx.require("moderation:read")?;
    let user_id = query
        .user_id
        .as_deref()
        .map(|u| parse_uuid(u, "user_id"))
        .transpose()?;
    let event_type = match query.source.as_deref() {
        None => None,
        Some(s) => Some(
            event_type_for_source(s)
                .ok_or_else(|| AurixError::Validation("source must be 'voice' or 'text'".into()))?,
        ),
    };
    let (limit, offset) = paging(query.page, query.per_page);
    let rows = aurix_db::queries::list_safety_incidents(
        &state.control.pool,
        ctx.app_id.0,
        user_id,
        event_type,
        query.status.as_deref(),
        limit,
        offset,
    )
    .await
    .map_err(|e| AurixError::Database(format!("Safety incident list failed: {e}")))?;
    to_json(rows)
}

async fn load_safety_incident(
    state: &AppState,
    app_id: AppId,
    incident_id: Uuid,
) -> Result<aurix_db::models::ModerationEventRow, ApiError> {
    let row = aurix_db::queries::get_moderation_event(&state.control.pool, app_id.0, incident_id)
        .await
        .map_err(|e| AurixError::Database(format!("Safety incident lookup failed: {e}")))?
        .filter(is_safety_event)
        .ok_or_else(|| AurixError::NotFound("Safety incident not found".into()))?;
    Ok(row)
}

/// The incident with its context and evidence-clip metadata (the clip itself is fetched
/// through `GET /v1/recordings/{id}/download` or included by `/export`).
pub async fn get_safety_incident(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(incident_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("moderation:read")?;
    let row = load_safety_incident(&state, ctx.app_id, incident_id).await?;
    let recording = match (row.recording_id, state.recording.as_deref()) {
        (Some(rid), Some(svc)) => svc.get_recording(ctx.app_id, rid).await?,
        _ => None,
    };
    to_json(IncidentExport::from_row(row, recording, Utc::now()))
}

/// Self-contained evidence bundle for hand-off to a trust & safety team: the incident, the
/// surrounding messages and — when one exists and the key may read recordings — the decrypted
/// audio clip inline (`audio.content_base64`, Ogg/Opus). Access to the clip is audited like a
/// recording download.
pub async fn export_safety_incident(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(incident_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("moderation:read")?;
    let row = load_safety_incident(&state, ctx.app_id, incident_id).await?;
    let mut clip: Option<Vec<u8>> = None;
    let recording = match (row.recording_id, state.recording.as_deref()) {
        (Some(rid), Some(svc)) => {
            let rec = svc.get_recording(ctx.app_id, rid).await?;
            if rec.is_some() && ctx.has("recordings:read") {
                match svc.read_recording(ctx.app_id, rid).await {
                    Ok((_, bytes)) => {
                        state.control.audit.log(
                            Some(ctx.app_id),
                            ctx.actor(),
                            AuditAction::RecordingAccessed,
                            "recording",
                            &rid.to_string(),
                            serde_json::json!({"bytes": bytes.len(), "incident_id": incident_id}),
                            client_ip_string(ip),
                        );
                        clip = Some(bytes);
                    }
                    Err(AurixError::NotFound(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            rec
        }
        _ => None,
    };
    let export = IncidentExport::from_row(row, recording, Utc::now());
    let mut body = serde_json::to_value(&export)
        .map_err(|e| AurixError::Internal(format!("export serialization: {e}")))?;
    if let (Some(bytes), Some(audio)) = (clip, body.get_mut("audio")) {
        audio["content_base64"] =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    Ok(Json(body))
}

/// Current decayed risk of a user in this app.
pub async fn get_safety_user_risk(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(user_id): Path<Uuid>,
) -> JsonResult {
    ctx.require("moderation:read")?;
    let user = aurix_db::queries::get_user(&state.control.pool, ctx.app_id.0, user_id)
        .await
        .map_err(|e| AurixError::Database(format!("User lookup failed: {e}")))?
        .ok_or_else(|| AurixError::NotFound("User not found".into()))?;
    let snapshot = state
        .control
        .safety
        .user_risk(ctx.app_id, UserId(user.id))
        .await?;
    to_json(snapshot)
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
/// Every permission an API key can carry; `ctx.require(...)` calls must use one of these,
/// otherwise scoped keys could never be granted the right (see `tests/openapi_contract.rs`).
pub const KNOWN_PERMISSIONS: &[&str] = &[
    "*",
    "tokens:issue",
    "turn:issue",
    "channels:read",
    "channels:write",
    "users:read",
    "users:write",
    "users:erase",
    "users:export",
    "moderation:read",
    "moderation:write",
    "recordings:read",
    "recordings:write",
    "audio_streams:read",
    "audio_streams:write",
    "chat:read",
    "chat:write",
    "tts:write",
    "keys:manage",
    "analytics:read",
    "audit:read",
    "webhooks:read",
    "webhooks:write",
    "events:read",
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

/// Runs one retention sweep now (also when the periodic sweep is disabled). `409` while
/// another node holds the sweep lock.
pub async fn admin_retention_sweep(
    State(state): State<AppState>,
    Extension(admin): Extension<AdminContext>,
) -> JsonResult {
    if admin.role != "superadmin" {
        return Err(AurixError::AuthorizationDenied(
            "Only superadmins can run retention sweeps".into(),
        )
        .into());
    }
    let report = state.control.retention.sweep_once().await?.ok_or_else(|| {
        AurixError::Conflict("A retention sweep is already running on another node".into())
    })?;
    to_json(report)
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
