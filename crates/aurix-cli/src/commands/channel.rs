use anyhow::bail;
use clap::{Args as ClapArgs, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};

use super::{obj, query, s, seg, Ctx};
use crate::http::Auth;
use crate::output::json_arg;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List channels of the app.
    List {
        #[arg(long)]
        page: Option<i64>,
        #[arg(long)]
        per_page: Option<i64>,
        /// Only channels with participants.
        #[arg(long)]
        active_only: bool,
    },
    Get {
        channel_id: String,
    },
    /// Create a channel (`--config` is a full ChannelConfig JSON; `--type` is a shortcut).
    Create {
        #[arg(long)]
        name: String,
        #[arg(long = "type", value_parser = ["positional", "team", "command", "whisper", "echo"], conflicts_with = "config")]
        channel_type: Option<String>,
        #[arg(long)]
        max_participants: Option<i64>,
        #[arg(long, value_name = "JSON|@file|-")]
        config: Option<String>,
    },
    /// Delete a channel and disconnect its participants.
    Delete {
        channel_id: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Current participants (roster).
    Participants {
        channel_id: String,
    },
    /// Replace the channel configuration (`PUT /v1/channels/{id}/config`).
    SetConfig {
        channel_id: String,
        #[arg(long, value_name = "JSON|@file|-")]
        config: String,
    },
    /// Chat history (newest first, cursor pagination).
    Messages {
        channel_id: String,
        #[arg(long)]
        before: Option<String>,
        #[arg(long)]
        after: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Post a system message into the channel chat.
    Send {
        channel_id: String,
        #[arg(long)]
        text: String,
        /// Sender name shown to players (default "System").
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long, value_name = "JSON")]
        metadata: Option<String>,
    },
    /// Text-to-speech announcement into the channel (needs a configured TTS provider).
    Announce {
        channel_id: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        voice: Option<String>,
    },
    /// Read markers of a channel conversation.
    ReadMarkers {
        channel_id: String,
    },
    /// Recording/streaming taps of a channel.
    Streams {
        channel_id: String,
    },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<()> {
    let api = &ctx.api;
    match args.cmd {
        Cmd::List {
            page,
            per_page,
            active_only,
        } => {
            let q = query(vec![
                ("page", page.map(|v| v.to_string())),
                ("per_page", per_page.map(|v| v.to_string())),
                ("active_only", active_only.then(|| "true".to_string())),
            ]);
            ctx.print(&api.get("/v1/channels", &q, Auth::ApiKey).await?)
        }
        Cmd::Get { channel_id } => ctx.print(
            &api.get(
                &format!("/v1/channels/{}", seg(&channel_id)),
                &[],
                Auth::ApiKey,
            )
            .await?,
        ),
        Cmd::Create {
            name,
            channel_type,
            max_participants,
            config,
        } => {
            let mut cfg = match config {
                Some(c) => json_arg(&c)?,
                None => json!({}),
            };
            if !cfg.is_object() {
                bail!("--config must be a JSON object");
            }
            if let Some(t) = channel_type {
                cfg["channel_type"] = Value::String(t);
            }
            if let Some(m) = max_participants {
                cfg["max_participants"] = Value::from(m);
            }
            let body = obj(vec![
                ("name", Some(Value::String(name))),
                (
                    "config",
                    cfg.as_object()
                        .filter(|o| !o.is_empty())
                        .map(|_| cfg.clone()),
                ),
            ]);
            ctx.print(&api.post("/v1/channels", body, Auth::ApiKey).await?)
        }
        Cmd::Delete { channel_id, yes } => {
            if !yes {
                bail!("deleting a channel disconnects everyone in it; re-run with --yes");
            }
            ctx.print(
                &api.delete(
                    &format!("/v1/channels/{}", seg(&channel_id)),
                    &[],
                    Auth::ApiKey,
                )
                .await?,
            )
        }
        Cmd::Participants { channel_id } => ctx.print(
            &api.get(
                &format!("/v1/channels/{}/participants", seg(&channel_id)),
                &[],
                Auth::ApiKey,
            )
            .await?,
        ),
        Cmd::SetConfig { channel_id, config } => {
            let cfg = json_arg(&config)?;
            if !cfg.is_object() {
                bail!("--config must be a JSON object (ChannelConfig)");
            }
            ctx.print(
                &api.json(
                    Method::PUT,
                    &format!("/v1/channels/{}/config", seg(&channel_id)),
                    &[],
                    Some(&cfg),
                    Auth::ApiKey,
                )
                .await?,
            )
        }
        Cmd::Messages {
            channel_id,
            before,
            after,
            limit,
        } => {
            let q = query(vec![
                ("before", before),
                ("after", after),
                ("limit", limit.map(|v| v.to_string())),
            ]);
            ctx.print(
                &api.get(
                    &format!("/v1/channels/{}/messages", seg(&channel_id)),
                    &q,
                    Auth::ApiKey,
                )
                .await?,
            )
        }
        Cmd::Send {
            channel_id,
            text,
            display_name,
            metadata,
        } => {
            let body = obj(vec![
                ("text", Some(Value::String(text))),
                ("display_name", s(&display_name)),
                ("metadata", metadata.as_deref().map(json_arg).transpose()?),
            ]);
            ctx.print(
                &api.post(
                    &format!("/v1/channels/{}/messages", seg(&channel_id)),
                    body,
                    Auth::ApiKey,
                )
                .await?,
            )
        }
        Cmd::Announce {
            channel_id,
            text,
            voice,
        } => {
            let body = obj(vec![
                ("text", Some(Value::String(text))),
                ("voice", s(&voice)),
            ]);
            ctx.print(
                &api.post(
                    &format!("/v1/channels/{}/tts", seg(&channel_id)),
                    body,
                    Auth::ApiKey,
                )
                .await?,
            )
        }
        Cmd::ReadMarkers { channel_id } => ctx.print(
            &api.get(
                &format!("/v1/channels/{}/read-markers", seg(&channel_id)),
                &[],
                Auth::ApiKey,
            )
            .await?,
        ),
        Cmd::Streams { channel_id } => ctx.print(
            &api.get(
                &format!("/v1/channels/{}/audio/streams", seg(&channel_id)),
                &[],
                Auth::ApiKey,
            )
            .await?,
        ),
    }
}
