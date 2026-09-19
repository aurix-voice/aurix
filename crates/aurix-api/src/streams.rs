//! Real-time audio streams of a channel to operator services (`/v1/channels/:id/audio/streams`).
//!
//! * `pull`: the operator opens a WebSocket to this node (`…/streams/pull`) and receives
//!   frames; the stream lives as long as the socket.
//! * `push`: `POST …/streams` makes this node connect to the operator's WebSocket URL and
//!   send frames until the stream is deleted, the channel ends or the target stays down.
//!
//! Streams are node-local: they tap the media of the node that receives the request, which
//! in a cascaded channel sees every participant as long as it hosts at least one of them.

use crate::errors::{ApiError, Json, Path, Query};
use crate::middleware::{ApiKeyContext, ClientIp};
use crate::state::AppState;
use aurix_common::error::AurixError;
use aurix_common::types::{AppId, AuditAction, ChannelId, UserId};
use aurix_recording::live::{
    LiveStreamInfo, LiveStreams, Outgoing, PushTarget, StreamFormat, StreamMode, StreamReceiver,
    StreamSpec,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

const MAX_STREAM_USERS: usize = 32;

fn live(state: &AppState) -> Result<Arc<LiveStreams>, ApiError> {
    let svc = state
        .recording
        .as_ref()
        .filter(|r| r.live().enabled())
        .ok_or_else(|| {
            AurixError::InvalidConfiguration(
                "Live audio streams are disabled (recording.live.enabled)".into(),
            )
        })?;
    Ok(svc.live().clone())
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(value).map_err(|_| AurixError::Validation(format!("Invalid {field}")).into())
}

fn client_ip_string(ip: Option<Extension<ClientIp>>) -> Option<String> {
    ip.map(|Extension(c)| c.0.to_string())
}

/// Channel of the caller's tenant plus the participant filter, validated against the same
/// tenant. The channel must be hosted on this node once it has participants.
async fn prepare(
    state: &AppState,
    app_id: AppId,
    channel_id: ChannelId,
    users: Option<Vec<String>>,
) -> Result<Option<Vec<UserId>>, ApiError> {
    state
        .control
        .channels
        .require_channel(app_id, channel_id)
        .await?;
    let users = match users {
        None => None,
        Some(raw) => {
            if raw.is_empty() || raw.len() > MAX_STREAM_USERS {
                return Err(AurixError::Validation(format!(
                    "users must contain 1..={MAX_STREAM_USERS} ids"
                ))
                .into());
            }
            let mut ids = Vec::with_capacity(raw.len());
            for u in &raw {
                let id = UserId::from_uuid(parse_uuid(u, "user id")?);
                aurix_db::queries::get_user(&state.control.pool, app_id.0, id.0)
                    .await
                    .map_err(|e| AurixError::Database(e.to_string()))?
                    .ok_or_else(|| AurixError::UserNotFound(id.to_string()))?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            Some(ids)
        }
    };
    if state.sfu.read().get_channel(&channel_id).is_none() {
        let members = state
            .control
            .sessions
            .get_channel_members(app_id, channel_id)
            .await?;
        if !members.is_empty() {
            return Err(AurixError::Conflict(
                "Channel participants are hosted on another node; open the stream through that node"
                    .into(),
            )
            .into());
        }
    }
    Ok(users)
}

fn parse_format(raw: Option<&str>) -> Result<StreamFormat, ApiError> {
    match raw {
        None | Some("opus") => Ok(StreamFormat::Opus),
        Some("pcm_s16le") | Some("pcm") => Ok(StreamFormat::PcmS16le),
        Some(other) => {
            Err(AurixError::Validation(format!("unknown format {other:?} (opus|pcm_s16le)")).into())
        }
    }
}

fn stream_of_channel(
    info: Option<LiveStreamInfo>,
    channel_id: ChannelId,
    stream_id: Uuid,
) -> Result<LiveStreamInfo, ApiError> {
    info.filter(|i| i.channel_id == channel_id)
        .ok_or_else(|| AurixError::NotFound(format!("Audio stream {stream_id}")).into())
}

#[derive(Deserialize)]
pub struct CreateStreamRequest {
    /// Only `push` can be created through REST; `pull` streams are opened on the WebSocket.
    #[serde(default)]
    pub mode: Option<String>,
    pub url: String,
    #[serde(default)]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub users: Option<Vec<String>>,
    #[serde(default)]
    pub label: Option<String>,
}

pub async fn create_stream(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(channel_id): Path<String>,
    Json(req): Json<CreateStreamRequest>,
) -> Result<Response, ApiError> {
    ctx.require("audio_streams:write")?;
    let live = live(&state)?;
    if !live.config().push_enabled {
        return Err(AurixError::InvalidConfiguration(
            "Push streams are disabled (recording.live.push_enabled)".into(),
        )
        .into());
    }
    if req.mode.as_deref().is_some_and(|m| m != "push") {
        return Err(AurixError::Validation(
            "only mode \"push\" can be created here; pull streams are opened on …/streams/pull"
                .into(),
        )
        .into());
    }
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(parse_uuid(&channel_id, "channel id")?);
    let url = live.validate_push_url(&req.url)?;
    let headers: Vec<(String, String)> = req.headers.unwrap_or_default().into_iter().collect();
    LiveStreams::validate_push_headers(&headers)?;
    let users = prepare(&state, app_id, channel_id, req.users).await?;
    let spec = StreamSpec {
        format: parse_format(req.format.as_deref())?,
        users,
        label: req.label,
    };
    let (info, receiver) = live.open(app_id, channel_id, spec, StreamMode::Push, Some(&url))?;
    live.spawn_push(
        receiver,
        url,
        PushTarget {
            url: req.url,
            headers,
        },
    );
    state.control.audit.log(
        Some(app_id),
        ctx.actor(),
        AuditAction::LiveStreamStarted,
        "audio_stream",
        &info.id.to_string(),
        serde_json::json!({
            "channel_id": channel_id,
            "mode": "push",
            "format": info.format,
            "push_url": info.push_url,
            "users": info.users,
        }),
        client_ip_string(ip),
    );
    Ok((StatusCode::CREATED, axum::Json(info)).into_response())
}

pub async fn list_channel_streams(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path(channel_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ctx.require("audio_streams:read")?;
    let live = live(&state)?;
    let channel_id = ChannelId::from_uuid(parse_uuid(&channel_id, "channel id")?);
    state
        .control
        .channels
        .require_channel(ctx.app_id, channel_id)
        .await?;
    let streams = live.list(ctx.app_id, Some(channel_id));
    Ok(Json(serde_json::json!({ "streams": streams })))
}

pub async fn list_streams(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ctx.require("audio_streams:read")?;
    let live = live(&state)?;
    let streams = live.list(ctx.app_id, None);
    Ok(Json(serde_json::json!({ "streams": streams })))
}

pub async fn get_stream(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    Path((channel_id, stream_id)): Path<(String, String)>,
) -> Result<Json<LiveStreamInfo>, ApiError> {
    ctx.require("audio_streams:read")?;
    let live = live(&state)?;
    let channel_id = ChannelId::from_uuid(parse_uuid(&channel_id, "channel id")?);
    let stream_id = parse_uuid(&stream_id, "stream id")?;
    let info = stream_of_channel(live.get(ctx.app_id, stream_id), channel_id, stream_id)?;
    Ok(Json(info))
}

pub async fn delete_stream(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path((channel_id, stream_id)): Path<(String, String)>,
) -> Result<Json<LiveStreamInfo>, ApiError> {
    ctx.require("audio_streams:write")?;
    let live = live(&state)?;
    let channel_id = ChannelId::from_uuid(parse_uuid(&channel_id, "channel id")?);
    let stream_id = parse_uuid(&stream_id, "stream id")?;
    stream_of_channel(live.get(ctx.app_id, stream_id), channel_id, stream_id)?;
    let info = live
        .close(Some(ctx.app_id), stream_id, "operator")
        .ok_or_else(|| AurixError::NotFound(format!("Audio stream {stream_id}")))?;
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::LiveStreamStopped,
        "audio_stream",
        &stream_id.to_string(),
        serde_json::json!({
            "channel_id": channel_id,
            "frames_sent": info.frames_sent,
            "frames_dropped": info.frames_dropped,
        }),
        client_ip_string(ip),
    );
    Ok(Json(info))
}

#[derive(Deserialize, Default)]
pub struct PullQuery {
    pub format: Option<String>,
    /// Comma-separated user ids.
    pub users: Option<String>,
    pub label: Option<String>,
}

/// `GET …/streams/pull` (WebSocket upgrade). Frames start flowing once the socket is open;
/// closing the socket ends the stream.
pub async fn pull_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(channel_id): Path<String>,
    Query(q): Query<PullQuery>,
) -> Result<Response, ApiError> {
    ctx.require("audio_streams:write")?;
    let live = live(&state)?;
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(parse_uuid(&channel_id, "channel id")?);
    let users = q.users.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let users = prepare(&state, app_id, channel_id, users).await?;
    let spec = StreamSpec {
        format: parse_format(q.format.as_deref())?,
        users,
        label: q.label,
    };
    let (info, receiver) = live.open(app_id, channel_id, spec, StreamMode::Pull, None)?;
    let audit = state.control.audit.clone();
    let actor = ctx.actor();
    let ip = client_ip_string(ip);
    audit.log(
        Some(app_id),
        actor,
        AuditAction::LiveStreamStarted,
        "audio_stream",
        &info.id.to_string(),
        serde_json::json!({
            "channel_id": channel_id,
            "mode": "pull",
            "format": info.format,
            "users": info.users,
        }),
        ip.clone(),
    );
    Ok(ws.on_upgrade(move |socket| async move {
        let stream_id = receiver.id;
        let closed = run_pull(socket, live, receiver).await;
        if let Some(closed) = closed {
            audit.log(
                Some(app_id),
                actor,
                AuditAction::LiveStreamStopped,
                "audio_stream",
                &stream_id.to_string(),
                serde_json::json!({
                    "channel_id": channel_id,
                    "frames_sent": closed.frames_sent,
                    "frames_dropped": closed.frames_dropped,
                }),
                ip,
            );
        }
    }))
}

/// Forward queued frames to the socket until the stream ends or the consumer disconnects.
/// Returns the final status when the disconnect closed the stream (`None` when the stream
/// was already closed server-side, e.g. by DELETE or channel teardown).
async fn run_pull(
    socket: WebSocket,
    live: Arc<LiveStreams>,
    receiver: StreamReceiver,
) -> Option<LiveStreamInfo> {
    let StreamReceiver { id, mut rx } = receiver;
    let (mut sink, mut source) = socket.split();
    loop {
        tokio::select! {
            item = rx.recv() => match item {
                None => {
                    let _ = sink.close().await;
                    return None;
                }
                Some(out) => {
                    let msg = match out {
                        Outgoing::Control(c) => match serde_json::to_string(&c) {
                            Ok(s) => Message::Text(s),
                            Err(_) => continue,
                        },
                        Outgoing::Audio(b) => Message::Binary(b.to_vec()),
                    };
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
            },
            incoming = source.next() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    debug!("Live stream {} consumer disconnected", id);
    live.close(None, id, "consumer_disconnected")
}
