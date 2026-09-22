//! Real-time audio streams of a channel to operator services (`/v1/channels/:id/audio/streams`).
//!
//! * `pull`: the operator opens a WebSocket to this node (`…/streams/pull`) and receives
//!   frames; the stream lives as long as the socket.
//! * `push`: `POST …/streams` makes this node connect to the operator's WebSocket URL and
//!   send frames until the stream is deleted, the channel ends or the target stays down.
//!
//! A stream is owned by the node that opened it: that node pins the channel so the cascade
//! forwards every participant's audio to it (also when it hosts none of them), taps the
//! frames and publishes the stream to the fleet directory (`live_streams`). Any node lists,
//! reads and stops the fleet's streams for a tenant, and a pull consumer that reconnects to
//! another node (a load balancer in front of the fleet) is piped to the owner: the node
//! opens the owner's `…/pull?resume=` with the caller's credentials and relays the socket.

use crate::errors::{ApiError, Json, Path, Query};
use crate::middleware::{ApiKeyContext, ClientIp};
use crate::state::AppState;
use aurix_common::error::AurixError;
use aurix_common::types::{AppId, AuditAction, ChannelId, MediaNodeId, UserId};
use aurix_control::channel_manager::ChannelManager;
use aurix_recording::live::{
    LiveStreamInfo, LiveStreams, Outgoing, PushTarget, StreamFormat, StreamMode, StreamReceiver,
    StreamSpec,
};
use aurix_recording::live_directory;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as UpstreamMessage;
use tracing::{debug, warn};
use uuid::Uuid;

const MAX_STREAM_USERS: usize = 32;
const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Request headers forwarded to the owner node so it authorizes the caller itself.
const FORWARDED_HEADERS: [&str; 3] = [
    "x-api-key",
    "authorization",
    crate::middleware::ADMIN_APP_HEADER,
];

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
/// tenant. Returns the channel's media configuration (needed to pin it on this node).
async fn prepare(
    state: &AppState,
    app_id: AppId,
    channel_id: ChannelId,
    users: Option<Vec<String>>,
) -> Result<(aurix_common::types::ChannelConfig, Option<Vec<UserId>>), ApiError> {
    let row = state
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
    Ok((ChannelManager::config_from_row(&row), users))
}

/// Opens the tap with the channel pinned on this node for the stream's lifetime (the pin is
/// released by the lifecycle task on the stream's `Closed` notice).
#[allow(clippy::too_many_arguments)]
fn open_pinned(
    state: &AppState,
    live: &LiveStreams,
    app_id: AppId,
    channel_id: ChannelId,
    config: aurix_common::types::ChannelConfig,
    spec: StreamSpec,
    mode: StreamMode,
    push_url: Option<&url::Url>,
) -> Result<(LiveStreamInfo, StreamReceiver), ApiError> {
    state.sfu.read().pin_channel(channel_id, app_id, config)?;
    match live.open(app_id, channel_id, spec, mode, push_url) {
        Ok(opened) => Ok(opened),
        Err(e) => {
            state.sfu.read().unpin_channel(&channel_id);
            Err(e.into())
        }
    }
}

fn parse_users(raw: Option<String>) -> Option<Vec<String>> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    })
}

/// Streams of the tenant owned by other (healthy) nodes, from the fleet directory.
async fn remote_streams(
    state: &AppState,
    live: &LiveStreams,
    app_id: AppId,
    channel_id: Option<ChannelId>,
) -> Result<Vec<LiveStreamInfo>, ApiError> {
    let rows = aurix_db::queries::list_remote_live_streams(
        &state.control.pool,
        app_id.0,
        channel_id.map(|c| c.0),
        live.node_id(),
    )
    .await
    .map_err(|e| AurixError::Database(e.to_string()))?;
    Ok(rows
        .into_iter()
        .filter_map(live_directory::info_of)
        .collect())
}

/// One stream of the tenant: this node's live status, else the directory entry of another
/// node's stream. Directory rows that name this node are stale (a restart) and ignored.
async fn find_stream(
    state: &AppState,
    live: &LiveStreams,
    app_id: AppId,
    stream_id: Uuid,
) -> Result<Option<LiveStreamInfo>, ApiError> {
    if let Some(local) = live.get(app_id, stream_id) {
        return Ok(Some(local));
    }
    let row = aurix_db::queries::get_live_stream(&state.control.pool, app_id.0, stream_id)
        .await
        .map_err(|e| AurixError::Database(e.to_string()))?;
    Ok(row
        .filter(|r| r.node_id != live.node_id())
        .and_then(live_directory::info_of))
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
    /// One server-side mix of the selected participants instead of per-participant frames.
    #[serde(default)]
    pub mix: bool,
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
    let (config, users) = prepare(&state, app_id, channel_id, req.users).await?;
    let spec = StreamSpec {
        format: parse_format(req.format.as_deref())?,
        users,
        label: req.label,
        mix: req.mix,
    };
    let (info, receiver) = open_pinned(
        &state,
        &live,
        app_id,
        channel_id,
        config,
        spec,
        StreamMode::Push,
        Some(&url),
    )?;
    live_directory::publish(&state.control.pool, &info).await;
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
            "mix": info.mix,
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
    let mut streams = live.list(ctx.app_id, Some(channel_id));
    streams.extend(remote_streams(&state, &live, ctx.app_id, Some(channel_id)).await?);
    Ok(Json(serde_json::json!({ "streams": streams })))
}

pub async fn list_streams(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ctx.require("audio_streams:read")?;
    let live = live(&state)?;
    let mut streams = live.list(ctx.app_id, None);
    streams.extend(remote_streams(&state, &live, ctx.app_id, None).await?);
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
    let info = stream_of_channel(
        find_stream(&state, &live, ctx.app_id, stream_id).await?,
        channel_id,
        stream_id,
    )?;
    Ok(Json(info))
}

/// `DELETE`: a stream owned by this node is closed synchronously (200 with its final status);
/// one owned by another node is asked to stop through the control plane (202 with its last
/// published status) and disappears from the directory once that node has closed it.
pub async fn delete_stream(
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path((channel_id, stream_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    ctx.require("audio_streams:write")?;
    let live = live(&state)?;
    let channel_id = ChannelId::from_uuid(parse_uuid(&channel_id, "channel id")?);
    let stream_id = parse_uuid(&stream_id, "stream id")?;
    let found = stream_of_channel(
        find_stream(&state, &live, ctx.app_id, stream_id).await?,
        channel_id,
        stream_id,
    )?;
    let (status, info) = if found.node_id == live.node_id() {
        let info = live
            .close(Some(ctx.app_id), stream_id, "operator")
            .ok_or_else(|| AurixError::NotFound(format!("Audio stream {stream_id}")))?;
        (StatusCode::OK, info)
    } else {
        state
            .control
            .events
            .publish(aurix_control::ServerEvent::LiveStreamStopRequested {
                app_id: ctx.app_id,
                stream_id,
                node: MediaNodeId::from_uuid(found.node_id),
                reason: "operator".into(),
            });
        (StatusCode::ACCEPTED, found)
    };
    state.control.audit.log(
        Some(ctx.app_id),
        ctx.actor(),
        AuditAction::LiveStreamStopped,
        "audio_stream",
        &stream_id.to_string(),
        serde_json::json!({
            "channel_id": channel_id,
            "node_id": info.node_id,
            "frames_sent": info.frames_sent,
            "frames_dropped": info.frames_dropped,
        }),
        client_ip_string(ip),
    );
    Ok((status, axum::Json(info)).into_response())
}

#[derive(Deserialize, Default)]
pub struct PullQuery {
    pub format: Option<String>,
    /// Comma-separated user ids.
    pub users: Option<String>,
    pub label: Option<String>,
    #[serde(default)]
    pub mix: bool,
    /// Id of a pull stream whose consumer disconnected: re-attach to it (the buffered frames
    /// are replayed after a fresh `hello`) instead of opening a new stream. Other options are
    /// ignored; the stream keeps its original format/users/mix.
    pub resume: Option<String>,
}

/// `GET …/streams/pull` (WebSocket upgrade). Frames start flowing once the socket is open;
/// when the socket closes the stream is kept for `recording.live.outage_buffer_ms` so the
/// consumer can `?resume=<id>` (then it ends).
pub async fn pull_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(ctx): Extension<ApiKeyContext>,
    ip: Option<Extension<ClientIp>>,
    Path(channel_id): Path<String>,
    Query(q): Query<PullQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ctx.require("audio_streams:write")?;
    let live = live(&state)?;
    let app_id = ctx.app_id;
    let channel_id = ChannelId::from_uuid(parse_uuid(&channel_id, "channel id")?);
    let audit = state.control.audit.clone();
    let actor = ctx.actor();
    let ip = client_ip_string(ip);
    if let Some(resume) = q.resume.as_deref() {
        let stream_id = parse_uuid(resume, "stream id")?;
        let found = stream_of_channel(
            find_stream(&state, &live, app_id, stream_id).await?,
            channel_id,
            stream_id,
        )?;
        if found.node_id != live.node_id() {
            let target = owner_pull_url(&state, found.node_id, channel_id, stream_id)?;
            let credentials = forwarded_credentials(&headers);
            return Ok(ws.on_upgrade(move |socket| async move {
                proxy_pull(socket, stream_id, target, credentials).await;
            }));
        }
        let (info, receiver, replay) = live.resume_pull(app_id, stream_id).await?;
        live_directory::publish(&state.control.pool, &info).await;
        return Ok(ws.on_upgrade(move |socket| async move {
            if let Some(closed) = run_pull(socket, live, receiver, replay).await {
                log_stopped(&audit, app_id, actor, channel_id, &closed, ip);
            }
        }));
    }
    let (config, users) = prepare(&state, app_id, channel_id, parse_users(q.users)).await?;
    let spec = StreamSpec {
        format: parse_format(q.format.as_deref())?,
        users,
        label: q.label,
        mix: q.mix,
    };
    let (info, receiver) = open_pinned(
        &state,
        &live,
        app_id,
        channel_id,
        config,
        spec,
        StreamMode::Pull,
        None,
    )?;
    live_directory::publish(&state.control.pool, &info).await;
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
            "mix": info.mix,
            "users": info.users,
        }),
        ip.clone(),
    );
    Ok(ws.on_upgrade(move |socket| async move {
        if let Some(closed) = run_pull(socket, live, receiver, Vec::new()).await {
            log_stopped(&audit, app_id, actor, channel_id, &closed, ip);
        }
    }))
}

fn log_stopped(
    audit: &aurix_common::audit::AuditLogger,
    app_id: AppId,
    actor: UserId,
    channel_id: ChannelId,
    closed: &LiveStreamInfo,
    ip: Option<String>,
) {
    audit.log(
        Some(app_id),
        actor,
        AuditAction::LiveStreamStopped,
        "audio_stream",
        &closed.id.to_string(),
        serde_json::json!({
            "channel_id": channel_id,
            "frames_sent": closed.frames_sent,
            "frames_dropped": closed.frames_dropped,
            "reconnects": closed.reconnects,
        }),
        ip,
    );
}

/// The owner node's own pull endpoint for `stream_id`, derived from its advertised REST URL.
fn owner_pull_url(
    state: &AppState,
    owner: Uuid,
    channel_id: ChannelId,
    stream_id: Uuid,
) -> Result<url::Url, ApiError> {
    let node = state
        .control
        .nodes
        .get_node(&MediaNodeId::from_uuid(owner))
        .filter(|n| n.healthy)
        .ok_or_else(|| {
            AurixError::Conflict(format!(
                "Audio stream {stream_id} is served by node {owner}, which is not reachable"
            ))
        })?;
    let api_url = node.api_url.as_deref().ok_or_else(|| {
        AurixError::Conflict(format!(
            "Audio stream {stream_id} is served by node {owner}, which advertises no \
             server.external_url; resume it through that node"
        ))
    })?;
    pull_url_of(api_url, owner, channel_id, stream_id)
}

fn pull_url_of(
    api_url: &str,
    owner: Uuid,
    channel_id: ChannelId,
    stream_id: Uuid,
) -> Result<url::Url, ApiError> {
    let mut url = url::Url::parse(api_url)
        .map_err(|e| AurixError::InvalidConfiguration(format!("node {owner} api_url: {e}")))?;
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => {
            return Err(AurixError::InvalidConfiguration(format!(
                "node {owner} api_url has scheme {other:?}"
            ))
            .into())
        }
    };
    url.set_scheme(scheme)
        .map_err(|_| AurixError::InvalidConfiguration(format!("node {owner} api_url")))?;
    let base = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!(
        "{base}/v1/channels/{channel_id}/audio/streams/pull"
    ));
    url.set_query(Some(&format!("resume={stream_id}")));
    url.set_fragment(None);
    Ok(url)
}

fn forwarded_credentials(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    FORWARDED_HEADERS
        .iter()
        .filter_map(|name| {
            let name = HeaderName::from_static(name);
            let value = headers.get(&name)?.clone();
            Some((name, value))
        })
        .collect()
}

/// Relay `socket` to the owner node's pull endpoint. Nothing is buffered here beyond the two
/// sockets' own send windows: a consumer that cannot keep up stalls the relay, and the owner
/// applies its per-stream queue/drop accounting exactly as for a direct consumer. The owner
/// authorizes the forwarded credentials itself; its handshake failure (401/404/409…) closes
/// the consumer's socket with the status in the close reason.
async fn proxy_pull(
    mut socket: WebSocket,
    stream_id: Uuid,
    target: url::Url,
    credentials: Vec<(HeaderName, HeaderValue)>,
) {
    let request = match target.as_str().into_client_request() {
        Ok(mut request) => {
            for (name, value) in credentials {
                request.headers_mut().insert(name, value);
            }
            request
        }
        Err(e) => {
            warn!("Live stream {stream_id} proxy: invalid owner URL: {e}");
            let _ = socket.send(close_frame(1011, "owner url")).await;
            return;
        }
    };
    let upstream = match tokio::time::timeout(
        PROXY_CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            debug!("Live stream {stream_id} proxy: owner refused: {e}");
            let reason = match &e {
                tokio_tungstenite::tungstenite::Error::Http(resp) => {
                    format!("owner responded {}", resp.status())
                }
                _ => "owner unreachable".to_string(),
            };
            let _ = socket.send(close_frame(1011, &reason)).await;
            return;
        }
        Err(_) => {
            debug!("Live stream {stream_id} proxy: owner connect timed out");
            let _ = socket.send(close_frame(1011, "owner timeout")).await;
            return;
        }
    };
    let (mut down_tx, mut down_rx) = socket.split();
    let (mut up_tx, mut up_rx) = upstream.split();
    loop {
        tokio::select! {
            item = up_rx.next() => match item {
                Some(Ok(UpstreamMessage::Text(t))) => {
                    if down_tx.send(Message::Text(t)).await.is_err() { break; }
                }
                Some(Ok(UpstreamMessage::Binary(b))) => {
                    if down_tx.send(Message::Binary(b)).await.is_err() { break; }
                }
                Some(Ok(UpstreamMessage::Ping(p))) => {
                    if up_tx.send(UpstreamMessage::Pong(p)).await.is_err() { break; }
                }
                Some(Ok(UpstreamMessage::Close(frame))) => {
                    let _ = down_tx
                        .send(Message::Close(frame.map(|f| CloseFrame {
                            code: f.code.into(),
                            reason: f.reason.into_owned().into(),
                        })))
                        .await;
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    let _ = down_tx.send(close_frame(1011, "owner disconnected")).await;
                    return;
                }
            },
            item = down_rx.next() => match item {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(Message::Ping(p))) => {
                    if down_tx.send(Message::Pong(p)).await.is_err() { break; }
                }
                Some(Ok(_)) => {}
            },
        }
    }
    debug!("Live stream {stream_id} proxy: consumer disconnected");
    let _ = up_tx.send(UpstreamMessage::Close(None)).await;
}

fn close_frame(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.to_string().into(),
    }))
}

/// Forward `replay` and then the queued frames to the socket until the stream ends or the
/// consumer disconnects. A disconnect parks the stream for the outage window (or closes it
/// when buffering is off). Returns the final status when the disconnect closed the stream
/// (`None` when it was parked or already closed server-side, e.g. by DELETE or teardown).
async fn run_pull(
    socket: WebSocket,
    live: Arc<LiveStreams>,
    receiver: StreamReceiver,
    replay: Vec<Outgoing>,
) -> Option<LiveStreamInfo> {
    let id = receiver.id;
    let (mut sink, mut source) = socket.split();
    let to_message = |out: Outgoing| match out {
        Outgoing::Control(c) => serde_json::to_string(&c).ok().map(Message::Text),
        Outgoing::Audio(b) => Some(Message::Binary(b.to_vec())),
    };
    for out in replay {
        if let Some(msg) = to_message(out) {
            if sink.send(msg).await.is_err() {
                debug!("Live stream {} consumer disconnected during replay", id);
                return live.detach_pull(receiver, "consumer_disconnected");
            }
        }
    }
    let StreamReceiver { id, mut rx } = receiver;
    loop {
        tokio::select! {
            item = rx.recv() => match item {
                None => {
                    let _ = sink.close().await;
                    return None;
                }
                Some(out) => {
                    let Some(msg) = to_message(out) else { continue };
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
    live.detach_pull(StreamReceiver { id, rx }, "consumer_disconnected")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_pull_url_derives_from_api_url() {
        let owner = Uuid::nil();
        let ch = ChannelId::from_uuid(Uuid::from_u128(1));
        let id = Uuid::from_u128(2);
        let url = pull_url_of("https://eu1.example", owner, ch, id).unwrap();
        assert_eq!(
            url.as_str(),
            format!("wss://eu1.example/v1/channels/{ch}/audio/streams/pull?resume={id}")
        );
        let url = pull_url_of("http://10.0.0.5:8080/aurix/", owner, ch, id).unwrap();
        assert_eq!(
            url.as_str(),
            format!("ws://10.0.0.5:8080/aurix/v1/channels/{ch}/audio/streams/pull?resume={id}")
        );
        assert!(pull_url_of("ftp://x", owner, ch, id).is_err());
        assert!(pull_url_of("not a url", owner, ch, id).is_err());
    }
}
