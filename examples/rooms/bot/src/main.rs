//! Aurix Rooms demo bot.
//!
//! One native `aurix-client` session (the same core the game SDKs ship) that sits in a room and
//! plays a playlist — music, speech, ambience and synthetic test signals — through the regular
//! uplink, so anybody joining hears exactly what a player would. The Rooms backend mints its
//! token (`POST /api/bot/session`, shared secret) and shows the "now playing" card it reports
//! (`PUT /api/bot/status`). Chat commands from the room: `!np`, `!next`, `!list`.

mod playlist;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use aurix_client::{
    ChannelId, Client, ClientConfig, ConnectionState, DspConfig, EncoderSettings, Event,
    OpusSignal, UserId,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use playlist::{Kind, Track, TrackInfo, FRAME_SAMPLES, SAMPLE_RATE};

const STATUS_INTERVAL: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(20);

#[derive(Parser, Debug)]
#[command(name = "aurix-rooms-bot", version, about)]
struct Args {
    /// Rooms backend (the one that owns the bot secret and talks to the Aurix node).
    #[arg(long, env = "ROOMS_URL", default_value = "http://127.0.0.1:8090")]
    rooms_url: String,
    /// Shared secret configured as `ROOMS_BOT_TOKEN` on the backend.
    #[arg(long, env = "ROOMS_BOT_TOKEN", hide_env_values = true)]
    bot_token: String,
    /// Room slug; defaults to the backend's stage room.
    #[arg(long, env = "ROOMS_BOT_ROOM")]
    room: Option<String>,
    #[arg(long, env = "ROOMS_BOT_NAME", default_value = "Aurix Bot")]
    name: String,
    /// Directory with `playlist.json` from `fetch-assets.sh`; without it only the built-in
    /// test signals play.
    #[arg(long, env = "ROOMS_BOT_ASSETS")]
    assets: Option<PathBuf>,
    /// Node WebSocket URL override (the backend normally hands out the right one).
    #[arg(long, env = "ROOMS_BOT_WS_URL")]
    ws_url: Option<String>,
    /// Silence between tracks.
    #[arg(long, default_value_t = 1500)]
    gap_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    token: String,
    user_id: UserId,
    expires_at: chrono::DateTime<chrono::Utc>,
    channel_id: ChannelId,
    room: String,
    ws_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    room: String,
    track: TrackInfo,
    position_ms: u64,
    up_next: Vec<TrackInfo>,
}

struct Backend {
    http: reqwest::Client,
    base: String,
    bot_token: String,
}

impl Backend {
    async fn session(&self, room: Option<&str>, name: &str) -> Result<Session> {
        let body = serde_json::json!({ "room": room, "name": name });
        let res = self
            .http
            .post(format!("{}/api/bot/session", self.base))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .context("bot session request")?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            return Err(anyhow!("bot session refused: {status} {text}"));
        }
        res.json().await.context("bot session body")
    }

    async fn status(&self, status: &Status) -> Result<()> {
        let res = self
            .http
            .put(format!("{}/api/bot/status", self.base))
            .bearer_auth(&self.bot_token)
            .json(status)
            .send()
            .await
            .context("bot status request")?;
        if !res.status().is_success() {
            return Err(anyhow!("bot status refused: {}", res.status()));
        }
        Ok(())
    }
}

/// Where the playlist stands: which track, which 20 ms frame of it, or how much of the gap
/// between tracks is left.
struct Player {
    tracks: Vec<Track>,
    index: usize,
    frame: u64,
    gap_frames: u64,
    gap_left: u64,
    buf: Vec<f32>,
}

impl Player {
    fn new(tracks: Vec<Track>, gap_ms: u64) -> Self {
        let gap_frames = gap_ms * SAMPLE_RATE as u64 / 1000 / FRAME_SAMPLES as u64;
        Self {
            tracks,
            index: 0,
            frame: 0,
            gap_frames,
            gap_left: 0,
            buf: Vec::with_capacity(FRAME_SAMPLES * 2),
        }
    }

    fn current(&self) -> &Track {
        &self.tracks[self.index]
    }

    fn position_ms(&self) -> u64 {
        self.frame * 1000 * FRAME_SAMPLES as u64 / SAMPLE_RATE as u64
    }

    fn up_next(&self, n: usize) -> Vec<TrackInfo> {
        (1..=n)
            .map(|k| {
                self.tracks[(self.index + k) % self.tracks.len()]
                    .info
                    .clone()
            })
            .collect()
    }

    fn status(&self, room: &str) -> Status {
        Status {
            room: room.to_string(),
            track: self.current().info.clone(),
            position_ms: self.position_ms(),
            up_next: self.up_next(3),
        }
    }

    fn skip(&mut self) {
        self.index = (self.index + 1) % self.tracks.len();
        self.frame = 0;
        self.gap_left = 0;
    }

    /// Advance one tick. `Some(pcm)` to send, `None` during the inter-track gap; `changed`
    /// tells the caller a new track just started.
    fn tick(&mut self) -> (Option<&[f32]>, bool) {
        if self.gap_left > 0 {
            self.gap_left -= 1;
            if self.gap_left == 0 {
                self.index = (self.index + 1) % self.tracks.len();
                self.frame = 0;
                return (None, true);
            }
            return (None, false);
        }
        if self.tracks[self.index].frame(self.frame, &mut self.buf) {
            self.frame += 1;
            return (Some(&self.buf), false);
        }
        if self.gap_frames == 0 {
            self.index = (self.index + 1) % self.tracks.len();
            self.frame = 0;
            return (None, true);
        }
        self.gap_left = self.gap_frames;
        (None, false)
    }
}

fn encoder_for(track: &Track) -> EncoderSettings {
    EncoderSettings {
        channels: track.channels(),
        signal: match track.info.kind {
            Kind::Speech => OpusSignal::Voice,
            _ => OpusSignal::Music,
        },
        complexity: 10,
        ..EncoderSettings::default()
    }
}

fn describe(info: &TrackInfo) -> String {
    let kind = match info.kind {
        Kind::Music => "music",
        Kind::Speech => "speech",
        Kind::Ambience => "ambience",
        Kind::Signal => "test signal",
    };
    let mut s = format!("▶ {}", info.title);
    if let Some(artist) = &info.artist {
        s.push_str(&format!(" — {artist}"));
    }
    s.push_str(&format!(
        " · {kind} · {} · {}:{:02}",
        if info.stereo { "stereo" } else { "mono" },
        info.duration_ms / 60_000,
        info.duration_ms % 60_000 / 1000
    ));
    if let Some(license) = &info.license {
        s.push_str(&format!(" · {license}"));
    }
    s
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,aurix_client=warn".into()),
        )
        .init();
    let args = Args::parse();
    let tracks = playlist::load(args.assets.as_deref())?;
    info!(tracks = tracks.len(), "playlist loaded");
    for t in &tracks {
        info!("  {}", describe(&t.info));
    }
    let backend = Backend {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?,
        base: args.rooms_url.trim_end_matches('/').to_string(),
        bot_token: args.bot_token.clone(),
    };
    let mut player = Player::new(tracks, args.gap_ms);

    let mut delay = Duration::from_secs(1);
    loop {
        let session = loop {
            match backend.session(args.room.as_deref(), &args.name).await {
                Ok(s) => break s,
                Err(e) => {
                    warn!("{e:#}; retrying in {delay:?}");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        };
        delay = Duration::from_secs(1);
        info!(room = %session.room, user = %session.user_id.0, "session minted");
        let ws_url = args
            .ws_url
            .clone()
            .unwrap_or_else(|| session.ws_url.clone());
        match run(&args, &backend, &mut player, session, &ws_url).await {
            Ok(()) => return Ok(()),
            Err(e) => warn!("session ended: {e:#}; starting over"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// One control session from connect to terminal disconnect; the playlist position survives.
async fn run(
    args: &Args,
    backend: &Backend,
    player: &mut Player,
    mut session: Session,
    ws_url: &str,
) -> Result<()> {
    let mut cfg = ClientConfig::new(ws_url, session.token.clone());
    cfg.dsp = DspConfig::BYPASS;
    cfg.vad_gate = false;
    cfg.encoder = encoder_for(player.current());
    cfg.reconnect.max_attempts = u32::MAX;
    cfg.reconnect.max_delay = Duration::from_secs(5);
    let client = Client::new(cfg)?;
    client.connect()?;
    let channel = session.channel_id;
    let room = session.room.clone();

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut joined = false;
    let mut announce = true;
    let mut last_status = Instant::now() - STATUS_INTERVAL;
    let mut refresh_at = refresh_time(&session);
    let mut pending_session: Option<tokio::task::JoinHandle<Result<Session>>> = None;
    let http = backend.http.clone();
    let base = backend.base.clone();
    let bot_token = backend.bot_token.clone();

    loop {
        ticker.tick().await;

        while let Some(ev) = client.poll_event() {
            match &ev {
                Event::SessionReady(info) => {
                    info!(session = %info.session_id.0, resumed = info.resumed, "session ready");
                    if !info.resumed {
                        joined = false;
                        client.join_channel(channel, None)?;
                    }
                }
                Event::ChannelJoined {
                    channel_id,
                    participants,
                    ..
                } if *channel_id == channel => {
                    info!(participants = participants.len(), "joined room");
                    joined = true;
                    announce = true;
                }
                Event::ChannelLeft { channel_id } if *channel_id == channel => joined = false,
                Event::MediaPathChanged { path, reason } => {
                    info!(?path, reason = %reason, "media path");
                }
                Event::ChatMessage {
                    request_id: None,
                    message,
                } if message.from_user_id != session.user_id
                    && message.channel_id == Some(channel) =>
                {
                    let reply = match message.text.trim() {
                        "!np" => Some(describe(&player.current().info)),
                        "!next" | "!skip" => {
                            player.skip();
                            client.set_encoder_settings(encoder_for(player.current()))?;
                            announce = true;
                            last_status = Instant::now() - STATUS_INTERVAL;
                            None
                        }
                        "!list" => Some(
                            player
                                .tracks
                                .iter()
                                .enumerate()
                                .map(|(i, t)| {
                                    format!(
                                        "{}{}. {}",
                                        if i == player.index { "▶ " } else { "" },
                                        i + 1,
                                        t.info.title
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        _ => None,
                    };
                    if let Some(text) = reply {
                        client.send_chat(channel, &text, None)?;
                    }
                }
                Event::Recovering { attempt, cause, .. } => {
                    warn!(attempt, cause = %cause, "recovering");
                }
                Event::Recovered { resumed, migrated } => {
                    info!(resumed, migrated, "recovered");
                }
                Event::Kicked { .. }
                | Event::Disconnected { .. }
                | Event::FailedToRecover { .. } => {
                    return Err(anyhow!("{ev:?}"));
                }
                Event::RequestFailed { code, message, .. }
                | Event::ServerError { code, message } => {
                    warn!(code = %code, message = %message, "server refused a request");
                }
                _ => {}
            }
        }

        if pending_session.is_none() && Instant::now() >= refresh_at {
            let (http, base, bot_token, room, name) = (
                http.clone(),
                base.clone(),
                bot_token.clone(),
                args.room.clone(),
                args.name.clone(),
            );
            pending_session = Some(tokio::spawn(async move {
                Backend {
                    http,
                    base,
                    bot_token,
                }
                .session(room.as_deref(), &name)
                .await
            }));
        }
        if pending_session.as_ref().is_some_and(|h| h.is_finished()) {
            let handle = pending_session.take().expect("checked");
            match handle.await {
                Ok(Ok(fresh)) => {
                    client.set_token(&fresh.token)?;
                    refresh_at = refresh_time(&fresh);
                    session = fresh;
                    info!("token refreshed");
                }
                Ok(Err(e)) => {
                    warn!("token refresh failed: {e:#}");
                    refresh_at = Instant::now() + Duration::from_secs(30);
                }
                Err(e) => warn!("token refresh task: {e}"),
            }
        }

        let live = joined && client.state() == ConnectionState::MediaBound;
        if !live {
            continue;
        }
        if announce {
            announce = false;
            client.send_chat(channel, &describe(&player.current().info), None)?;
        }
        let channels = player.current().channels();
        let (pcm, changed) = player.tick();
        if let Some(pcm) = pcm {
            client.push_capture_f32(pcm, SAMPLE_RATE, channels);
        }
        if changed {
            client.set_encoder_settings(encoder_for(player.current()))?;
            announce = true;
            last_status = Instant::now() - STATUS_INTERVAL;
        }
        if last_status.elapsed() >= STATUS_INTERVAL {
            last_status = Instant::now();
            let status = player.status(&room);
            let backend = Backend {
                http: http.clone(),
                base: base.clone(),
                bot_token: bot_token.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) = backend.status(&status).await {
                    warn!("{e:#}");
                }
            });
        }
    }
}

fn refresh_time(session: &Session) -> Instant {
    let ttl = (session.expires_at - chrono::Utc::now())
        .to_std()
        .unwrap_or_default();
    Instant::now() + ttl.mul_f32(0.66).max(Duration::from_secs(30))
}
